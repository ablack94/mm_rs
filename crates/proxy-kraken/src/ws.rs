use anyhow::Result;
use axum::extract::ws::{self, WebSocket};
use futures::{SinkExt, StreamExt};
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

    // Bot -> Kraken: rate limiting + token injection
    let token_for_bot = token_for_inject.clone();
    let state_for_rl = state.clone();
    let bot_write_for_reject = bot_write.clone();
    let bot_to_upstream = tokio::spawn(async move {
        while let Some(Ok(msg)) = bot_read.next().await {
            match msg {
                ws::Message::Text(text) => {
                    let text_str = text.to_string();
                    let parsed: serde_json::Value = match serde_json::from_str(&text_str) {
                        Ok(v) => v,
                        Err(_) => {
                            if upstream_write
                                .send(Message::Text(text_str.into()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                            continue;
                        }
                    };

                    let method = parsed.get("method").and_then(|m| m.as_str()).unwrap_or("");
                    let priority = parsed.get("_priority").and_then(|p| p.as_str()).unwrap_or("");
                    let req_id = parsed.get("req_id").and_then(|r| r.as_u64()).unwrap_or(0);

                    let is_exempt = matches!(method,
                        "cancel_all" | "cancel_all_orders_after" | "subscribe" | "ping" | ""
                    ) || !matches!(method, "add_order" | "amend_order" | "cancel_order");

                    let is_urgent = priority == "urgent";

                    if !is_exempt && !is_urgent {
                        let allowed = state_for_rl.rate_limiter.lock().await.try_consume(method);
                        if !allowed {
                            let remaining = state_for_rl.rate_limiter.lock().await.tokens_remaining();
                            tracing::warn!(
                                method,
                                req_id,
                                tokens_remaining = format!("{:.1}", remaining),
                                "Rate limited by proxy"
                            );
                            let rejection = serde_json::json!({
                                "method": method,
                                "req_id": req_id,
                                "success": false,
                                "error": "EOrder:Rate limit exceeded [proxy]"
                            });
                            let reject_text = rejection.to_string();
                            let mut writer = bot_write_for_reject.lock().await;
                            let _ = writer
                                .send(ws::Message::Text(reject_text.into()))
                                .await;
                            continue;
                        }
                    }

                    let outgoing = prepare_outgoing(&parsed, &token_for_bot).await;
                    if upstream_write
                        .send(Message::Text(outgoing.into()))
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

    // Kraken -> Bot: forward as-is
    let bot_write_for_upstream = bot_write.clone();
    let upstream_to_bot = tokio::spawn(async move {
        while let Some(Ok(msg)) = upstream_read.next().await {
            match msg {
                Message::Text(text) => {
                    let mut writer = bot_write_for_upstream.lock().await;
                    if writer
                        .send(ws::Message::Text(text.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
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

/// Strip `_priority` and inject the real Kraken WS token into `params.token`.
async fn prepare_outgoing(v: &serde_json::Value, token: &Arc<Mutex<String>>) -> String {
    let mut v = v.clone();

    if let Some(obj) = v.as_object_mut() {
        obj.remove("_priority");
    }

    if let Some(params) = v.get_mut("params") {
        if params.get("token").is_some() {
            let real_token = token.lock().await.clone();
            params["token"] = serde_json::Value::String(real_token);
        }
    }

    serde_json::to_string(&v).unwrap_or_default()
}

/// Handle a public WebSocket proxy connection (pure relay, no token injection).
pub async fn handle_kraken_public_ws(bot_ws: WebSocket, upstream_url: String) {
    let result = proxy_common::ws_relay::relay_public_ws(bot_ws, upstream_url).await;
    if let Err(e) = result {
        tracing::error!(error = %e, "Kraken public WS proxy connection error");
    }
}

fn millis_nonce() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .to_string()
}
