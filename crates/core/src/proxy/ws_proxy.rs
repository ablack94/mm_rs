use anyhow::Result;
use axum::extract::ws::{self, WebSocket};
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::exchange::auth::sign_request;

/// State shared across WS proxy connections for token management.
pub struct WsProxyState {
    pub api_key: String,
    pub api_secret: String,
    pub kraken_ws_url: String,
    pub kraken_rest_url: String,
    /// Cached WS token and its fetch time.
    token_cache: Mutex<Option<(String, std::time::Instant)>>,
    client: reqwest::Client,
}

impl WsProxyState {
    pub fn new(
        api_key: String,
        api_secret: String,
        kraken_ws_url: String,
        kraken_rest_url: String,
    ) -> Self {
        Self {
            api_key,
            api_secret,
            kraken_ws_url,
            kraken_rest_url,
            token_cache: Mutex::new(None),
            client: reqwest::Client::new(),
        }
    }

    /// Get a valid WS token, refreshing if expired (tokens last ~15 min).
    pub async fn get_token(&self) -> Result<String> {
        let mut cache = self.token_cache.lock().await;
        if let Some((ref token, ref fetched_at)) = *cache {
            // Refresh if older than 10 minutes (tokens expire ~15 min)
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

/// Handle a single WebSocket proxy connection.
/// Bot connects to us, we connect upstream to Kraken, and relay bidirectionally.
/// We inject the real WS token into outgoing messages that contain a token field.
pub async fn handle_ws_connection(
    bot_ws: WebSocket,
    state: Arc<WsProxyState>,
) {
    let result = run_ws_proxy(bot_ws, state).await;
    if let Err(e) = result {
        tracing::error!(error = %e, "WS proxy connection error");
    }
}

async fn run_ws_proxy(
    bot_ws: WebSocket,
    state: Arc<WsProxyState>,
) -> Result<()> {
    // Get a fresh token for the upstream connection
    let token = state.get_token().await?;

    // Connect upstream to Kraken
    tracing::info!(url = state.kraken_ws_url, "WS proxy: connecting upstream");
    let (upstream_ws, _) = connect_async(&state.kraken_ws_url).await?;
    tracing::info!("WS proxy: upstream connected");

    let (mut upstream_write, mut upstream_read) = upstream_ws.split();
    let (mut bot_write, mut bot_read) = bot_ws.split();

    let token_for_inject = Arc::new(Mutex::new(token));
    let state_for_refresh = state.clone();
    let token_ref = token_for_inject.clone();

    // Spawn token refresh task (refresh every 10 minutes)
    let refresh_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(600));
        interval.tick().await; // skip first immediate tick
        loop {
            interval.tick().await;
            match state_for_refresh.get_token().await {
                Ok(new_token) => {
                    *token_ref.lock().await = new_token;
                    tracing::info!("WS proxy: token refreshed");
                }
                Err(e) => {
                    tracing::error!(error = %e, "WS proxy: token refresh failed");
                }
            }
        }
    });

    // Bot -> Kraken: inject token into messages
    let token_for_bot = token_for_inject.clone();
    let bot_to_upstream = tokio::spawn(async move {
        while let Some(Ok(msg)) = bot_read.next().await {
            match msg {
                ws::Message::Text(text) => {
                    let injected = inject_token(&text, &token_for_bot).await;
                    if upstream_write
                        .send(Message::Text(injected.into()))
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
    let upstream_to_bot = tokio::spawn(async move {
        while let Some(Ok(msg)) = upstream_read.next().await {
            match msg {
                Message::Text(text) => {
                    if bot_write
                        .send(ws::Message::Text(text.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Message::Ping(data) => {
                    if bot_write
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

    // Wait for either direction to close
    tokio::select! {
        _ = bot_to_upstream => {
            tracing::info!("WS proxy: bot disconnected");
        }
        _ = upstream_to_bot => {
            tracing::info!("WS proxy: upstream disconnected");
        }
    }

    refresh_handle.abort();
    Ok(())
}

/// Inject the real WS token into a JSON message if it has a "params.token" field.
async fn inject_token(text: &str, token: &Arc<Mutex<String>>) -> String {
    let mut v: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return text.to_string(),
    };

    if let Some(params) = v.get_mut("params") {
        if params.get("token").is_some() {
            let real_token = token.lock().await.clone();
            params["token"] = serde_json::Value::String(real_token);
        }
    }

    serde_json::to_string(&v).unwrap_or_else(|_| text.to_string())
}

fn millis_nonce() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .to_string()
}
