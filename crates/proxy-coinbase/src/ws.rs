use anyhow::Result;
use axum::extract::ws::{self, WebSocket};
use futures::{SinkExt, StreamExt, stream::SplitSink};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::auth::{build_jwt, build_ws_jwt};
use crate::pairs;
use crate::translate;

/// Type alias for the bot's write half.
type BotWriter = SplitSink<WebSocket, ws::Message>;
use proxy_common::rate_limit::{TokenBucket, TokenBucketConfig};

const COINBASE_MARKET_WS: &str = "wss://advanced-trade-ws.coinbase.com";
const COINBASE_USER_WS: &str = "wss://advanced-trade-ws-user.coinbase.com";

/// State for Coinbase WS proxy.
pub struct CoinbaseWsState {
    pub api_key: String,
    pub api_secret: String,
    pub coinbase_base_url: String,
    /// Shared rate limiter.
    rate_limiter: Mutex<TokenBucket>,
    client: reqwest::Client,
}

impl CoinbaseWsState {
    pub fn new(
        api_key: String,
        api_secret: String,
        coinbase_base_url: String,
        rate_limit_config: TokenBucketConfig,
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

    let (mut coinbase_write, mut coinbase_read) = coinbase_ws.split();
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

    // Track cl_ord_id → Coinbase order_id mapping for amend/cancel operations.
    // Coinbase REST returns exchange order_ids, but the bot sends cl_ord_ids.
    let order_id_map: Arc<Mutex<std::collections::HashMap<String, String>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));

    // Bot -> Coinbase: intercept order commands, translate to REST
    let state_for_orders = state.clone();
    let bot_write_for_orders = bot_write.clone();
    let subscribed_for_orders = subscribed.clone();
    let order_id_map_for_orders = order_id_map.clone();
    let coinbase_write = Arc::new(Mutex::new(coinbase_write));
    let coinbase_write_for_sub = coinbase_write.clone();

    let bot_to_coinbase = tokio::spawn(async move {
        while let Some(Ok(msg)) = bot_read.next().await {
            match msg {
                ws::Message::Text(text) => {
                    let text_str = text.to_string();
                    let parsed: serde_json::Value = match serde_json::from_str(&text_str) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    let method = parsed.get("method").and_then(|m| m.as_str()).unwrap_or("");

                    match method {
                        "subscribe" => {
                            // Handle executions subscription
                            let channel = parsed
                                .get("params")
                                .and_then(|p| p.get("channel"))
                                .and_then(|c| c.as_str())
                                .unwrap_or("");

                            if channel == "executions" {
                                let mut sub = subscribed_for_orders.lock().await;
                                if !*sub {
                                    // Subscribe to user channel on Coinbase.
                                    // Coinbase delivers ALL order events regardless of product_ids,
                                    // but still requires at least one product_id in the subscribe.
                                    // Extract the bot's symbols from the subscribe params and convert.
                                    let product_ids: Vec<String> = parsed
                                        .get("params")
                                        .and_then(|p| p.get("symbol"))
                                        .and_then(|s| s.as_array())
                                        .map(|arr| {
                                            arr.iter()
                                                .filter_map(|v| v.as_str())
                                                .map(|s| pairs::to_coinbase(s))
                                                .collect()
                                        })
                                        .unwrap_or_else(|| vec!["BTC-USD".to_string()]);
                                    let sub_msg = state_for_orders.ws_subscribe_msg(
                                        "user",
                                        &product_ids,
                                    );
                                    let mut writer = coinbase_write_for_sub.lock().await;
                                    let _ = writer.send(Message::Text(sub_msg.into())).await;
                                    *sub = true;
                                }

                                // Send confirmation back to bot
                                let confirm = translate::subscribe_confirmed("executions");
                                let mut writer = bot_write_for_orders.lock().await;
                                let _ = writer.send(ws::Message::Text(confirm.into())).await;

                                // Send empty snapshot
                                let snapshot = serde_json::json!({
                                    "channel": "executions",
                                    "type": "snapshot",
                                    "data": []
                                })
                                .to_string();
                                let _ = writer.send(ws::Message::Text(snapshot.into())).await;
                            }
                        }
                        "ping" => {
                            let req_id = parsed.get("req_id").and_then(|r| r.as_u64()).unwrap_or(0);
                            let pong = translate::pong_response(req_id);
                            let mut writer = bot_write_for_orders.lock().await;
                            let _ = writer.send(ws::Message::Text(pong.into())).await;
                        }
                        "add_order" => {
                            handle_add_order(&parsed, &state_for_orders, &bot_write_for_orders, &order_id_map_for_orders).await;
                        }
                        "amend_order" => {
                            handle_amend_order(&parsed, &state_for_orders, &bot_write_for_orders, &order_id_map_for_orders).await;
                        }
                        "cancel_order" => {
                            handle_cancel_order(&parsed, &state_for_orders, &bot_write_for_orders, &order_id_map_for_orders).await;
                        }
                        "cancel_all" => {
                            handle_cancel_all(&parsed, &state_for_orders, &bot_write_for_orders, &order_id_map_for_orders).await;
                        }
                        "cancel_all_orders_after" => {
                            // DMS: Coinbase doesn't support this. Send success acknowledgment.
                            let req_id = parsed.get("req_id").and_then(|r| r.as_u64()).unwrap_or(0);
                            let resp = translate::order_response_success(
                                "cancel_all_orders_after",
                                req_id,
                                "",
                                "",
                            );
                            let mut writer = bot_write_for_orders.lock().await;
                            let _ = writer.send(ws::Message::Text(resp.into())).await;
                        }
                        _ => {
                            tracing::debug!(method, "Unknown WS method from bot, ignoring");
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
    let coinbase_to_bot = tokio::spawn(async move {
        let mut fill_tracker = translate::FillTracker::new();

        while let Some(Ok(msg)) = coinbase_read.next().await {
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
                            // Clean up order_id_map for terminal order states
                            cleanup_terminal_orders(&parsed, &order_id_map_for_cleanup).await;

                            let kraken_msgs =
                                translate::coinbase_user_to_kraken(&parsed, &mut fill_tracker);
                            let mut writer = bot_write_for_upstream.lock().await;
                            for msg in kraken_msgs {
                                if writer.send(ws::Message::Text(msg.into())).await.is_err() {
                                    return;
                                }
                            }
                        }
                        "heartbeats" => {
                            // Forward as Kraken-style heartbeat
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

    tokio::select! {
        _ = bot_to_coinbase => {
            tracing::info!("Coinbase private WS proxy: bot disconnected");
        }
        _ = coinbase_to_bot => {
            tracing::info!("Coinbase private WS proxy: Coinbase user WS disconnected");
        }
    }

    heartbeat_handle.abort();
    Ok(())
}

/// Handle add_order: translate Kraken WS add_order to Coinbase REST POST /orders
async fn handle_add_order(
    parsed: &serde_json::Value,
    state: &CoinbaseWsState,
    bot_write: &Arc<Mutex<BotWriter>>,
    order_id_map: &Arc<Mutex<std::collections::HashMap<String, String>>>,
) {
    let req_id = parsed.get("req_id").and_then(|r| r.as_u64()).unwrap_or(0);
    let params = match parsed.get("params") {
        Some(p) => p,
        None => {
            send_error(bot_write, "add_order", req_id, "Missing params").await;
            return;
        }
    };

    let symbol = params.get("symbol").and_then(|s| s.as_str()).unwrap_or("");
    let side = params.get("side").and_then(|s| s.as_str()).unwrap_or("buy");
    let cl_ord_id = params.get("cl_ord_id").and_then(|c| c.as_str()).unwrap_or("");
    let limit_price = params.get("limit_price").and_then(|p| p.as_f64()).unwrap_or(0.0);
    let order_qty = params.get("order_qty").and_then(|q| q.as_f64()).unwrap_or(0.0);
    let post_only = params.get("post_only").and_then(|p| p.as_bool()).unwrap_or(true);

    let product_id = pairs::to_coinbase(symbol);

    let body = serde_json::json!({
        "client_order_id": cl_ord_id,
        "product_id": product_id,
        "side": side.to_uppercase(),
        "order_configuration": {
            "limit_limit_gtc": {
                "base_size": format!("{}", order_qty),
                "limit_price": format!("{}", limit_price),
                "post_only": post_only
            }
        }
    });

    // Rate limit check
    {
        let allowed = state.rate_limiter.lock().await.try_consume("add_order");
        if !allowed {
            send_error(bot_write, "add_order", req_id, "EOrder:Rate limit exceeded [proxy]").await;
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

                // Store cl_ord_id → exchange order_id mapping for amend/cancel
                if !order_id.is_empty() {
                    order_id_map.lock().await.insert(cl_ord_id.to_string(), order_id.to_string());
                }

                let response = translate::order_response_success("add_order", req_id, order_id, cl_ord_id);
                let mut writer = bot_write.lock().await;
                let _ = writer.send(ws::Message::Text(response.into())).await;

                // Send synthetic execution report so bot gets OrderAcknowledged immediately.
                // The user WS may also send an OPEN event later; duplicate acks are harmless.
                let exec_msg = translate::synthetic_exec_new(order_id, cl_ord_id, symbol, side);
                let _ = writer.send(ws::Message::Text(exec_msg.into())).await;
            } else {
                let error = resp
                    .get("error_response")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("Unknown Coinbase error");
                send_error(bot_write, "add_order", req_id, error).await;
            }
        }
        Err(e) => {
            send_error(bot_write, "add_order", req_id, &format!("REST error: {}", e)).await;
        }
    }
}

/// Handle amend_order: translate to Coinbase REST POST /orders/edit
async fn handle_amend_order(
    parsed: &serde_json::Value,
    state: &CoinbaseWsState,
    bot_write: &Arc<Mutex<BotWriter>>,
    order_id_map: &Arc<Mutex<std::collections::HashMap<String, String>>>,
) {
    let req_id = parsed.get("req_id").and_then(|r| r.as_u64()).unwrap_or(0);
    let params = match parsed.get("params") {
        Some(p) => p,
        None => {
            send_error(bot_write, "amend_order", req_id, "Missing params").await;
            return;
        }
    };

    let cl_ord_id = params.get("cl_ord_id").and_then(|c| c.as_str()).unwrap_or("");

    // Look up the Coinbase exchange order_id from our mapping
    let exchange_order_id = order_id_map.lock().await.get(cl_ord_id).cloned();
    let exchange_order_id = match exchange_order_id {
        Some(id) => id,
        None => {
            tracing::warn!(cl_ord_id, "No exchange order_id found for amend — rejecting");
            send_error(bot_write, "amend_order", req_id, "Unknown order (no order_id mapping)").await;
            return;
        }
    };

    let limit_price = params.get("limit_price").and_then(|p| p.as_f64());
    let order_qty = params.get("order_qty").and_then(|q| q.as_f64());

    // Rate limit check
    {
        let allowed = state.rate_limiter.lock().await.try_consume("amend_order");
        if !allowed {
            send_error(bot_write, "amend_order", req_id, "EOrder:Rate limit exceeded [proxy]").await;
            return;
        }
    }

    let mut body = serde_json::json!({
        "order_id": exchange_order_id
    });

    if let Some(price) = limit_price {
        body["price"] = serde_json::json!(format!("{}", price));
    }
    if let Some(qty) = order_qty {
        body["size"] = serde_json::json!(format!("{}", qty));
    }

    match state.rest_post("/api/v3/brokerage/orders/edit", &body).await {
        Ok(resp) => {
            let success = resp.get("success").and_then(|s| s.as_bool()).unwrap_or(false);
            if success {
                let response = translate::order_response_success("amend_order", req_id, &exchange_order_id, cl_ord_id);
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

/// Handle cancel_order: translate to Coinbase REST POST /orders/batch_cancel
async fn handle_cancel_order(
    parsed: &serde_json::Value,
    state: &CoinbaseWsState,
    bot_write: &Arc<Mutex<BotWriter>>,
    order_id_map: &Arc<Mutex<std::collections::HashMap<String, String>>>,
) {
    let req_id = parsed.get("req_id").and_then(|r| r.as_u64()).unwrap_or(0);
    let params = match parsed.get("params") {
        Some(p) => p,
        None => {
            send_error(bot_write, "cancel_order", req_id, "Missing params").await;
            return;
        }
    };

    let cl_ord_ids: Vec<String> = params
        .get("cl_ord_id")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    if cl_ord_ids.is_empty() {
        send_error(bot_write, "cancel_order", req_id, "No order IDs provided").await;
        return;
    }

    // Rate limit check
    {
        let allowed = state.rate_limiter.lock().await.try_consume("cancel_order");
        if !allowed {
            send_error(bot_write, "cancel_order", req_id, "EOrder:Rate limit exceeded [proxy]").await;
            return;
        }
    }

    // Translate cl_ord_ids to Coinbase exchange order_ids
    let map = order_id_map.lock().await;
    let mut exchange_ids: Vec<String> = Vec::new();
    for cl_id in &cl_ord_ids {
        match map.get(cl_id) {
            Some(exchange_id) => exchange_ids.push(exchange_id.clone()),
            None => tracing::warn!(cl_ord_id = cl_id.as_str(), "No exchange order_id for cancel — skipping"),
        }
    }
    drop(map);

    if exchange_ids.is_empty() {
        send_error(bot_write, "cancel_order", req_id, "No exchange order_ids found for given cl_ord_ids").await;
        return;
    }

    let body = serde_json::json!({
        "order_ids": exchange_ids
    });

    match state.rest_post("/api/v3/brokerage/orders/batch_cancel", &body).await {
        Ok(_resp) => {
            // Remove cancelled orders from the mapping
            let mut map = order_id_map.lock().await;
            for cl_id in &cl_ord_ids {
                map.remove(cl_id);
            }

            // Send success for the cancel
            let response = translate::order_response_success("cancel_order", req_id, "", "");
            let mut writer = bot_write.lock().await;
            let _ = writer.send(ws::Message::Text(response.into())).await;
        }
        Err(e) => {
            send_error(bot_write, "cancel_order", req_id, &format!("REST error: {}", e)).await;
        }
    }
}

/// Handle cancel_all: cancel all open orders via Coinbase REST
async fn handle_cancel_all(
    parsed: &serde_json::Value,
    state: &CoinbaseWsState,
    bot_write: &Arc<Mutex<BotWriter>>,
    order_id_map: &Arc<Mutex<std::collections::HashMap<String, String>>>,
) {
    let req_id = parsed.get("req_id").and_then(|r| r.as_u64()).unwrap_or(0);

    // First get all open orders, then cancel them
    let path = "/api/v3/brokerage/orders/historical/batch?order_status=OPEN&limit=250";
    let host = state.api_host();
    let jwt = match build_jwt(&state.api_key, &state.api_secret, "GET", host, path) {
        Ok(j) => j,
        Err(e) => {
            send_error(bot_write, "cancel_all", req_id, &format!("JWT error: {}", e)).await;
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
            send_error(bot_write, "cancel_all", req_id, &format!("REST error: {}", e)).await;
            return;
        }
    };

    let data: serde_json::Value = match resp.json().await {
        Ok(d) => d,
        Err(e) => {
            send_error(bot_write, "cancel_all", req_id, &format!("Parse error: {}", e)).await;
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

    tracing::info!(count = order_ids.len(), "cancel_all: found open orders to cancel");

    if !order_ids.is_empty() {
        let body = serde_json::json!({"order_ids": order_ids});
        match state.rest_post("/api/v3/brokerage/orders/batch_cancel", &body).await {
            Ok(_) => tracing::info!("cancel_all: batch cancel succeeded"),
            Err(e) => tracing::error!(error = %e, "cancel_all: batch cancel failed"),
        }
    }

    // Clear the order_id_map since all orders are cancelled
    {
        let mut map = order_id_map.lock().await;
        let count = map.len();
        map.clear();
        if count > 0 {
            tracing::debug!(cleared = count, "cancel_all: cleared order_id_map");
        }
    }

    let response = translate::order_response_success("cancel_all", req_id, "", "");
    let mut writer = bot_write.lock().await;
    let _ = writer.send(ws::Message::Text(response.into())).await;
}

/// Remove entries from order_id_map when orders reach terminal states (filled, cancelled, expired, failed).
async fn cleanup_terminal_orders(
    coinbase_msg: &serde_json::Value,
    order_id_map: &Arc<Mutex<std::collections::HashMap<String, String>>>,
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

    let (coinbase_write, mut coinbase_read) = coinbase_ws.split();
    let (bot_write, mut bot_read) = bot_ws.split();

    let bot_write = Arc::new(Mutex::new(bot_write));
    let coinbase_write = Arc::new(Mutex::new(coinbase_write));

    // Coinbase merges order books for USD/USDC pairs and always returns the
    // base-USD product_id in level2 data, even when subscribed via base-USDC.
    // Track the mapping so we can relabel book data with the correct symbol.
    // Key: Coinbase base product (e.g., "DOGE-USD"), Value: internal symbol the bot expects (e.g., "DOGE/USDC").
    let product_id_map: Arc<Mutex<std::collections::HashMap<String, String>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));

    // Bot -> Coinbase: translate subscribe messages
    let state_for_sub = state.clone();
    let coinbase_write_for_sub = coinbase_write.clone();
    let bot_write_for_sub = bot_write.clone();
    let product_id_map_for_sub = product_id_map.clone();
    let bot_to_coinbase = tokio::spawn(async move {
        while let Some(Ok(msg)) = bot_read.next().await {
            match msg {
                ws::Message::Text(text) => {
                    let text_str = text.to_string();
                    let parsed: serde_json::Value = match serde_json::from_str(&text_str) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    let method = parsed.get("method").and_then(|m| m.as_str()).unwrap_or("");

                    if method == "subscribe" {
                        // Extract the original symbols the bot requested (e.g., ["DOGE/USDC"])
                        let bot_symbols: Vec<String> = parsed
                            .get("params")
                            .and_then(|p| p.get("symbol"))
                            .and_then(|s| s.as_array())
                            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                            .unwrap_or_default();

                        if let Some(coinbase_sub) = translate::kraken_subscribe_to_coinbase(&parsed) {
                            // Add HMAC auth to subscription
                            let channel = coinbase_sub.get("channel").and_then(|c| c.as_str()).unwrap_or("");
                            let product_ids: Vec<String> = coinbase_sub
                                .get("product_ids")
                                .and_then(|p| p.as_array())
                                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                                .unwrap_or_default();

                            // Build mapping: Coinbase returns USD product_ids for USDC subscriptions.
                            // E.g., subscribe DOGE-USDC → Coinbase returns DOGE-USD in book data.
                            {
                                let mut map = product_id_map_for_sub.lock().await;
                                for (pid, sym) in product_ids.iter().zip(bot_symbols.iter()) {
                                    // The base USD product_id that Coinbase will return
                                    let base_pid = pid.replace("-USDC", "-USD");
                                    map.insert(base_pid, sym.clone());
                                    // Also map the exact product_id in case Coinbase uses it
                                    map.insert(pid.clone(), sym.clone());
                                }
                            }

                            let sub_msg = state_for_sub.ws_subscribe_msg(channel, &product_ids);
                            let mut writer = coinbase_write_for_sub.lock().await;
                            let _ = writer.send(Message::Text(sub_msg.into())).await;

                            // Send confirmation back to bot
                            let kraken_channel = parsed
                                .get("params")
                                .and_then(|p| p.get("channel"))
                                .and_then(|c| c.as_str())
                                .unwrap_or("book");
                            let confirm = translate::subscribe_confirmed(kraken_channel);
                            let mut bot_writer = bot_write_for_sub.lock().await;
                            let _ = bot_writer.send(ws::Message::Text(confirm.into())).await;
                        }
                    } else if method == "ping" {
                        let req_id = parsed.get("req_id").and_then(|r| r.as_u64()).unwrap_or(0);
                        let pong = translate::pong_response(req_id);
                        let mut writer = bot_write_for_sub.lock().await;
                        let _ = writer.send(ws::Message::Text(pong.into())).await;
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
        while let Some(Ok(msg)) = coinbase_read.next().await {
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
                            if let Some(kraken_msg) = translate::coinbase_book_to_kraken_mapped(&parsed, &map) {
                                let mut writer = bot_write_for_upstream.lock().await;
                                if writer
                                    .send(ws::Message::Text(kraken_msg.into()))
                                    .await
                                    .is_err()
                                {
                                    break;
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
    });

    tokio::select! {
        _ = bot_to_coinbase => {
            tracing::info!("Coinbase public WS proxy: bot disconnected");
        }
        _ = coinbase_to_bot => {
            tracing::info!("Coinbase public WS proxy: Coinbase WS disconnected");
        }
    }

    Ok(())
}
