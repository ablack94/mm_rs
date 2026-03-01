use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use rust_decimal::Decimal;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use crate::config::Config;
use crate::exchange::messages::*;
use crate::exchange::ws::*;
use crate::traits::{DeadManSwitch, ExchangeClient, OrderManager};
use crate::types::*;
use trading_primitives::book::LevelUpdate;

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
    pub async fn connect(
        config: Config,
        event_tx: mpsc::Sender<EngineEvent>,
        exchange: Arc<dyn ExchangeClient>,
    ) -> Result<Self> {
        // Get auth token (proxy returns placeholder, injected upstream)
        tracing::info!("Fetching WS auth token...");
        let _token = exchange.get_ws_token().await?;

        // Build WS URL
        let base = config.exchange.proxy_url.trim_end_matches('/');
        let ws_base = base.replacen("http://", "ws://", 1).replacen("https://", "wss://", 1);
        let ws_url = format!("{}/ws/private", ws_base);
        tracing::info!(url = ws_url, "Connecting private WS...");

        // Initial connection + subscribe
        let (writer, reader) = try_private_connect(&ws_url, &config.exchange.proxy_token).await?;

        // Shared state
        let shared_writer: Arc<Mutex<Option<WsWriter>>> = Arc::new(Mutex::new(Some(writer)));

        // Spawn reconnect loop (owns the read side lifecycle)
        let reconnect_writer = shared_writer.clone();
        let proxy_token = config.exchange.proxy_token.clone();

        let reconnect_handle = tokio::spawn(async move {
            // Run initial read loop
            run_private_read_loop(reader, &event_tx).await;

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

                    match try_private_connect(&ws_url, &proxy_token).await {
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
                run_private_read_loop(new_reader, &event_tx).await;
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

    async fn send_command(&self, cmd: &ProxyCommand) -> Result<()> {
        let text = serde_json::to_string(cmd)?;
        tracing::debug!(msg = &text, "WS send");
        self.ws_write_tx
            .send(WsRequest::Json(text))
            .await
            .map_err(|_| anyhow::anyhow!("WS write channel closed"))?;
        Ok(())
    }

    /// Spawn the public WS feed as a background task that sends events into the channel.
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
        self.send_command(&ProxyCommand::PlaceOrder {
            req_id,
            symbol: request.pair.to_string(),
            side: request.side.to_string(),
            order_type: if request.market { "market".into() } else { "limit".into() },
            price: request.price,
            qty: request.qty,
            cl_ord_id: request.cl_ord_id.clone(),
            post_only: if request.market { false } else { request.post_only },
            priority: if request.urgent { OrderPriority::Urgent } else { OrderPriority::Normal },
            market: request.market,
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
        self.send_command(&ProxyCommand::AmendOrder {
            req_id,
            cl_ord_id: cl_ord_id.to_string(),
            price: new_price,
            qty: new_qty,
        })
        .await
    }

    async fn cancel_orders(&self, cl_ord_ids: &[String]) -> Result<()> {
        let req_id = self.next_req_id().await;
        self.send_command(&ProxyCommand::CancelOrders {
            req_id,
            cl_ord_ids: cl_ord_ids.to_vec(),
        })
        .await
    }

    async fn cancel_all(&self) -> Result<()> {
        let req_id = self.next_req_id().await;
        self.send_command(&ProxyCommand::CancelAll { req_id })
            .await
    }
}

#[async_trait]
impl DeadManSwitch for LiveExchange {
    async fn refresh(&self, timeout_secs: u64) -> Result<()> {
        let req_id = self.next_req_id().await;
        self.send_command(&ProxyCommand::SetDms {
            req_id,
            timeout_secs,
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
) -> Result<(WsWriter, WsReader)> {
    let ws = WsConnection::connect_with_token(ws_url, proxy_token).await?;
    let (mut writer, reader) = ws.into_split();
    let sub_cmd = ProxyCommand::Subscribe {
        channel: "executions".into(),
        symbols: vec![],
        depth: None,
    };
    let sub_msg = serde_json::to_string(&sub_cmd)?;
    writer.send_raw(&sub_msg).await?;
    Ok((writer, reader))
}

/// Read loop for private WS: routes events into the engine.
/// Returns when the WS connection closes or the event channel is dropped.
async fn run_private_read_loop(
    mut reader: WsReader,
    event_tx: &mpsc::Sender<EngineEvent>,
) {
    while let Some(raw) = reader.recv().await {
        let proxy_event = match parse_proxy_event(&raw) {
            Ok(e) => e,
            Err(_) => {
                tracing::debug!(raw, "Unknown private WS message");
                continue;
            }
        };

        let event = match proxy_event {
            ProxyEvent::Fill {
                order_id,
                cl_ord_id,
                symbol,
                side,
                qty,
                price,
                fee,
                is_maker,
                timestamp,
                is_fully_filled,
            } => {
                let order_side = if side == "buy" {
                    OrderSide::Buy
                } else {
                    OrderSide::Sell
                };
                let ts = timestamp
                    .parse()
                    .unwrap_or_else(|_| Utc::now());
                tracing::info!(
                    pair = symbol,
                    side,
                    price = %price,
                    qty = %qty,
                    "Fill"
                );
                Some(EngineEvent::Fill(Fill {
                    order_id,
                    cl_ord_id,
                    pair: trading_primitives::Ticker::from(symbol.as_str()),
                    side: order_side,
                    price,
                    qty,
                    fee,
                    is_maker,
                    is_fully_filled,
                    timestamp: ts,
                }))
            }
            ProxyEvent::OrderAccepted {
                cl_ord_id,
                order_id,
                ..
            } => {
                tracing::info!(cl_ord_id, order_id, "Order accepted");
                Some(EngineEvent::OrderAcknowledged {
                    cl_ord_id,
                    order_id,
                })
            }
            ProxyEvent::OrderCancelled {
                cl_ord_id,
                reason,
                symbol,
            } => {
                tracing::info!(cl_ord_id, reason = reason.as_deref().unwrap_or("none"), "Order cancelled");
                let pair = symbol
                    .map(|s| trading_primitives::Ticker::from(s.as_str()))
                    .unwrap_or_else(|| trading_primitives::Ticker::from("UNKNOWN/UNKNOWN"));
                Some(EngineEvent::OrderCancelled {
                    cl_ord_id,
                    pair,
                    reason,
                })
            }
            ProxyEvent::OrderRejected {
                cl_ord_id,
                error,
                symbol,
                ..
            } => {
                tracing::warn!(cl_ord_id, error, "Order rejected");
                let pair = symbol
                    .map(|s| trading_primitives::Ticker::from(s.as_str()))
                    .unwrap_or_else(|| trading_primitives::Ticker::from("UNKNOWN/UNKNOWN"));
                Some(EngineEvent::OrderRejected {
                    cl_ord_id,
                    pair,
                    reason: error,
                })
            }
            ProxyEvent::Subscribed { channel } => {
                tracing::info!(channel, "Private subscription confirmed");
                None
            }
            ProxyEvent::Heartbeat | ProxyEvent::Pong { .. } => None,
            ProxyEvent::CommandAck { cmd, .. } => {
                tracing::debug!(cmd, "Command acknowledged");
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
        let sub_cmd = ProxyCommand::Subscribe {
            channel: "book".into(),
            symbols: pairs.to_vec(),
            depth: Some(depth),
        };
        let sub_msg = serde_json::to_string(&sub_cmd)?;
        writer.send_raw(&sub_msg).await?;
    }

    let mut sub_rx = sub_rx;
    loop {
        let ws_msg = if let Some(ref mut rx) = sub_rx {
            tokio::select! {
                raw = reader.recv() => {
                    match raw {
                        Some(text) => Some(text),
                        None => break,
                    }
                }
                new_pairs = rx.recv() => {
                    if let Some(new_pairs) = new_pairs {
                        if !new_pairs.is_empty() {
                            tracing::info!(?new_pairs, "Subscribing to new pairs dynamically");
                            let sub_cmd = ProxyCommand::Subscribe {
                                channel: "book".into(),
                                symbols: new_pairs,
                                depth: Some(depth),
                            };
                            let sub_msg = serde_json::to_string(&sub_cmd)?;
                            writer.send_raw(&sub_msg).await?;
                        }
                    }
                    continue;
                }
            }
        } else {
            match reader.recv().await {
                Some(text) => Some(text),
                None => break,
            }
        };

        if let Some(raw) = ws_msg {
            let proxy_event = match parse_proxy_event(&raw) {
                Ok(e) => e,
                Err(_) => {
                    tracing::debug!(raw, "Unknown public WS message");
                    continue;
                }
            };
            match proxy_event {
                ProxyEvent::BookSnapshot { symbol, bids, asks } => {
                    let pair = trading_primitives::Ticker::from(symbol.as_str());
                    tx.send(EngineEvent::BookSnapshot {
                        pair,
                        bids: bids.into_iter().map(|l| LevelUpdate { price: l.price, qty: l.qty }).collect(),
                        asks: asks.into_iter().map(|l| LevelUpdate { price: l.price, qty: l.qty }).collect(),
                        timestamp: Utc::now(),
                    })
                    .await?;
                }
                ProxyEvent::BookUpdate { symbol, bids, asks } => {
                    let pair = trading_primitives::Ticker::from(symbol.as_str());
                    tx.send(EngineEvent::BookUpdate {
                        pair,
                        bid_updates: bids.into_iter().map(|l| LevelUpdate { price: l.price, qty: l.qty }).collect(),
                        ask_updates: asks.into_iter().map(|l| LevelUpdate { price: l.price, qty: l.qty }).collect(),
                        timestamp: Utc::now(),
                    })
                    .await?;
                }
                ProxyEvent::Subscribed { channel } => {
                    tracing::info!(channel, "Subscription confirmed");
                }
                ProxyEvent::Heartbeat | ProxyEvent::Pong { .. } => {}
                _ => {}
            }
        }
    }
    Ok(())
}
