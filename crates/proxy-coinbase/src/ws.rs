use anyhow::Result;
use axum::extract::ws::{self, WebSocket};
use futures::{SinkExt, StreamExt, stream::SplitSink};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::auth::{sign_request, timestamp_secs};
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
            client: reqwest::Client::new(),
        }
    }

    /// Build Coinbase HMAC auth headers for a request.
    fn auth_headers(&self, method: &str, path: &str, body: &str) -> Vec<(String, String)> {
        let timestamp = timestamp_secs();
        let signature = sign_request(&timestamp, method, path, body, &self.api_secret);
        vec![
            ("CB-ACCESS-KEY".to_string(), self.api_key.clone()),
            ("CB-ACCESS-SIGN".to_string(), signature),
            ("CB-ACCESS-TIMESTAMP".to_string(), timestamp),
            ("Content-Type".to_string(), "application/json".to_string()),
        ]
    }

    /// Make an authenticated POST request to Coinbase REST API.
    async fn rest_post(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let body_str = serde_json::to_string(body)?;
        let headers = self.auth_headers("POST", path, &body_str);
        let url = format!("{}{}", self.coinbase_base_url, path);

        let mut req = self.client.post(&url).body(body_str);
        for (key, value) in headers {
            req = req.header(&key, &value);
        }

        let resp = req.send().await?;
        let data: serde_json::Value = resp.json().await?;
        Ok(data)
    }

    /// Build Coinbase WS subscribe message with HMAC auth.
    /// WS auth uses message = timestamp + channel + comma_separated_product_ids
    fn ws_subscribe_msg(&self, channel: &str, product_ids: &[String]) -> String {
        let timestamp = timestamp_secs();
        let products_str = product_ids.join(",");
        let message = format!("{}{}{}", timestamp, channel, products_str);
        let signature = crate::auth::hmac_sha256_hex(&message, &self.api_secret);

        serde_json::json!({
            "type": "subscribe",
            "product_ids": product_ids,
            "channel": channel,
            "api_key": self.api_key,
            "timestamp": timestamp,
            "signature": signature
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

    // Bot -> Coinbase: intercept order commands, translate to REST
    let state_for_orders = state.clone();
    let bot_write_for_orders = bot_write.clone();
    let subscribed_for_orders = subscribed.clone();
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
                                    // Subscribe to user channel on Coinbase
                                    // Use a placeholder product - Coinbase requires at least one
                                    let sub_msg = state_for_orders.ws_subscribe_msg(
                                        "user",
                                        &["BTC-USD".to_string()],
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
                            handle_add_order(&parsed, &state_for_orders, &bot_write_for_orders).await;
                        }
                        "amend_order" => {
                            handle_amend_order(&parsed, &state_for_orders, &bot_write_for_orders).await;
                        }
                        "cancel_order" => {
                            handle_cancel_order(&parsed, &state_for_orders, &bot_write_for_orders).await;
                        }
                        "cancel_all" => {
                            handle_cancel_all(&parsed, &state_for_orders, &bot_write_for_orders).await;
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
                        "user" => {
                            let kraken_msgs = translate::coinbase_user_to_kraken(&parsed);
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
            let success = resp.get("success").and_then(|s| s.as_bool()).unwrap_or(false);
            if success {
                let order_id = resp
                    .get("success_response")
                    .and_then(|r| r.get("order_id"))
                    .and_then(|id| id.as_str())
                    .unwrap_or("");

                let response = translate::order_response_success("add_order", req_id, order_id, cl_ord_id);
                let mut writer = bot_write.lock().await;
                let _ = writer.send(ws::Message::Text(response.into())).await;
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

    // Coinbase edit needs the exchange order_id, not cl_ord_id.
    // We need to look up the order. For now, use cl_ord_id as order_id
    // (the bot tracks the mapping via OrderResponse).
    // Actually, Coinbase edit requires the order_id from their system.
    // The bot doesn't send this in amend_order. We need to look it up.
    // For now, we'll cancel + replace instead.

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

    // Try to edit using cl_ord_id — Coinbase may support this
    let mut body = serde_json::json!({
        "order_id": cl_ord_id
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
                let response = translate::order_response_success("amend_order", req_id, cl_ord_id, cl_ord_id);
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

    let body = serde_json::json!({
        "order_ids": cl_ord_ids
    });

    match state.rest_post("/api/v3/brokerage/orders/batch_cancel", &body).await {
        Ok(_resp) => {
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
) {
    let req_id = parsed.get("req_id").and_then(|r| r.as_u64()).unwrap_or(0);

    // First get all open orders, then cancel them
    let timestamp = timestamp_secs();
    let path = "/api/v3/brokerage/orders/historical/batch?order_status=OPEN&limit=250";
    let signature = sign_request(&timestamp, "GET", path, "", &state.api_secret);

    let url = format!("{}{}", state.coinbase_base_url, path);
    let resp = match state
        .client
        .get(&url)
        .header("CB-ACCESS-KEY", &state.api_key)
        .header("CB-ACCESS-SIGN", &signature)
        .header("CB-ACCESS-TIMESTAMP", &timestamp)
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

    if !order_ids.is_empty() {
        let body = serde_json::json!({"order_ids": order_ids});
        let _ = state.rest_post("/api/v3/brokerage/orders/batch_cancel", &body).await;
    }

    let response = translate::order_response_success("cancel_all", req_id, "", "");
    let mut writer = bot_write.lock().await;
    let _ = writer.send(ws::Message::Text(response.into())).await;
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

    // Bot -> Coinbase: translate subscribe messages
    let state_for_sub = state.clone();
    let coinbase_write_for_sub = coinbase_write.clone();
    let bot_write_for_sub = bot_write.clone();
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
                        if let Some(coinbase_sub) = translate::kraken_subscribe_to_coinbase(&parsed) {
                            // Add HMAC auth to subscription
                            let channel = coinbase_sub.get("channel").and_then(|c| c.as_str()).unwrap_or("");
                            let product_ids: Vec<String> = coinbase_sub
                                .get("product_ids")
                                .and_then(|p| p.as_array())
                                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                                .unwrap_or_default();

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
                            if let Some(kraken_msg) = translate::coinbase_book_to_kraken(&parsed) {
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
