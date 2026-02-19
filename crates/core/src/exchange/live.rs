use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use crate::config::Config;
use crate::exchange::messages::*;
use crate::exchange::ws::*;
use crate::traits::{DeadManSwitch, ExchangeClient, OrderManager};
use crate::types::*;

/// A serialized WS write request sent through a channel.
#[derive(Debug)]
enum WsRequest {
    Json(String),
}

/// Live exchange adapter: uses a single private WS connection (via proxy)
/// for both sending orders and receiving executions/responses.
/// The private WS automatically reconnects on disconnect.
pub struct LiveExchange {
    config: Config,
    /// Channel to send serialized WS messages to the write loop.
    ws_write_tx: mpsc::Sender<WsRequest>,
    req_counter: Arc<Mutex<u64>>,
    token: String,
    /// Maps req_id → cl_ord_id so we can route rejection responses.
    req_id_map: Arc<Mutex<HashMap<u64, String>>>,
    /// Shared writer that gets swapped on reconnect (kept alive via Arc clones in tasks).
    #[allow(dead_code)]
    shared_writer: Arc<Mutex<Option<WsWriter>>>,
    /// Internal task handles for clean shutdown.
    reconnect_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    write_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl LiveExchange {
    /// Connect to exchange via proxy: opens one private WS connection, splits it into
    /// a write loop (for orders/DMS) and a read loop (for executions/responses).
    /// The read loop automatically reconnects on disconnect with exponential backoff.
    ///
    /// In proxy mode, connects to the proxy's WS endpoint and uses a placeholder
    /// token (the proxy injects the real token on the upstream side).
    pub async fn connect(
        config: Config,
        event_tx: mpsc::Sender<EngineEvent>,
        exchange: Arc<dyn ExchangeClient>,
    ) -> Result<Self> {
        // Get auth token (proxy returns placeholder, injected upstream)
        tracing::info!("Fetching WS auth token...");
        let token = exchange.get_ws_token().await?;

        // Build WS URL
        let base = config.exchange.proxy_url.trim_end_matches('/');
        let ws_base = base.replacen("http://", "ws://", 1).replacen("https://", "wss://", 1);
        let ws_url = format!("{}/ws/private", ws_base);
        tracing::info!(url = ws_url, "Connecting private WS...");

        // Initial connection + subscribe
        let (writer, reader) = try_private_connect(&ws_url, &config.exchange.proxy_token, &token).await?;

        // Shared state
        let shared_writer: Arc<Mutex<Option<WsWriter>>> = Arc::new(Mutex::new(Some(writer)));
        let req_id_map = Arc::new(Mutex::new(HashMap::<u64, String>::new()));

        // Spawn reconnect loop (owns the read side lifecycle)
        let reconnect_writer = shared_writer.clone();
        let reconnect_token = token.clone();
        let proxy_token = config.exchange.proxy_token.clone();
        let req_id_map_read = req_id_map.clone();

        let reconnect_handle = tokio::spawn(async move {
            // Run initial read loop
            run_private_read_loop(reader, &event_tx, &req_id_map_read).await;

            // Reconnect loop: backoff → connect → subscribe → read → repeat
            loop {
                if event_tx.is_closed() {
                    tracing::info!("Engine event channel closed, stopping private WS reconnect");
                    break;
                }

                // Clear writer so write loop stops sending to dead connection
                *reconnect_writer.lock().await = None;

                // Backoff and reconnect
                let mut backoff_secs = 1u64;
                let (new_writer, new_reader) = loop {
                    tracing::warn!(backoff_secs, "Private WS disconnected, reconnecting...");
                    tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;

                    if event_tx.is_closed() {
                        tracing::info!("Engine event channel closed during reconnect backoff");
                        return;
                    }

                    match try_private_connect(&ws_url, &proxy_token, &reconnect_token).await {
                        Ok(pair) => break pair,
                        Err(e) => {
                            tracing::error!(error = %e, "Private WS reconnect failed");
                            backoff_secs = (backoff_secs * 2).min(30);
                        }
                    }
                };

                // Swap new writer in for the write loop
                *reconnect_writer.lock().await = Some(new_writer);
                tracing::info!("Private WS reconnected successfully");

                // Run read loop until next disconnect
                run_private_read_loop(new_reader, &event_tx, &req_id_map_read).await;
            }
        });

        // Write loop: reads from channel, sends through shared writer
        let (ws_write_tx, mut ws_write_rx) = mpsc::channel::<WsRequest>(256);
        let write_shared = shared_writer.clone();
        let write_handle = tokio::spawn(async move {
            while let Some(req) = ws_write_rx.recv().await {
                match req {
                    WsRequest::Json(text) => {
                        let mut guard = write_shared.lock().await;
                        if let Some(ref mut w) = *guard {
                            if let Err(e) = w.send_raw(&text).await {
                                tracing::error!(error = %e, "WS write error (connection lost)");
                                *guard = None;
                            }
                        } else {
                            tracing::warn!("WS write dropped: no active connection");
                        }
                    }
                }
            }
        });

        Ok(Self {
            config,
            ws_write_tx,
            req_counter: Arc::new(Mutex::new(0u64)),
            token,
            req_id_map,
            shared_writer,
            reconnect_handle: Mutex::new(Some(reconnect_handle)),
            write_handle: Mutex::new(Some(write_handle)),
        })
    }

    /// Abort internal read/write tasks for clean shutdown.
    pub async fn abort_tasks(&self) {
        if let Some(h) = self.reconnect_handle.lock().await.take() {
            h.abort();
        }
        if let Some(h) = self.write_handle.lock().await.take() {
            h.abort();
        }
    }

    async fn next_req_id(&self) -> u64 {
        let mut c = self.req_counter.lock().await;
        *c += 1;
        *c
    }

    async fn send_json(&self, msg: &impl serde::Serialize) -> Result<()> {
        let text = serde_json::to_string(msg)?;
        tracing::debug!(msg = &text, "WS send");
        self.ws_write_tx
            .send(WsRequest::Json(text))
            .await
            .map_err(|_| anyhow::anyhow!("WS write channel closed"))?;
        Ok(())
    }

    /// Spawn the public WS feed as a background task that sends events into the channel.
    /// In proxy mode, connects to the proxy's public WS endpoint instead of the
    /// default Kraken endpoint, so book data comes from the same source as orders.
    ///
    /// Accepts an optional `sub_rx` channel for dynamic pair subscriptions. When new
    /// pairs arrive on this channel, the book feed subscribes to them on the existing
    /// WS connection without reconnecting.
    pub async fn spawn_book_feed(
        &self,
        tx: mpsc::Sender<EngineEvent>,
        pairs: &[String],
        sub_rx: Option<mpsc::Receiver<Vec<String>>>,
    ) -> Result<tokio::task::JoinHandle<()>> {
        let base = self.config.exchange.proxy_url.trim_end_matches('/');
        let ws_base = base
            .replacen("http://", "ws://", 1)
            .replacen("https://", "wss://", 1);
        let url = format!("{}/ws/public", ws_base);
        tracing::info!(url, ?pairs, "Connecting public WS for book data");
        let pairs = pairs.to_vec();
        let depth = self.config.exchange.book_depth;
        let proxy_token = self.config.exchange.proxy_token.clone();

        let handle = tokio::spawn(async move {
            if let Err(e) = run_book_feed_dynamic(&url, &proxy_token, &pairs, depth, &tx, sub_rx).await {
                tracing::error!(error = %e, "Book feed error");
            }
        });
        Ok(handle)
    }

    /// Spawn a tick timer that sends periodic Tick events.
    pub fn spawn_ticker(
        &self,
        tx: mpsc::Sender<EngineEvent>,
        interval_secs: u64,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
            loop {
                interval.tick().await;
                if tx
                    .send(EngineEvent::Tick {
                        timestamp: Utc::now(),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        })
    }
}

#[async_trait]
impl OrderManager for LiveExchange {
    async fn place_order(&self, request: &OrderRequest) -> Result<()> {
        let req_id = self.next_req_id().await;
        self.req_id_map.lock().await.insert(req_id, request.cl_ord_id.clone());
        let order_type = if request.market { "market" } else { "limit" };
        self.send_json(&AddOrderMsg {
            method: "add_order",
            params: AddOrderParams {
                order_type,
                side: request.side.to_string(),
                symbol: request.symbol.clone(),
                limit_price: request.price,
                order_qty: request.qty,
                post_only: if request.market { false } else { request.post_only },
                time_in_force: "gtc",
                cl_ord_id: request.cl_ord_id.clone(),
                token: self.token.clone(),
            },
            req_id,
            _priority: if request.urgent { Some("urgent".into()) } else { None },
        })
        .await
    }

    async fn amend_order(
        &self,
        cl_ord_id: &str,
        new_price: Option<Decimal>,
        new_qty: Option<Decimal>,
    ) -> Result<()> {
        let req_id = self.next_req_id().await;
        self.send_json(&AmendOrderMsg {
            method: "amend_order",
            params: AmendOrderParams {
                cl_ord_id: cl_ord_id.to_string(),
                limit_price: new_price,
                order_qty: new_qty,
                post_only: true,
                token: self.token.clone(),
            },
            req_id,
            _priority: None,
        })
        .await
    }

    async fn cancel_orders(&self, cl_ord_ids: &[String]) -> Result<()> {
        let req_id = self.next_req_id().await;
        self.send_json(&CancelOrderMsg {
            method: "cancel_order",
            params: CancelOrderParams {
                cl_ord_id: cl_ord_ids.to_vec(),
                token: self.token.clone(),
            },
            req_id,
            _priority: None,
        })
        .await
    }

    async fn cancel_all(&self) -> Result<()> {
        let req_id = self.next_req_id().await;
        self.send_json(&CancelAllMsg {
            method: "cancel_all",
            params: CancelAllParams {
                token: self.token.clone(),
            },
            req_id,
        })
        .await
    }
}

#[async_trait]
impl DeadManSwitch for LiveExchange {
    async fn refresh(&self, timeout_secs: u64) -> Result<()> {
        let req_id = self.next_req_id().await;
        self.send_json(&DmsMsg {
            method: "cancel_all_orders_after",
            params: DmsParams {
                timeout: timeout_secs,
                token: self.token.clone(),
            },
            req_id,
        })
        .await
    }

    async fn disable(&self) -> Result<()> {
        self.refresh(0).await
    }
}

// --- Private WS helpers ---

/// Connect to the private WS, subscribe to executions, and return (writer, reader).
async fn try_private_connect(
    ws_url: &str,
    proxy_token: &str,
    exec_token: &str,
) -> Result<(WsWriter, WsReader)> {
    let ws = WsConnection::connect_with_token(ws_url, proxy_token).await?;
    let (mut writer, reader) = ws.into_split();
    let sub_msg = serde_json::to_string(&SubscribeExecMsg {
        method: "subscribe",
        params: SubscribeExecParams {
            channel: "executions",
            snap_orders: true,
            snap_trades: false,
            ratecounter: true,
            token: exec_token.to_string(),
        },
    })?;
    writer.send_raw(&sub_msg).await?;
    Ok((writer, reader))
}

/// Read loop for private WS: routes executions and order responses into the engine.
/// Returns when the WS connection closes or the event channel is dropped.
async fn run_private_read_loop(
    mut reader: WsReader,
    event_tx: &mpsc::Sender<EngineEvent>,
    req_id_map: &Arc<Mutex<HashMap<u64, String>>>,
) {
    while let Some(raw) = reader.recv().await {
        let msg = parse_ws_message(&raw);
        let event = match msg {
            WsMessage::Execution(report) => {
                match report.exec_type.as_str() {
                    "trade" => {
                        let side = if report.side == "buy" {
                            OrderSide::Buy
                        } else {
                            OrderSide::Sell
                        };
                        tracing::info!(
                            symbol = report.symbol,
                            side = report.side,
                            price = %report.last_price,
                            qty = %report.last_qty,
                            order_status = report.order_status,
                            "Execution report: trade"
                        );
                        Some(EngineEvent::Fill(Fill {
                            order_id: report.order_id,
                            cl_ord_id: report.cl_ord_id,
                            symbol: report.symbol,
                            side,
                            price: report.last_price,
                            qty: report.last_qty,
                            fee: report.fee,
                            is_maker: report.is_maker,
                            is_fully_filled: report.order_status == "filled",
                            timestamp: report.timestamp,
                        }))
                    }
                    "filled" => {
                        tracing::info!(
                            cl_ord_id = report.cl_ord_id,
                            symbol = report.symbol,
                            "Order fully filled (status update)"
                        );
                        None
                    }
                    "new" | "pending_new" => {
                        tracing::info!(
                            cl_ord_id = report.cl_ord_id,
                            order_id = report.order_id,
                            exec_type = report.exec_type,
                            "Order acknowledged by exchange"
                        );
                        Some(EngineEvent::OrderAcknowledged {
                            cl_ord_id: report.cl_ord_id,
                            order_id: report.order_id,
                        })
                    }
                    "canceled" | "expired" => {
                        tracing::info!(
                            cl_ord_id = report.cl_ord_id,
                            symbol = report.symbol,
                            exec_type = report.exec_type,
                            cancel_reason = report.cancel_reason.as_deref().unwrap_or("none"),
                            "Order cancelled/expired"
                        );
                        Some(EngineEvent::OrderCancelled {
                            cl_ord_id: report.cl_ord_id,
                            symbol: report.symbol,
                            reason: report.cancel_reason,
                        })
                    }
                    other => {
                        tracing::debug!(
                            exec_type = other,
                            cl_ord_id = report.cl_ord_id,
                            "Unhandled exec_type"
                        );
                        None
                    }
                }
            }
            WsMessage::OrderResponse {
                success,
                method,
                error,
                cl_ord_id,
                req_id,
                ..
            } => {
                if !success {
                    let resolved_id = match cl_ord_id {
                        Some(id) => Some(id),
                        None => req_id_map.lock().await.remove(&req_id),
                    };
                    let err_msg = error.as_deref().unwrap_or("unknown");
                    tracing::warn!(
                        method,
                        req_id,
                        cl_ord_id = resolved_id.as_deref().unwrap_or("?"),
                        error = err_msg,
                        "Order rejected by exchange"
                    );
                    resolved_id.map(|id| EngineEvent::OrderRejected {
                        cl_ord_id: id,
                        symbol: String::new(),
                        reason: err_msg.to_string(),
                    })
                } else {
                    req_id_map.lock().await.remove(&req_id);
                    tracing::debug!(method, "Order response OK");
                    None
                }
            }
            WsMessage::SubscribeConfirmed { channel } => {
                tracing::info!(channel, "Private subscription confirmed");
                None
            }
            WsMessage::Heartbeat | WsMessage::Pong => None,
            WsMessage::ExecutionSnapshot(_) => {
                tracing::info!("Ignoring execution snapshot (positions loaded from state)");
                None
            }
            _ => None,
        };
        if let Some(e) = event {
            if event_tx.send(e).await.is_err() {
                break;
            }
        }
    }
    tracing::warn!("Private WS read loop ended");
}

// --- Book feed (public WS, separate connection) ---

async fn run_book_feed_dynamic(
    url: &str,
    proxy_token: &str,
    pairs: &[String],
    depth: u32,
    tx: &mpsc::Sender<EngineEvent>,
    sub_rx: Option<mpsc::Receiver<Vec<String>>>,
) -> Result<()> {
    let ws = WsConnection::connect_with_token(url, proxy_token).await?;
    tracing::info!("Public WS connected");
    let (mut writer, mut reader) = ws.into_split();

    // Subscribe to initial pairs
    if !pairs.is_empty() {
        let sub_msg = serde_json::to_string(&SubscribeBookMsg {
            method: "subscribe",
            params: SubscribeBookParams {
                channel: "book",
                symbol: pairs.to_vec(),
                depth,
            },
        })?;
        writer.send_raw(&sub_msg).await?;
    }

    let mut sub_rx = sub_rx;
    loop {
        // If we have a subscription channel, select on both WS and channel.
        // Otherwise, just read WS.
        let ws_msg = if let Some(ref mut rx) = sub_rx {
            tokio::select! {
                raw = reader.recv() => {
                    match raw {
                        Some(text) => Some(text),
                        None => break, // WS closed
                    }
                }
                new_pairs = rx.recv() => {
                    if let Some(new_pairs) = new_pairs {
                        if !new_pairs.is_empty() {
                            tracing::info!(?new_pairs, "Subscribing to new pairs dynamically");
                            let sub_msg = serde_json::to_string(&SubscribeBookMsg {
                                method: "subscribe",
                                params: SubscribeBookParams {
                                    channel: "book",
                                    symbol: new_pairs,
                                    depth,
                                },
                            })?;
                            writer.send_raw(&sub_msg).await?;
                        }
                    }
                    continue; // Go back to select
                }
            }
        } else {
            match reader.recv().await {
                Some(text) => Some(text),
                None => break,
            }
        };

        if let Some(raw) = ws_msg {
            let msg = parse_ws_message(&raw);
            match msg {
                WsMessage::BookSnapshot {
                    symbol,
                    bids,
                    asks,
                } => {
                    tx.send(EngineEvent::BookSnapshot {
                        symbol,
                        bids,
                        asks,
                        timestamp: Utc::now(),
                    })
                    .await?;
                }
                WsMessage::BookUpdate {
                    symbol,
                    bid_updates,
                    ask_updates,
                } => {
                    tx.send(EngineEvent::BookUpdate {
                        symbol,
                        bid_updates,
                        ask_updates,
                        timestamp: Utc::now(),
                    })
                    .await?;
                }
                WsMessage::SubscribeConfirmed { channel } => {
                    tracing::info!(channel, "Subscription confirmed");
                }
                WsMessage::Heartbeat | WsMessage::Pong => {}
                WsMessage::Unknown(raw) => {
                    tracing::debug!(raw, "Unknown public message");
                }
                _ => {}
            }
        }
    }
    Ok(())
}
