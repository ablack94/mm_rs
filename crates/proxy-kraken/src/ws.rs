use anyhow::Result;
use axum::extract::ws::{self, WebSocket};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::auth::sign_request;
use proxy_common::rate_limit::{TokenBucket, TokenBucketConfig};

/// State shared across WS proxy connections for Kraken token management.
pub struct KrakenWsState {
    pub api_key: String,
    pub api_secret: String,
    pub kraken_ws_url: String,
    pub kraken_rest_url: String,
    /// Cached WS token and its fetch time.
    token_cache: Mutex<Option<(String, std::time::Instant)>>,
    client: reqwest::Client,
    /// Shared rate limiter (per-API-key, shared across all WS connections).
    rate_limiter: Mutex<TokenBucket>,
}

impl KrakenWsState {
    pub fn new(
        api_key: String,
        api_secret: String,
        kraken_ws_url: String,
        kraken_rest_url: String,
        rate_limit_config: TokenBucketConfig,
    ) -> Self {
        Self {
            api_key,
            api_secret,
            kraken_ws_url,
            kraken_rest_url,
            token_cache: Mutex::new(None),
            client: reqwest::Client::new(),
            rate_limiter: Mutex::new(TokenBucket::new(rate_limit_config)),
        }
    }

    /// Get a valid WS token, refreshing if expired (tokens last ~15 min).
    pub async fn get_token(&self) -> Result<String> {
        let mut cache = self.token_cache.lock().await;
        if let Some((ref token, ref fetched_at)) = *cache {
            if fetched_at.elapsed().as_secs() < 600 {
                return Ok(token.clone());
            }
        }

        let token = self.fetch_ws_token().await?;
        *cache = Some((token.clone(), std::time::Instant::now()));
        Ok(token)
    }

    async fn fetch_ws_token(&self) -> Result<String> {
        let urlpath = "/0/private/GetWebSocketsToken";
        let nonce = millis_nonce();
        let post_data = format!("nonce={}", nonce);
        let signature = sign_request(urlpath, &nonce, &post_data, &self.api_secret)?;

        let resp: serde_json::Value = self
            .client
            .post(format!("{}{}", self.kraken_rest_url, urlpath))
            .header("API-Key", &self.api_key)
            .header("API-Sign", &signature)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(post_data)
            .send()
            .await?
            .json()
            .await?;

        resp["result"]["token"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("No token in WS token response"))
    }
}

/// Handle a single private WebSocket proxy connection.
pub async fn handle_kraken_private_ws(
    bot_ws: WebSocket,
    state: Arc<KrakenWsState>,
) {
    let result = run_ws_proxy(bot_ws, state).await;
    if let Err(e) = result {
        tracing::error!(error = %e, "Kraken WS proxy connection error");
    }
}

async fn run_ws_proxy(
    bot_ws: WebSocket,
    state: Arc<KrakenWsState>,
) -> Result<()> {
    let token = state.get_token().await?;

    tracing::info!(url = state.kraken_ws_url, "Kraken WS proxy: connecting upstream");
    let (upstream_ws, _) = connect_async(&state.kraken_ws_url).await?;
    tracing::info!("Kraken WS proxy: upstream connected");

    let (mut upstream_write, mut upstream_read) = upstream_ws.split();
    let (bot_write, mut bot_read) = bot_ws.split();

    let token_for_inject = Arc::new(Mutex::new(token));
    let state_for_refresh = state.clone();
    let token_ref = token_for_inject.clone();

    let bot_write = Arc::new(Mutex::new(bot_write));

    // Token refresh task (every 10 minutes)
    let refresh_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(600));
        interval.tick().await; // skip first immediate tick
        loop {
            interval.tick().await;
            match state_for_refresh.get_token().await {
                Ok(new_token) => {
                    *token_ref.lock().await = new_token;
                    tracing::info!("Kraken WS proxy: token refreshed");
                }
                Err(e) => {
                    tracing::error!(error = %e, "Kraken WS proxy: token refresh failed");
                }
            }
        }
    });

    // Bot -> Kraken: translate ProxyCommand to Kraken WS v2, rate limit, inject token
    let token_for_bot = token_for_inject.clone();
    let state_for_rl = state.clone();
    let bot_write_for_reject = bot_write.clone();
    let bot_to_upstream = tokio::spawn(async move {
        while let Some(Ok(msg)) = bot_read.next().await {
            match msg {
                ws::Message::Text(text) => {
                    let text_str = text.to_string();
                    let parsed: Value = match serde_json::from_str(&text_str) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    // Determine command name (new "cmd" or legacy "method")
                    let cmd = parsed.get("cmd")
                        .or_else(|| parsed.get("method"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("");
                    let priority = parsed.get("priority")
                        .or_else(|| parsed.get("_priority"))
                        .and_then(|p| p.as_str())
                        .unwrap_or("normal");
                    let req_id = parsed.get("req_id").and_then(|r| r.as_u64()).unwrap_or(0);

                    // Rate limiting for order operations
                    let kraken_method = match cmd {
                        "place_order" | "add_order" => "add_order",
                        "amend_order" => "amend_order",
                        "cancel_orders" | "cancel_order" => "cancel_order",
                        _ => "",
                    };
                    let is_order_cmd = !kraken_method.is_empty();
                    let is_urgent = priority == "urgent";

                    if is_order_cmd && !is_urgent {
                        let allowed = state_for_rl.rate_limiter.lock().await.try_consume(kraken_method);
                        if !allowed {
                            let remaining = state_for_rl.rate_limiter.lock().await.tokens_remaining();
                            tracing::warn!(
                                cmd,
                                req_id,
                                tokens_remaining = format!("{:.1}", remaining),
                                "Rate limited by proxy"
                            );
                            // Send rejection in ProxyEvent format
                            let cl_ord_id = parsed.get("cl_ord_id")
                                .or_else(|| parsed.get("params").and_then(|p| p.get("cl_ord_id")))
                                .and_then(|s| s.as_str())
                                .unwrap_or("");
                            let rejection = json!({
                                "event": "order_rejected",
                                "req_id": req_id,
                                "cl_ord_id": cl_ord_id,
                                "error": "EOrder:Rate limit exceeded [proxy]"
                            });
                            let mut writer = bot_write_for_reject.lock().await;
                            let _ = writer
                                .send(ws::Message::Text(rejection.to_string().into()))
                                .await;
                            continue;
                        }
                    }

                    // Translate ProxyCommand to Kraken WS v2 format
                    let token = token_for_bot.lock().await.clone();
                    let kraken_msg = proxy_cmd_to_kraken(&parsed, cmd, &token);
                    if upstream_write
                        .send(Message::Text(kraken_msg.into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                ws::Message::Close(_) => break,
                _ => {}
            }
        }
    });

    // Kraken -> Bot: translate Kraken WS v2 to ProxyEvent
    let bot_write_for_upstream = bot_write.clone();
    let upstream_to_bot = tokio::spawn(async move {
        while let Some(Ok(msg)) = upstream_read.next().await {
            match msg {
                Message::Text(text) => {
                    let text_str = text.to_string();
                    let events = kraken_to_proxy_events(&text_str);
                    let mut writer = bot_write_for_upstream.lock().await;
                    for event_json in events {
                        if writer
                            .send(ws::Message::Text(event_json.into()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                Message::Ping(data) => {
                    let mut writer = bot_write_for_upstream.lock().await;
                    if writer
                        .send(ws::Message::Ping(data.into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    tokio::select! {
        _ = bot_to_upstream => {
            tracing::info!("Kraken WS proxy: bot disconnected");
        }
        _ = upstream_to_bot => {
            tracing::info!("Kraken WS proxy: upstream disconnected");
        }
    }

    refresh_handle.abort();
    Ok(())
}

// ---------------------------------------------------------------------------
// ProxyCommand -> Kraken WS v2 translation
// ---------------------------------------------------------------------------

fn proxy_cmd_to_kraken(parsed: &Value, cmd: &str, token: &str) -> String {
    match cmd {
        "place_order" | "add_order" => {
            let params = parsed.get("params").unwrap_or(parsed);
            let symbol = params.get("symbol").or_else(|| parsed.get("symbol"))
                .and_then(|s| s.as_str()).unwrap_or("");
            let side = params.get("side").or_else(|| parsed.get("side"))
                .and_then(|s| s.as_str()).unwrap_or("buy");
            let order_type = params.get("order_type").or_else(|| parsed.get("order_type"))
                .and_then(|s| s.as_str()).unwrap_or("limit");
            let price = params.get("price").or_else(|| params.get("limit_price"))
                .or_else(|| parsed.get("price"))
                .and_then(|p| p.as_f64()).unwrap_or(0.0);
            let qty = params.get("qty").or_else(|| params.get("order_qty"))
                .or_else(|| parsed.get("qty"))
                .and_then(|q| q.as_f64()).unwrap_or(0.0);
            let cl_ord_id = params.get("cl_ord_id").or_else(|| parsed.get("cl_ord_id"))
                .and_then(|s| s.as_str()).unwrap_or("");
            let post_only = params.get("post_only").or_else(|| parsed.get("post_only"))
                .and_then(|b| b.as_bool()).unwrap_or(false);
            let req_id = parsed.get("req_id").and_then(|r| r.as_u64()).unwrap_or(0);

            let mut kraken = json!({
                "method": "add_order",
                "req_id": req_id,
                "params": {
                    "symbol": symbol,
                    "side": side,
                    "order_type": order_type,
                    "limit_price": price,
                    "order_qty": qty,
                    "cl_ord_id": cl_ord_id,
                    "post_only": post_only,
                    "token": token,
                }
            });

            // For market orders, use time_in_force=ioc and remove limit_price
            let is_market = parsed.get("market").and_then(|m| m.as_bool()).unwrap_or(false);
            if is_market || order_type == "market" {
                kraken["params"]["order_type"] = json!("market");
                kraken["params"].as_object_mut().unwrap().remove("limit_price");
                kraken["params"]["time_in_force"] = json!("ioc");
            }

            kraken.to_string()
        }
        "amend_order" => {
            let params = parsed.get("params").unwrap_or(parsed);
            let cl_ord_id = params.get("cl_ord_id").or_else(|| parsed.get("cl_ord_id"))
                .and_then(|s| s.as_str()).unwrap_or("");
            let req_id = parsed.get("req_id").and_then(|r| r.as_u64()).unwrap_or(0);

            let mut kraken_params = json!({
                "cl_ord_id": cl_ord_id,
                "token": token,
            });
            if let Some(price) = params.get("price").or_else(|| params.get("limit_price"))
                .or_else(|| parsed.get("price"))
                .and_then(|p| p.as_f64()) {
                kraken_params["limit_price"] = json!(price);
            }
            if let Some(qty) = params.get("qty").or_else(|| params.get("order_qty"))
                .or_else(|| parsed.get("qty"))
                .and_then(|q| q.as_f64()) {
                kraken_params["order_qty"] = json!(qty);
            }

            json!({
                "method": "amend_order",
                "req_id": req_id,
                "params": kraken_params,
            }).to_string()
        }
        "cancel_orders" | "cancel_order" => {
            let params = parsed.get("params").unwrap_or(parsed);
            let req_id = parsed.get("req_id").and_then(|r| r.as_u64()).unwrap_or(0);

            // Get cl_ord_ids from new or old format
            let cl_ord_ids: Vec<String> = params.get("cl_ord_ids")
                .or_else(|| parsed.get("cl_ord_ids"))
                .or_else(|| params.get("cl_ord_id"))
                .and_then(|ids| ids.as_array())
                .map(|arr| arr.iter().filter_map(|s| s.as_str().map(String::from)).collect())
                .unwrap_or_default();

            json!({
                "method": "cancel_order",
                "req_id": req_id,
                "params": {
                    "cl_ord_id": cl_ord_ids,
                    "token": token,
                }
            }).to_string()
        }
        "cancel_all" => {
            let req_id = parsed.get("req_id").and_then(|r| r.as_u64()).unwrap_or(0);
            json!({
                "method": "cancel_all",
                "req_id": req_id,
                "params": { "token": token }
            }).to_string()
        }
        "set_dms" | "cancel_all_orders_after" => {
            let req_id = parsed.get("req_id").and_then(|r| r.as_u64()).unwrap_or(0);
            let params = parsed.get("params").unwrap_or(parsed);
            let timeout = params.get("timeout_secs").or_else(|| params.get("timeout"))
                .or_else(|| parsed.get("timeout_secs"))
                .and_then(|t| t.as_u64()).unwrap_or(60);
            json!({
                "method": "cancel_all_orders_after",
                "req_id": req_id,
                "params": {
                    "timeout": timeout,
                    "token": token,
                }
            }).to_string()
        }
        "subscribe" => {
            let params = parsed.get("params").unwrap_or(parsed);
            let channel = params.get("channel").or_else(|| parsed.get("channel"))
                .and_then(|c| c.as_str()).unwrap_or("");

            if channel == "executions" {
                json!({
                    "method": "subscribe",
                    "params": {
                        "channel": "executions",
                        "snap_orders": false,
                        "snap_trades": true,
                        "ratecounter": false,
                        "token": token,
                    }
                }).to_string()
            } else {
                // Book or other subscription — forward with symbol translation
                let symbols: Vec<String> = params.get("symbols")
                    .or_else(|| parsed.get("symbols"))
                    .or_else(|| params.get("symbol"))
                    .and_then(|s| s.as_array())
                    .map(|arr| arr.iter().filter_map(|s| s.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let depth = params.get("depth").or_else(|| parsed.get("depth"))
                    .and_then(|d| d.as_u64()).unwrap_or(10);

                json!({
                    "method": "subscribe",
                    "params": {
                        "channel": channel,
                        "symbol": symbols,
                        "depth": depth,
                    }
                }).to_string()
            }
        }
        "ping" => {
            let req_id = parsed.get("req_id").and_then(|r| r.as_u64()).unwrap_or(0);
            json!({
                "method": "ping",
                "req_id": req_id,
            }).to_string()
        }
        _ => {
            // Unknown — forward as-is with token injection
            let mut v = parsed.clone();
            if let Some(params) = v.get_mut("params") {
                if params.get("token").is_some() {
                    params["token"] = Value::String(token.to_string());
                }
            }
            v.to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// Kraken WS v2 -> ProxyEvent translation
// ---------------------------------------------------------------------------

/// Translate a Kraken WS v2 message into zero or more ProxyEvent JSON strings.
fn kraken_to_proxy_events(raw: &str) -> Vec<String> {
    let v: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let channel = v.get("channel").and_then(|c| c.as_str()).unwrap_or("");
    let msg_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");

    // Book data
    if channel == "book" {
        return translate_book(&v, msg_type);
    }

    // Execution data
    if channel == "executions" {
        return translate_executions(&v, msg_type);
    }

    // Heartbeat
    if channel == "heartbeat" {
        return vec![json!({"event": "heartbeat"}).to_string()];
    }

    // Status channel — drop
    if channel == "status" {
        return vec![];
    }

    // Subscribe confirmation
    if method == "subscribe" && v.get("success").and_then(|s| s.as_bool()).unwrap_or(false) {
        let result_channel = v["result"]["channel"].as_str().unwrap_or("");
        return vec![json!({"event": "subscribed", "channel": result_channel}).to_string()];
    }

    // Pong
    if method == "pong" {
        let req_id = v.get("req_id").and_then(|r| r.as_u64()).unwrap_or(0);
        return vec![json!({"event": "pong", "req_id": req_id}).to_string()];
    }

    // Order method responses (add_order, amend_order, cancel_order, cancel_all, cancel_all_orders_after)
    if !method.is_empty() && method != "subscribe" && method != "pong" {
        return translate_method_response(&v, method);
    }

    vec![]
}

fn translate_book(v: &Value, msg_type: &str) -> Vec<String> {
    let data = match v["data"].as_array() {
        Some(arr) if !arr.is_empty() => &arr[0],
        _ => return vec![],
    };

    let symbol = data["symbol"].as_str().unwrap_or("");
    let bids = data.get("bids").cloned().unwrap_or(json!([]));
    let asks = data.get("asks").cloned().unwrap_or(json!([]));

    let event_type = match msg_type {
        "snapshot" => "book_snapshot",
        "update" => "book_update",
        _ => return vec![],
    };

    vec![json!({
        "event": event_type,
        "symbol": symbol,
        "bids": bids,
        "asks": asks,
    }).to_string()]
}

fn translate_executions(v: &Value, msg_type: &str) -> Vec<String> {
    let data = match v["data"].as_array() {
        Some(arr) => arr,
        None => return vec![],
    };

    let mut events = Vec::new();

    for report in data {
        let exec_type = report["exec_type"].as_str().unwrap_or("");

        match exec_type {
            "trade" => {
                let order_status = report["order_status"].as_str().unwrap_or("");
                let event = json!({
                    "event": "fill",
                    "order_id": report["order_id"],
                    "cl_ord_id": report["cl_ord_id"],
                    "symbol": report["symbol"],
                    "side": report["side"],
                    "qty": report["last_qty"],
                    "price": report["last_price"],
                    "fee": report.get("fee").or_else(|| {
                        // Kraken puts fee in fees[0].qty
                        report["fees"].as_array()
                            .and_then(|f| f.first())
                            .and_then(|f| f.get("qty"))
                    }).unwrap_or(&json!(0)),
                    "is_maker": report.get("liquidity_ind")
                        .and_then(|l| l.as_str())
                        .map(|l| l == "m")
                        .unwrap_or(false),
                    "timestamp": report["timestamp"],
                    "is_fully_filled": order_status == "filled",
                });
                events.push(event.to_string());
            }
            "new" | "pending_new" => {
                // Only emit if this is an update (not snapshot — snapshot "new" is just resting orders)
                if msg_type == "update" {
                    let event = json!({
                        "event": "order_accepted",
                        "req_id": 0,
                        "cl_ord_id": report["cl_ord_id"],
                        "order_id": report["order_id"],
                    });
                    events.push(event.to_string());
                }
            }
            "canceled" | "expired" => {
                let reason = if exec_type == "expired" { "expired" } else { "user_requested" };
                let event = json!({
                    "event": "order_cancelled",
                    "cl_ord_id": report["cl_ord_id"],
                    "reason": reason,
                    "symbol": report["symbol"],
                });
                events.push(event.to_string());
            }
            _ => {}
        }
    }

    events
}

fn translate_method_response(v: &Value, method: &str) -> Vec<String> {
    let success = v.get("success").and_then(|s| s.as_bool()).unwrap_or(false);
    let req_id = v.get("req_id").and_then(|r| r.as_u64()).unwrap_or(0);

    if !success {
        let error = v.get("error").and_then(|e| e.as_str()).unwrap_or("Unknown error");
        let cl_ord_id = v["result"]["cl_ord_id"].as_str()
            .or_else(|| v.get("cl_ord_id").and_then(|s| s.as_str()))
            .unwrap_or("");
        return vec![json!({
            "event": "order_rejected",
            "req_id": req_id,
            "cl_ord_id": cl_ord_id,
            "error": error,
        }).to_string()];
    }

    match method {
        "add_order" => {
            let cl_ord_id = v["result"]["cl_ord_id"].as_str().unwrap_or("");
            let order_id = v["result"]["order_id"].as_str().unwrap_or("");
            vec![json!({
                "event": "order_accepted",
                "req_id": req_id,
                "cl_ord_id": cl_ord_id,
                "order_id": order_id,
            }).to_string()]
        }
        "amend_order" => {
            vec![json!({
                "event": "command_ack",
                "req_id": req_id,
                "cmd": "amend_order",
            }).to_string()]
        }
        "cancel_order" => {
            vec![json!({
                "event": "command_ack",
                "req_id": req_id,
                "cmd": "cancel_orders",
            }).to_string()]
        }
        "cancel_all" => {
            vec![json!({
                "event": "command_ack",
                "req_id": req_id,
                "cmd": "cancel_all",
            }).to_string()]
        }
        "cancel_all_orders_after" => {
            vec![json!({
                "event": "command_ack",
                "req_id": req_id,
                "cmd": "set_dms",
            }).to_string()]
        }
        _ => vec![],
    }
}

/// Handle a public WebSocket proxy connection.
/// Translates ProxyCommand subscribe to Kraken format (client→upstream)
/// and Kraken book data to ProxyEvent format (upstream→client).
pub async fn handle_kraken_public_ws(bot_ws: WebSocket, upstream_url: String) {
    let result = run_public_ws_proxy(bot_ws, &upstream_url).await;
    if let Err(e) = result {
        tracing::error!(error = %e, "Kraken public WS proxy connection error");
    }
}

async fn run_public_ws_proxy(bot_ws: WebSocket, upstream_url: &str) -> Result<()> {
    tracing::info!(url = upstream_url, "Kraken public WS proxy: connecting upstream");
    let (upstream_ws, _) = connect_async(upstream_url).await?;
    tracing::info!("Kraken public WS proxy: upstream connected");

    let (mut upstream_write, mut upstream_read) = upstream_ws.split();
    let (mut bot_write, mut bot_read) = bot_ws.split();

    // Client -> Upstream: translate ProxyCommand subscribe to Kraken format
    let client_to_upstream = tokio::spawn(async move {
        while let Some(Ok(msg)) = bot_read.next().await {
            if let ws::Message::Text(text) = msg {
                let text_str = text.to_string();
                if let Ok(parsed) = serde_json::from_str::<Value>(&text_str) {
                    let cmd = parsed.get("cmd")
                        .or_else(|| parsed.get("method"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("");

                    if cmd == "subscribe" {
                        let kraken_msg = proxy_cmd_to_kraken(&parsed, "subscribe", "");
                        if upstream_write.send(Message::Text(kraken_msg.into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });

    // Upstream -> Client: translate Kraken book data to ProxyEvent
    let upstream_to_client = tokio::spawn(async move {
        while let Some(Ok(msg)) = upstream_read.next().await {
            match msg {
                Message::Text(text) => {
                    let text_str = text.to_string();
                    let events = kraken_to_proxy_events(&text_str);
                    for event_json in events {
                        if bot_write
                            .send(ws::Message::Text(event_json.into()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                Message::Ping(data) => {
                    if bot_write.send(ws::Message::Ping(data.into())).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    tokio::select! {
        _ = client_to_upstream => {
            tracing::info!("Kraken public WS proxy: client disconnected");
        }
        _ = upstream_to_client => {
            tracing::info!("Kraken public WS proxy: upstream disconnected");
        }
    }

    Ok(())
}

fn millis_nonce() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .to_string()
}
