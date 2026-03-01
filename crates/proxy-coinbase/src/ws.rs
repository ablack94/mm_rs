use anyhow::Result;
use axum::extract::ws::{self, WebSocket};
use futures::{SinkExt, StreamExt, stream::SplitSink};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::auth::{build_jwt, build_ws_jwt};
use crate::pairs;
use crate::translate;

/// Type alias for the bot's write half.
type BotWriter = SplitSink<WebSocket, ws::Message>;

/// Tracks exchange order_id and current size for each cl_ord_id.
/// Coinbase requires `size` on every edit, even price-only amends.
#[derive(Clone, Debug)]
struct OrderInfo {
    exchange_id: String,
    size: String,
}

type OrderIdMap = std::collections::HashMap<String, OrderInfo>;
use proxy_common::rate_limit::{TokenBucket, TokenBucketConfig};

const COINBASE_MARKET_WS: &str = "wss://advanced-trade-ws.coinbase.com";
const COINBASE_USER_WS: &str = "wss://advanced-trade-ws-user.coinbase.com";

/// Convert f64 to Decimal for order tracking.
fn dec(f: f64) -> Decimal {
    Decimal::from_f64_retain(f).unwrap_or_default()
}

/// Parse a legacy-format WS message (method/params style) into a ProxyCommand.
fn parse_legacy_command(parsed: &serde_json::Value) -> Option<ProxyCommand> {
    let method = parsed.get("method").and_then(|m| m.as_str())?;
    let params = parsed.get("params").unwrap_or(parsed);
    let req_id = params.get("req_id").and_then(|r| r.as_u64()).unwrap_or(0);

    match method {
        "subscribe" => {
            let channel = params.get("channel").and_then(|c| c.as_str())?.to_string();
            let symbols = params.get("symbol").and_then(|s| s.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            Some(ProxyCommand::Subscribe { channel, symbols, depth: None })
        }
        "add_order" => {
            let symbol = params.get("symbol").and_then(|s| s.as_str())?.to_string();
            let side = params.get("side").and_then(|s| s.as_str())?.to_string();
            let cl_ord_id = params.get("cl_ord_id").and_then(|c| c.as_str())?.to_string();
            let price = dec(params.get("limit_price").and_then(|p| p.as_f64()).unwrap_or(0.0));
            let qty = dec(params.get("order_qty").and_then(|q| q.as_f64()).unwrap_or(0.0));
            let post_only = params.get("post_only").and_then(|p| p.as_bool()).unwrap_or(true);
            let market = params.get("market").and_then(|m| m.as_bool()).unwrap_or(false);
            Some(ProxyCommand::PlaceOrder {
                req_id, symbol, side, order_type: "limit".to_string(),
                price, qty, cl_ord_id, post_only,
                priority: exchange_api::OrderPriority::Normal, market,
            })
        }
        "amend_order" => {
            let cl_ord_id = params.get("cl_ord_id").and_then(|c| c.as_str())?.to_string();
            let price = params.get("limit_price").and_then(|p| p.as_f64()).map(dec);
            let qty = params.get("order_qty").and_then(|q| q.as_f64()).map(dec);
            Some(ProxyCommand::AmendOrder { req_id, cl_ord_id, price, qty })
        }
        "cancel_order" => {
            let cl_ord_ids = params.get("cl_ord_id").and_then(|c| c.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            Some(ProxyCommand::CancelOrders { req_id, cl_ord_ids })
        }
        "cancel_all" => Some(ProxyCommand::CancelAll { req_id }),
        "cancel_all_orders_after" => {
            let timeout_secs = params.get("timeout").and_then(|t| t.as_u64()).unwrap_or(0);
            Some(ProxyCommand::SetDms { req_id, timeout_secs })
        }
        "ping" => Some(ProxyCommand::Ping { req_id }),
        _ => None,
    }
}

use crate::state::ProxyOrderState;
use exchange_api::ProxyCommand;
use exchange_api::order_tracking::{
    TrackedOrder as ProxyTrackedOrder, OrderStatus, FillRecord, FillSource,
};
use rust_decimal::Decimal;

/// State for Coinbase WS proxy.
pub struct CoinbaseWsState {
    pub api_key: String,
    pub api_secret: String,
    pub coinbase_base_url: String,
    /// Shared rate limiter.
    rate_limiter: Mutex<TokenBucket>,
    client: reqwest::Client,
    /// Shared order/fill/position tracking state.
    pub order_state: Arc<ProxyOrderState>,
}

impl CoinbaseWsState {
    pub fn new(
        api_key: String,
        api_secret: String,
        coinbase_base_url: String,
        rate_limit_config: TokenBucketConfig,
        order_state: Arc<ProxyOrderState>,
    ) -> Self {
        Self {
            api_key,
            api_secret,
            coinbase_base_url,
            rate_limiter: Mutex::new(TokenBucket::new(rate_limit_config)),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("Failed to build HTTP client"),
            order_state,
        }
    }

    /// Extract host from base URL.
    fn api_host(&self) -> &str {
        self.coinbase_base_url
            .strip_prefix("https://")
            .or_else(|| self.coinbase_base_url.strip_prefix("http://"))
            .unwrap_or(&self.coinbase_base_url)
            .trim_end_matches('/')
    }

    /// Make an authenticated GET request to Coinbase REST API.
    async fn rest_get(&self, path: &str) -> Result<serde_json::Value> {
        let host = self.api_host();
        let jwt = build_jwt(&self.api_key, &self.api_secret, "GET", host, path)
            .map_err(|e| anyhow::anyhow!("JWT build error: {}", e))?;

        let url = format!("{}{}", self.coinbase_base_url, path);
        let resp = self.client
            .get(&url)
            .header("Authorization", format!("Bearer {}", jwt))
            .header("Content-Type", "application/json")
            .send()
            .await?;

        let status = resp.status();
        let resp_body = resp.text().await?;

        if !status.is_success() {
            tracing::error!(path, %status, body = &resp_body[..resp_body.len().min(300)], "Coinbase REST GET error");
            anyhow::bail!("Coinbase {} {}: {}", status, path, &resp_body[..resp_body.len().min(200)]);
        }

        let data: serde_json::Value = serde_json::from_str(&resp_body)?;
        Ok(data)
    }

    /// Make an authenticated POST request to Coinbase REST API.
    async fn rest_post(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let body_str = serde_json::to_string(body)?;
        let host = self.api_host();
        let jwt = build_jwt(&self.api_key, &self.api_secret, "POST", host, path)
            .map_err(|e| anyhow::anyhow!("JWT build error: {}", e))?;

        let url = format!("{}{}", self.coinbase_base_url, path);
        let resp = self.client
            .post(&url)
            .header("Authorization", format!("Bearer {}", jwt))
            .header("Content-Type", "application/json")
            .body(body_str)
            .send()
            .await?;

        let status = resp.status();
        let resp_body = resp.text().await?;

        if !status.is_success() {
            tracing::error!(path, %status, body = &resp_body[..resp_body.len().min(300)], "Coinbase REST error");
            anyhow::bail!("Coinbase {} {}: {}", status, path, &resp_body[..resp_body.len().min(200)]);
        }

        let data: serde_json::Value = serde_json::from_str(&resp_body)?;
        Ok(data)
    }

    /// Build Coinbase WS subscribe message with JWT auth.
    fn ws_subscribe_msg(&self, channel: &str, product_ids: &[String]) -> String {
        let jwt = build_ws_jwt(&self.api_key, &self.api_secret)
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "Failed to build WS JWT");
                String::new()
            });

        serde_json::json!({
            "type": "subscribe",
            "product_ids": product_ids,
            "channel": channel,
            "jwt": jwt
        })
        .to_string()
    }
}

/// Handle a private WebSocket proxy connection.
/// The bot sends Kraken-format order commands; we translate to Coinbase REST calls.
/// We also connect to Coinbase user WS for fill/order events.
pub async fn handle_coinbase_private_ws(
    bot_ws: WebSocket,
    state: Arc<CoinbaseWsState>,
) {
    let result = run_private_ws_proxy(bot_ws, state).await;
    if let Err(e) = result {
        tracing::error!(error = %e, "Coinbase private WS proxy error");
    }
}

async fn run_private_ws_proxy(
    bot_ws: WebSocket,
    state: Arc<CoinbaseWsState>,
) -> Result<()> {
    // Connect to Coinbase user WebSocket for order/fill events
    tracing::info!("Coinbase private WS proxy: connecting to user WS");
    let (coinbase_ws, _) = connect_async(COINBASE_USER_WS).await?;
    tracing::info!("Coinbase private WS proxy: user WS connected");

    let (mut coinbase_write, coinbase_read) = coinbase_ws.split();
    let (bot_write, mut bot_read) = bot_ws.split();
    let bot_write = Arc::new(Mutex::new(bot_write));

    // Subscribe to user channel for order events
    // We'll subscribe once we see the bot's executions subscribe
    let subscribed = Arc::new(Mutex::new(false));

    // Send heartbeat subscription immediately to keep connection alive
    let heartbeat_sub = serde_json::json!({
        "type": "subscribe",
        "channel": "heartbeats",
        "product_ids": []
    })
    .to_string();
    coinbase_write.send(Message::Text(heartbeat_sub.into())).await?;

    // Track cl_ord_id → (exchange order_id, size) for amend/cancel operations.
    // Coinbase REST returns exchange order_ids, but the bot sends cl_ord_ids.
    let order_id_map: Arc<Mutex<OrderIdMap>> = Arc::new(Mutex::new(OrderIdMap::new()));

    // DMS timer handle — spawned on cancel_all_orders_after, aborted on reset/disconnect
    let dms_timer: Arc<Mutex<Option<JoinHandle<()>>>> = Arc::new(Mutex::new(None));

    // Bot -> Coinbase: intercept order commands, translate to REST
    let state_for_orders = state.clone();
    let bot_write_for_orders = bot_write.clone();
    let subscribed_for_orders = subscribed.clone();
    let order_id_map_for_orders = order_id_map.clone();
    let dms_timer_for_orders = dms_timer.clone();
    let order_state_for_orders = state.order_state.clone();
    let coinbase_write = Arc::new(Mutex::new(coinbase_write));
    let coinbase_write_for_sub = coinbase_write.clone();

    // Track subscribed products for reconnect
    let subscribed_products: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let subscribed_products_for_orders = subscribed_products.clone();

    // Clones for the reconnect loop in coinbase_to_bot
    let state_for_reconnect = state.clone();
    let coinbase_write_for_reconnect = coinbase_write.clone();
    let subscribed_products_for_reconnect = subscribed_products.clone();

    let bot_to_coinbase = tokio::spawn(async move {
        while let Some(Ok(msg)) = bot_read.next().await {
            match msg {
                ws::Message::Text(text) => {
                    let text_str = text.to_string();

                    // Try typed ProxyCommand deserialization first, fall back to legacy format
                    let cmd = serde_json::from_str::<ProxyCommand>(&text_str)
                        .ok()
                        .or_else(|| {
                            let parsed: serde_json::Value = serde_json::from_str(&text_str).ok()?;
                            parse_legacy_command(&parsed)
                        });

                    let cmd = match cmd {
                        Some(c) => c,
                        None => {
                            tracing::debug!("Unknown WS message from bot, ignoring");
                            continue;
                        }
                    };

                    match cmd {
                        ProxyCommand::Subscribe { channel, symbols, .. } => {
                            if channel == "executions" {
                                let mut sub = subscribed_for_orders.lock().await;
                                if !*sub {
                                    let product_ids: Vec<String> = if symbols.is_empty() {
                                        vec!["BTC-USD".to_string()]
                                    } else {
                                        symbols.iter().map(|s| pairs::to_coinbase(s)).collect()
                                    };
                                    let sub_msg = state_for_orders.ws_subscribe_msg(
                                        "user",
                                        &product_ids,
                                    );
                                    let mut writer = coinbase_write_for_sub.lock().await;
                                    let _ = writer.send(Message::Text(sub_msg.into())).await;
                                    *sub = true;
                                    *subscribed_products_for_orders.lock().await = product_ids;
                                }

                                let confirm = translate::subscribe_confirmed("executions");
                                let mut writer = bot_write_for_orders.lock().await;
                                let _ = writer.send(ws::Message::Text(confirm.into())).await;
                            }
                        }
                        ProxyCommand::Ping { req_id } => {
                            let pong = translate::pong_response(req_id);
                            let mut writer = bot_write_for_orders.lock().await;
                            let _ = writer.send(ws::Message::Text(pong.into())).await;
                        }
                        ProxyCommand::PlaceOrder { req_id, symbol, side, cl_ord_id, price, qty, post_only, market, .. } => {
                            handle_place_order(
                                req_id, &symbol, &side, &cl_ord_id, price, qty, post_only, market,
                                &state_for_orders, &bot_write_for_orders,
                                &order_id_map_for_orders, &order_state_for_orders,
                            ).await;
                        }
                        ProxyCommand::AmendOrder { req_id, cl_ord_id, price, qty } => {
                            handle_amend_order(
                                req_id, &cl_ord_id, price, qty,
                                &state_for_orders, &bot_write_for_orders,
                                &order_id_map_for_orders, &order_state_for_orders,
                            ).await;
                        }
                        ProxyCommand::CancelOrders { req_id, cl_ord_ids } => {
                            handle_cancel_orders(
                                req_id, &cl_ord_ids,
                                &state_for_orders, &bot_write_for_orders,
                                &order_id_map_for_orders, &order_state_for_orders,
                            ).await;
                        }
                        ProxyCommand::CancelAll { req_id } => {
                            handle_cancel_all_cmd(
                                req_id,
                                &state_for_orders, &bot_write_for_orders,
                                &order_id_map_for_orders, &order_state_for_orders,
                            ).await;
                        }
                        ProxyCommand::SetDms { req_id, timeout_secs } => {
                            // Abort any existing DMS timer
                            {
                                let mut timer = dms_timer_for_orders.lock().await;
                                if let Some(handle) = timer.take() {
                                    handle.abort();
                                }
                            }

                            // Spawn new timer if timeout > 0 (timeout = 0 disables DMS)
                            if timeout_secs > 0 {
                                let state_for_dms = state_for_orders.clone();
                                let order_id_map_for_dms = order_id_map_for_orders.clone();
                                let order_state_for_dms = order_state_for_orders.clone();
                                let dms_timer_ref = dms_timer_for_orders.clone();
                                let handle = tokio::spawn(async move {
                                    tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)).await;
                                    tracing::warn!(timeout_secs, "DMS timer expired — cancelling all orders");
                                    cancel_all_orders(&state_for_dms, &order_id_map_for_dms, &order_state_for_dms).await;
                                    *dms_timer_ref.lock().await = None;
                                });
                                dms_timer_for_orders.lock().await.replace(handle);
                            }

                            let resp = translate::command_ack(req_id, "set_dms");
                            let mut writer = bot_write_for_orders.lock().await;
                            let _ = writer.send(ws::Message::Text(resp.into())).await;
                        }
                    }
                }
                ws::Message::Close(_) => break,
                _ => {}
            }
        }
    });

    // Coinbase user WS -> Bot: translate events to Kraken execution format
    let bot_write_for_upstream = bot_write.clone();
    let order_id_map_for_cleanup = order_id_map.clone();
    let order_state_for_upstream = state.order_state.clone();
    let coinbase_to_bot = tokio::spawn(async move {
        let mut fill_tracker = translate::FillTracker::new();
        let mut coinbase_read = coinbase_read;
        let mut backoff_secs = 1u64;

        loop {
            while let Some(Ok(msg)) = coinbase_read.next().await {
                backoff_secs = 1;
                match msg {
                    Message::Text(text) => {
                        let text_str = text.to_string();
                        let parsed: serde_json::Value = match serde_json::from_str(&text_str) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };

                        let channel = parsed.get("channel").and_then(|c| c.as_str()).unwrap_or("");

                        match channel {
                            "user" => {
                                let msg_type = parsed.get("type").and_then(|t| t.as_str()).unwrap_or("?");
                                tracing::debug!(msg_type, "Received Coinbase user WS event");

                                cleanup_terminal_orders(&parsed, &order_id_map_for_cleanup, &order_state_for_upstream).await;

                                // Extract and record fills in order tracking state
                                record_ws_fills(&parsed, &order_state_for_upstream).await;

                                let kraken_msgs =
                                    translate::coinbase_user_to_proxy_events(&parsed, &mut fill_tracker);
                                if !kraken_msgs.is_empty() {
                                    tracing::info!(count = kraken_msgs.len(), msg_type, "Forwarding user events to bot");
                                }
                                let mut writer = bot_write_for_upstream.lock().await;
                                for msg in kraken_msgs {
                                    if writer.send(ws::Message::Text(msg.into())).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            "heartbeats" => {
                                let hb = translate::heartbeat();
                                let mut writer = bot_write_for_upstream.lock().await;
                                let _ = writer.send(ws::Message::Text(hb.into())).await;
                            }
                            _ => {
                                tracing::debug!(channel, "Ignoring Coinbase user WS channel");
                            }
                        }
                    }
                    Message::Ping(data) => {
                        let mut writer = bot_write_for_upstream.lock().await;
                        let _ = writer.send(ws::Message::Ping(data.into())).await;
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }

            // Coinbase user WS disconnected — reconnect with backoff
            tracing::warn!(backoff_secs, "Coinbase user WS disconnected, reconnecting...");
            tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(30);

            let (new_ws, _) = match connect_async(COINBASE_USER_WS).await {
                Ok(ws) => ws,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to reconnect to Coinbase user WS");
                    continue;
                }
            };
            tracing::info!("Coinbase private WS: reconnected to user WS");

            let (new_write, new_read) = new_ws.split();
            coinbase_read = new_read;

            // Swap shared write half so bot_to_coinbase uses the new connection
            *coinbase_write_for_reconnect.lock().await = new_write;

            // Re-subscribe heartbeats
            {
                let heartbeat_sub = serde_json::json!({
                    "type": "subscribe",
                    "channel": "heartbeats",
                    "product_ids": []
                })
                .to_string();
                let mut w = coinbase_write_for_reconnect.lock().await;
                let _ = w.send(Message::Text(heartbeat_sub.into())).await;
            }

            // Re-subscribe user channel if previously subscribed
            {
                let products = subscribed_products_for_reconnect.lock().await.clone();
                if !products.is_empty() {
                    let sub_msg = state_for_reconnect.ws_subscribe_msg("user", &products);
                    let mut w = coinbase_write_for_reconnect.lock().await;
                    let _ = w.send(Message::Text(sub_msg.into())).await;
                }
            }
        }
    });

    // Heartbeat task — send periodic heartbeats to bot
    let bot_write_for_hb = bot_write.clone();
    let heartbeat_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            interval.tick().await;
            let hb = translate::heartbeat();
            let mut writer = bot_write_for_hb.lock().await;
            if writer.send(ws::Message::Text(hb.into())).await.is_err() {
                break;
            }
        }
    });

    // Fill reconciliation task — polls recent fills to catch any missed by WS
    let state_for_fills = state.clone();
    let bot_write_for_fills = bot_write.clone();
    let order_id_map_for_fills = order_id_map.clone();
    let order_state_for_fills = state.order_state.clone();
    let fill_reconciler = tokio::spawn(async move {
        let mut seen_trade_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Wait for subscriptions to be established before polling
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        tracing::info!("Fill reconciler started, polling every 10s");
        loop {
            interval.tick().await;
            let tracked = order_id_map_for_fills.lock().await.len();
            tracing::debug!(tracked_orders = tracked, seen_fills = seen_trade_ids.len(), "Fill reconciler polling");
            if let Err(e) = reconcile_fills(
                &state_for_fills,
                &bot_write_for_fills,
                &order_id_map_for_fills,
                &mut seen_trade_ids,
                &order_state_for_fills,
            ).await {
                tracing::warn!(error = %e, "Fill reconciliation poll failed");
            }
        }
    });

    // Only exit when the bot disconnects; coinbase_to_bot handles reconnection internally
    let _ = bot_to_coinbase.await;
    tracing::info!("Coinbase private WS proxy: bot disconnected");
    coinbase_to_bot.abort();
    heartbeat_handle.abort();
    fill_reconciler.abort();
    // Abort DMS timer on disconnect
    if let Some(handle) = dms_timer.lock().await.take() {
        handle.abort();
    }
    Ok(())
}

/// Handle place_order: translate to Coinbase REST POST /orders
async fn handle_place_order(
    req_id: u64,
    symbol: &str,
    side: &str,
    cl_ord_id: &str,
    price: Decimal,
    qty: Decimal,
    post_only: bool,
    is_market: bool,
    state: &CoinbaseWsState,
    bot_write: &Arc<Mutex<BotWriter>>,
    order_id_map: &Arc<Mutex<OrderIdMap>>,
    order_state: &Arc<ProxyOrderState>,
) {
    let product_id = pairs::to_coinbase(symbol);

    let body = if is_market {
        serde_json::json!({
            "client_order_id": cl_ord_id,
            "product_id": product_id,
            "side": side.to_uppercase(),
            "order_configuration": {
                "market_market_ioc": {
                    "base_size": qty.to_string()
                }
            }
        })
    } else {
        serde_json::json!({
            "client_order_id": cl_ord_id,
            "product_id": product_id,
            "side": side.to_uppercase(),
            "order_configuration": {
                "limit_limit_gtc": {
                    "base_size": qty.to_string(),
                    "limit_price": price.to_string(),
                    "post_only": post_only
                }
            }
        })
    };

    // Rate limit check
    {
        let allowed = state.rate_limiter.lock().await.try_consume("place_order");
        if !allowed {
            send_error(bot_write, "place_order", req_id, "EOrder:Rate limit exceeded [proxy]").await;
            return;
        }
    }

    match state.rest_post("/api/v3/brokerage/orders", &body).await {
        Ok(resp) => {
            tracing::debug!(symbol, cl_ord_id, response = %resp, "Coinbase order response");
            let success = resp.get("success").and_then(|s| s.as_bool()).unwrap_or(false);
            if success {
                let order_id = resp
                    .get("success_response")
                    .and_then(|r| r.get("order_id"))
                    .and_then(|id| id.as_str())
                    .unwrap_or("");

                if !order_id.is_empty() {
                    order_id_map.lock().await.insert(cl_ord_id.to_string(), OrderInfo {
                        exchange_id: order_id.to_string(),
                        size: qty.to_string(),
                    });

                    order_state.orders.lock().await.insert(ProxyTrackedOrder {
                        cl_ord_id: cl_ord_id.to_string(),
                        exchange_id: order_id.to_string(),
                        pair: pairs::to_internal(&product_id),
                        side: side.to_string(),
                        price,
                        original_qty: qty,
                        filled_qty: Decimal::ZERO,
                        status: OrderStatus::Pending,
                    });
                }

                let response = translate::order_response_success("place_order", req_id, order_id, cl_ord_id);
                let mut writer = bot_write.lock().await;
                let _ = writer.send(ws::Message::Text(response.into())).await;

                let exec_msg = translate::synthetic_exec_new(order_id, cl_ord_id, symbol, side);
                let _ = writer.send(ws::Message::Text(exec_msg.into())).await;
            } else {
                let error = resp
                    .get("error_response")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("Unknown Coinbase error");
                send_error(bot_write, "place_order", req_id, error).await;
            }
        }
        Err(e) => {
            send_error(bot_write, "place_order", req_id, &format!("REST error: {}", e)).await;
        }
    }
}

/// Handle amend_order: translate to Coinbase REST POST /orders/edit
async fn handle_amend_order(
    req_id: u64,
    cl_ord_id: &str,
    price: Option<Decimal>,
    qty: Option<Decimal>,
    state: &CoinbaseWsState,
    bot_write: &Arc<Mutex<BotWriter>>,
    order_id_map: &Arc<Mutex<OrderIdMap>>,
    order_state: &Arc<ProxyOrderState>,
) {
    let order_info = order_id_map.lock().await.get(cl_ord_id).cloned();
    let order_info = match order_info {
        Some(info) => info,
        None => {
            tracing::warn!(cl_ord_id, "No exchange order_id found for amend — rejecting");
            send_error(bot_write, "amend_order", req_id, "Unknown order (no order_id mapping)").await;
            return;
        }
    };

    // Rate limit check
    {
        let allowed = state.rate_limiter.lock().await.try_consume("amend_order");
        if !allowed {
            send_error(bot_write, "amend_order", req_id, "EOrder:Rate limit exceeded [proxy]").await;
            return;
        }
    }

    // Coinbase requires `size` on every edit — use provided qty or fall back to stored size.
    let size = match qty {
        Some(q) => q.to_string(),
        None => order_info.size.clone(),
    };

    let mut body = serde_json::json!({
        "order_id": order_info.exchange_id,
        "size": size
    });

    if let Some(p) = price {
        body["price"] = serde_json::json!(p.to_string());
    }

    match state.rest_post("/api/v3/brokerage/orders/edit", &body).await {
        Ok(resp) => {
            let success = resp.get("success").and_then(|s| s.as_bool()).unwrap_or(false);
            if success {
                if qty.is_some() {
                    if let Some(info) = order_id_map.lock().await.get_mut(cl_ord_id) {
                        info.size = size.clone();
                    }
                }

                order_state.orders.lock().await.update_price_qty(cl_ord_id, price, qty);

                let response = translate::order_response_success("amend_order", req_id, &order_info.exchange_id, cl_ord_id);
                let mut writer = bot_write.lock().await;
                let _ = writer.send(ws::Message::Text(response.into())).await;
            } else {
                let errors = resp.get("errors").and_then(|e| e.as_array());
                let error_msg = errors
                    .and_then(|errs| errs.first())
                    .and_then(|e| e.get("edit_failure_reason"))
                    .and_then(|r| r.as_str())
                    .unwrap_or("Edit failed");
                send_error(bot_write, "amend_order", req_id, error_msg).await;
            }
        }
        Err(e) => {
            send_error(bot_write, "amend_order", req_id, &format!("REST error: {}", e)).await;
        }
    }
}

/// Handle cancel_orders: translate to Coinbase REST POST /orders/batch_cancel
async fn handle_cancel_orders(
    req_id: u64,
    cl_ord_ids: &[String],
    state: &CoinbaseWsState,
    bot_write: &Arc<Mutex<BotWriter>>,
    order_id_map: &Arc<Mutex<OrderIdMap>>,
    order_state: &Arc<ProxyOrderState>,
) {
    if cl_ord_ids.is_empty() {
        send_error(bot_write, "cancel_orders", req_id, "No order IDs provided").await;
        return;
    }

    // Rate limit check
    {
        let allowed = state.rate_limiter.lock().await.try_consume("cancel_orders");
        if !allowed {
            send_error(bot_write, "cancel_orders", req_id, "EOrder:Rate limit exceeded [proxy]").await;
            return;
        }
    }

    let map = order_id_map.lock().await;
    let mut exchange_ids: Vec<String> = Vec::new();
    for cl_id in cl_ord_ids {
        match map.get(cl_id) {
            Some(info) => exchange_ids.push(info.exchange_id.clone()),
            None => tracing::warn!(cl_ord_id = cl_id.as_str(), "No exchange order_id for cancel — skipping"),
        }
    }
    drop(map);

    if exchange_ids.is_empty() {
        send_error(bot_write, "cancel_orders", req_id, "No exchange order_ids found for given cl_ord_ids").await;
        return;
    }

    let body = serde_json::json!({ "order_ids": exchange_ids });

    match state.rest_post("/api/v3/brokerage/orders/batch_cancel", &body).await {
        Ok(_resp) => {
            let mut map = order_id_map.lock().await;
            for cl_id in cl_ord_ids {
                map.remove(cl_id);
            }

            {
                let mut orders = order_state.orders.lock().await;
                for cl_id in cl_ord_ids {
                    orders.update_status(cl_id, OrderStatus::Cancelled);
                }
            }

            let response = translate::order_response_success("cancel_orders", req_id, "", "");
            let mut writer = bot_write.lock().await;
            let _ = writer.send(ws::Message::Text(response.into())).await;
        }
        Err(e) => {
            send_error(bot_write, "cancel_orders", req_id, &format!("REST error: {}", e)).await;
        }
    }
}

/// Cancel all open orders via Coinbase REST API and clear the order_id_map.
/// Used by both the `cancel_all` WS handler and the DMS timer.
async fn cancel_all_orders(
    state: &CoinbaseWsState,
    order_id_map: &Arc<Mutex<OrderIdMap>>,
    order_state: &Arc<ProxyOrderState>,
) {
    let path = "/api/v3/brokerage/orders/historical/batch?order_status=OPEN&limit=250";
    let host = state.api_host();
    let jwt = match build_jwt(&state.api_key, &state.api_secret, "GET", host, path) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!(error = %e, "cancel_all_orders: JWT error");
            return;
        }
    };

    let url = format!("{}{}", state.coinbase_base_url, path);
    let resp = match state
        .client
        .get(&url)
        .header("Authorization", format!("Bearer {}", jwt))
        .header("Content-Type", "application/json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "cancel_all_orders: REST error");
            return;
        }
    };

    let data: serde_json::Value = match resp.json().await {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "cancel_all_orders: parse error");
            return;
        }
    };

    let order_ids: Vec<String> = data
        .get("orders")
        .and_then(|o| o.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|o| o.get("order_id").and_then(|id| id.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();

    tracing::info!(count = order_ids.len(), "cancel_all_orders: found open orders to cancel");

    if !order_ids.is_empty() {
        let body = serde_json::json!({"order_ids": order_ids});
        match state.rest_post("/api/v3/brokerage/orders/batch_cancel", &body).await {
            Ok(_) => tracing::info!("cancel_all_orders: batch cancel succeeded"),
            Err(e) => tracing::error!(error = %e, "cancel_all_orders: batch cancel failed"),
        }
    }

    // Clear the order_id_map since all orders are cancelled
    {
        let mut map = order_id_map.lock().await;
        let count = map.len();
        map.clear();
        if count > 0 {
            tracing::debug!(cleared = count, "cancel_all_orders: cleared order_id_map");
        }
    }

    // Mark all tracked orders as cancelled
    order_state.orders.lock().await.cancel_all();
}

/// Handle cancel_all: cancel all open orders via Coinbase REST
async fn handle_cancel_all_cmd(
    req_id: u64,
    state: &CoinbaseWsState,
    bot_write: &Arc<Mutex<BotWriter>>,
    order_id_map: &Arc<Mutex<OrderIdMap>>,
    order_state: &Arc<ProxyOrderState>,
) {
    cancel_all_orders(state, order_id_map, order_state).await;
    let response = translate::order_response_success("cancel_all", req_id, "", "");
    let mut writer = bot_write.lock().await;
    let _ = writer.send(ws::Message::Text(response.into())).await;
}

/// Poll recent fills from Coinbase REST and emit any that the WS missed.
/// Tracks seen trade_ids to avoid duplicates.
async fn reconcile_fills(
    state: &CoinbaseWsState,
    bot_write: &Arc<Mutex<BotWriter>>,
    order_id_map: &Arc<Mutex<OrderIdMap>>,
    seen_trade_ids: &mut std::collections::HashSet<String>,
    order_state: &Arc<ProxyOrderState>,
) -> Result<()> {
    let path = "/api/v3/brokerage/orders/historical/fills?limit=20";
    let data = state.rest_get(path).await?;

    let fills = match data.get("fills").and_then(|f| f.as_array()) {
        Some(f) => f,
        None => return Ok(()),
    };

    let mut new_fills = Vec::new();

    // Build reverse map: exchange_id → cl_ord_id (Coinbase fills API doesn't return client_order_id)
    let reverse_map: std::collections::HashMap<String, String> = {
        let map = order_id_map.lock().await;
        map.iter().map(|(cl_id, info)| (info.exchange_id.clone(), cl_id.clone())).collect()
    };

    for fill in fills {
        let trade_id = fill.get("trade_id").and_then(|t| t.as_str()).unwrap_or("");
        if trade_id.is_empty() {
            continue;
        }

        // Skip if we've seen this fill before
        if seen_trade_ids.contains(trade_id) {
            continue;
        }
        seen_trade_ids.insert(trade_id.to_string());

        // Match by order_id (exchange_id) since Coinbase doesn't return client_order_id in fills
        let exchange_order_id = fill.get("order_id").and_then(|o| o.as_str()).unwrap_or("");
        let cl_ord_id = match reverse_map.get(exchange_order_id) {
            Some(cl_id) => cl_id.as_str(),
            None => continue, // Not one of our orders
        };

        let product_id = fill.get("product_id").and_then(|p| p.as_str()).unwrap_or("");
        let symbol = pairs::to_internal(product_id);
        let side = fill.get("side").and_then(|s| s.as_str()).unwrap_or("BUY").to_lowercase();
        let price_f: f64 = fill.get("price").and_then(|p| p.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let qty_f: f64 = fill.get("size").and_then(|s| s.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let fee_f: f64 = fill.get("commission").and_then(|c| c.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let timestamp = fill.get("trade_time").and_then(|t| t.as_str()).unwrap_or("");

        if qty_f <= 0.0 || price_f <= 0.0 {
            continue;
        }

        tracing::warn!(
            trade_id, cl_ord_id, product_id, side = side.as_str(), price = price_f, qty = qty_f,
            "Fill reconciliation: emitting missed fill"
        );

        // Record reconciliation fill in tracking state (Decimal)
        {
            let fill = FillRecord {
                fill_id: trade_id.to_string(),
                cl_ord_id: cl_ord_id.to_string(),
                pair: symbol.clone(),
                side: side.clone(),
                price: dec(price_f),
                qty: dec(qty_f),
                fee: dec(fee_f),
                timestamp: timestamp.to_string(),
                source: FillSource::Reconciliation,
            };
            order_state.fills.lock().await.record(fill);
            order_state.orders.lock().await.add_fill(cl_ord_id, dec(qty_f));
            order_state.positions.lock().await.apply_fill(&symbol, &side, dec(qty_f), dec(price_f));
        }

        new_fills.push(translate::build_fill_event(
            exchange_order_id,
            cl_ord_id,
            &symbol,
            &side,
            qty_f,
            price_f,
            fee_f,
            timestamp,
        ));

        // Clean up the order from our map since it's filled
        order_id_map.lock().await.remove(cl_ord_id);
    }

    if !new_fills.is_empty() {
        tracing::warn!(count = new_fills.len(), "Fill reconciliation: forwarding missed fills to bot");
        let mut writer = bot_write.lock().await;
        for msg in new_fills {
            let _ = writer.send(ws::Message::Text(msg.into())).await;
        }
    }

    Ok(())
}

/// Remove entries from order_id_map when orders reach terminal states (filled, cancelled, expired, failed).
async fn cleanup_terminal_orders(
    coinbase_msg: &serde_json::Value,
    order_id_map: &Arc<Mutex<OrderIdMap>>,
    order_state: &Arc<ProxyOrderState>,
) {
    let events = match coinbase_msg.get("events").and_then(|e| e.as_array()) {
        Some(e) => e,
        None => return,
    };

    let mut to_remove = Vec::new();
    for event in events {
        let orders = match event.get("orders").and_then(|o| o.as_array()) {
            Some(o) => o,
            None => continue,
        };
        for order in orders {
            let status = order.get("status").and_then(|s| s.as_str()).unwrap_or("");
            let cl_ord_id = order.get("client_order_id").and_then(|c| c.as_str()).unwrap_or("");
            if matches!(status, "FILLED" | "CANCELLED" | "EXPIRED" | "FAILED") && !cl_ord_id.is_empty() {
                to_remove.push(cl_ord_id.to_string());
            }
        }
    }

    if !to_remove.is_empty() {
        let mut map = order_id_map.lock().await;
        for cl_id in &to_remove {
            map.remove(cl_id);
        }
        tracing::debug!(count = to_remove.len(), remaining = map.len(), "Cleaned terminal orders from order_id_map");

        // Update order tracking state
        let mut orders = order_state.orders.lock().await;
        for cl_id in &to_remove {
            // Determine the correct terminal status
            // (We already know it's terminal from the match above, default to Cancelled)
            orders.update_status(cl_id, OrderStatus::Cancelled);
        }
    }
}

/// Extract fill data from Coinbase user WS events and record in order tracking state.
/// This runs alongside the existing FillTracker translation but targets the proxy's
/// own position tracking rather than the bot's execution reports.
async fn record_ws_fills(
    coinbase_msg: &serde_json::Value,
    order_state: &Arc<ProxyOrderState>,
) {
    let events = match coinbase_msg.get("events").and_then(|e| e.as_array()) {
        Some(e) => e,
        None => return,
    };

    for event in events {
        let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
        // Only process update events (not snapshots — those are pre-existing state)
        if event_type == "snapshot" {
            continue;
        }
        let orders = match event.get("orders").and_then(|o| o.as_array()) {
            Some(o) => o,
            None => continue,
        };

        for order in orders {
            let status = order.get("status").and_then(|s| s.as_str()).unwrap_or("");
            // Only process orders with fill data (FILLED or OPEN with cumulative_quantity > 0)
            if !matches!(status, "FILLED" | "OPEN") {
                continue;
            }

            let cum_qty: f64 = order.get("cumulative_quantity")
                .and_then(|q| q.as_str()).and_then(|q| q.parse().ok()).unwrap_or(0.0);
            if cum_qty <= 0.0 {
                continue;
            }

            let avg_price: f64 = order.get("avg_price")
                .and_then(|p| p.as_str()).and_then(|p| p.parse().ok()).unwrap_or(0.0);
            let total_fees: f64 = order.get("total_fees")
                .and_then(|f| f.as_str()).and_then(|f| f.parse().ok()).unwrap_or(0.0);
            let client_order_id = order.get("client_order_id")
                .and_then(|c| c.as_str()).unwrap_or("");
            let order_id = order.get("order_id")
                .and_then(|o| o.as_str()).unwrap_or("");
            let product_id = order.get("product_id")
                .and_then(|p| p.as_str()).unwrap_or("");
            let side = order.get("order_side")
                .and_then(|s| s.as_str()).unwrap_or("").to_lowercase();
            let timestamp = order.get("creation_time")
                .and_then(|t| t.as_str()).unwrap_or("");

            if client_order_id.is_empty() || product_id.is_empty() {
                continue;
            }

            let symbol = pairs::to_internal(product_id);

            // Use a synthetic fill_id based on cl_ord_id + cumulative qty to deduplicate.
            let fill_id = format!("{}-ws-{:.8}", client_order_id, cum_qty);

            let fill = FillRecord {
                fill_id,
                cl_ord_id: client_order_id.to_string(),
                pair: symbol.clone(),
                side: side.clone(),
                price: dec(avg_price),
                qty: dec(cum_qty),
                fee: dec(total_fees),
                timestamp: timestamp.to_string(),
                source: FillSource::WebSocket,
            };

            let recorded = order_state.fills.lock().await.record(fill);
            if recorded {
                let prev_filled = order_state.orders.lock().await
                    .get(client_order_id)
                    .map(|o| o.filled_qty)
                    .unwrap_or(Decimal::ZERO);
                let inc_qty = dec(cum_qty) - prev_filled;
                if inc_qty > dec(1e-12) {
                    let inc_cost = dec(avg_price) * dec(cum_qty) - dec(avg_price) * prev_filled;
                    let inc_price = if inc_qty > Decimal::ZERO { inc_cost / inc_qty } else { dec(avg_price) };
                    order_state.orders.lock().await.add_fill(client_order_id, inc_qty);
                    order_state.positions.lock().await.apply_fill(&symbol, &side, inc_qty, inc_price);
                }

                if status == "FILLED" {
                    order_state.orders.lock().await.update_status(client_order_id, OrderStatus::Filled);
                }

                tracing::debug!(
                    cl_ord_id = client_order_id,
                    order_id,
                    pair = symbol.as_str(),
                    side = side.as_str(),
                    cum_qty,
                    avg_price,
                    "Recorded WS fill in order tracking state"
                );
            }
        }
    }
}

/// Send error response back to bot.
async fn send_error(
    bot_write: &Arc<Mutex<BotWriter>>,
    method: &str,
    req_id: u64,
    error: &str,
) {
    let response = translate::order_response_error(method, req_id, error);
    let mut writer = bot_write.lock().await;
    let _ = writer.send(ws::Message::Text(response.into())).await;
}

/// Handle a public WebSocket proxy connection.
/// Translates Coinbase level2 book data to Kraken book format.
pub async fn handle_coinbase_public_ws(
    bot_ws: WebSocket,
    state: Arc<CoinbaseWsState>,
) {
    let result = run_public_ws_proxy(bot_ws, state).await;
    if let Err(e) = result {
        tracing::error!(error = %e, "Coinbase public WS proxy error");
    }
}

async fn run_public_ws_proxy(
    bot_ws: WebSocket,
    state: Arc<CoinbaseWsState>,
) -> Result<()> {
    tracing::info!("Coinbase public WS proxy: connecting to market data WS");
    let (coinbase_ws, _) = connect_async(COINBASE_MARKET_WS).await?;
    tracing::info!("Coinbase public WS proxy: connected");

    let (coinbase_write, coinbase_read) = coinbase_ws.split();
    let (bot_write, mut bot_read) = bot_ws.split();

    let bot_write = Arc::new(Mutex::new(bot_write));
    let coinbase_write = Arc::new(Mutex::new(coinbase_write));

    // Coinbase merges order books for USD/USDC pairs and always returns the
    // base-USD product_id in level2 data, even when subscribed via base-USDC.
    // Track the mapping so we can relabel book data with the correct symbol.
    // Key: Coinbase base product (e.g., "DOGE-USD"), Value: internal symbol the bot expects (e.g., "DOGE/USDC").
    let product_id_map: Arc<Mutex<std::collections::HashMap<String, String>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));

    // Track channel subscriptions for reconnect: Vec<(channel, product_ids)>
    let subscribed_channels: Arc<Mutex<Vec<(String, Vec<String>)>>> =
        Arc::new(Mutex::new(Vec::new()));

    // Bot -> Coinbase: translate subscribe messages
    let state_for_sub = state.clone();
    let coinbase_write_for_sub = coinbase_write.clone();
    let bot_write_for_sub = bot_write.clone();
    let product_id_map_for_sub = product_id_map.clone();
    let subscribed_channels_for_sub = subscribed_channels.clone();

    // Clones for the reconnect loop in coinbase_to_bot
    let state_for_reconnect = state.clone();
    let coinbase_write_for_reconnect = coinbase_write.clone();
    let subscribed_channels_for_reconnect = subscribed_channels.clone();

    let bot_to_coinbase = tokio::spawn(async move {
        while let Some(Ok(msg)) = bot_read.next().await {
            match msg {
                ws::Message::Text(text) => {
                    let text_str = text.to_string();

                    let cmd = serde_json::from_str::<ProxyCommand>(&text_str)
                        .ok()
                        .or_else(|| {
                            let parsed: serde_json::Value = serde_json::from_str(&text_str).ok()?;
                            parse_legacy_command(&parsed)
                        });

                    match cmd {
                        Some(ProxyCommand::Subscribe { channel, symbols, .. }) => {
                            if let Some(coinbase_sub) = translate::subscribe_to_coinbase(&channel, &symbols) {
                                let coinbase_channel = coinbase_sub.get("channel").and_then(|c| c.as_str()).unwrap_or("");
                                let product_ids: Vec<String> = coinbase_sub
                                    .get("product_ids")
                                    .and_then(|p| p.as_array())
                                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                                    .unwrap_or_default();

                                {
                                    let mut map = product_id_map_for_sub.lock().await;
                                    for (pid, sym) in product_ids.iter().zip(symbols.iter()) {
                                        let base_pid = pid.replace("-USDC", "-USD");
                                        map.insert(base_pid, sym.clone());
                                        map.insert(pid.clone(), sym.clone());
                                    }
                                }

                                let sub_msg = state_for_sub.ws_subscribe_msg(coinbase_channel, &product_ids);
                                let mut writer = coinbase_write_for_sub.lock().await;
                                let _ = writer.send(Message::Text(sub_msg.into())).await;
                                drop(writer);

                                subscribed_channels_for_sub.lock().await
                                    .push((coinbase_channel.to_string(), product_ids.clone()));

                                let confirm = translate::subscribe_confirmed(&channel);
                                let mut bot_writer = bot_write_for_sub.lock().await;
                                let _ = bot_writer.send(ws::Message::Text(confirm.into())).await;
                            }
                        }
                        Some(ProxyCommand::Ping { req_id }) => {
                            let pong = translate::pong_response(req_id);
                            let mut writer = bot_write_for_sub.lock().await;
                            let _ = writer.send(ws::Message::Text(pong.into())).await;
                        }
                        _ => {}
                    }
                }
                ws::Message::Close(_) => break,
                _ => {}
            }
        }
    });

    // Coinbase -> Bot: translate book data to Kraken format
    let bot_write_for_upstream = bot_write.clone();
    let coinbase_to_bot = tokio::spawn(async move {
        let mut coinbase_read = coinbase_read;
        let mut backoff_secs = 1u64;

        loop {
            while let Some(Ok(msg)) = coinbase_read.next().await {
                backoff_secs = 1;
                match msg {
                    Message::Text(text) => {
                        let text_str = text.to_string();
                        let parsed: serde_json::Value = match serde_json::from_str(&text_str) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };

                        let channel = parsed.get("channel").and_then(|c| c.as_str()).unwrap_or("");

                        match channel {
                            "l2_data" => {
                                let map = product_id_map.lock().await;
                                if let Some(kraken_msg) = translate::coinbase_book_to_proxy_event_mapped(&parsed, &map) {
                                    drop(map);
                                    let mut writer = bot_write_for_upstream.lock().await;
                                    if writer
                                        .send(ws::Message::Text(kraken_msg.into()))
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                            }
                            "heartbeats" => {
                                let hb = translate::heartbeat();
                                let mut writer = bot_write_for_upstream.lock().await;
                                let _ = writer.send(ws::Message::Text(hb.into())).await;
                            }
                            "subscriptions" => {
                                tracing::debug!("Coinbase subscription confirmed");
                            }
                            _ => {
                                tracing::debug!(channel, "Ignoring Coinbase market WS channel");
                            }
                        }
                    }
                    Message::Ping(data) => {
                        let mut writer = bot_write_for_upstream.lock().await;
                        let _ = writer.send(ws::Message::Ping(data.into())).await;
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }

            // Coinbase market WS disconnected — reconnect with backoff
            tracing::warn!(backoff_secs, "Coinbase market WS disconnected, reconnecting...");
            tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(30);

            let (new_ws, _) = match connect_async(COINBASE_MARKET_WS).await {
                Ok(ws) => ws,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to reconnect to Coinbase market WS");
                    continue;
                }
            };
            tracing::info!("Coinbase public WS: reconnected to market WS");

            let (new_write, new_read) = new_ws.split();
            coinbase_read = new_read;

            // Swap shared write half
            *coinbase_write_for_reconnect.lock().await = new_write;

            // Re-subscribe all tracked channels with fresh JWTs
            {
                let subs = subscribed_channels_for_reconnect.lock().await.clone();
                for (channel, product_ids) in &subs {
                    let sub_msg = state_for_reconnect.ws_subscribe_msg(channel, product_ids);
                    let mut w = coinbase_write_for_reconnect.lock().await;
                    let _ = w.send(Message::Text(sub_msg.into())).await;
                }
            }
        }
    });

    // Only exit when the bot disconnects; coinbase_to_bot handles reconnection internally
    let _ = bot_to_coinbase.await;
    tracing::info!("Coinbase public WS proxy: bot disconnected");
    coinbase_to_bot.abort();

    Ok(())
}
