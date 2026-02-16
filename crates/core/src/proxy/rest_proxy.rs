use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::{get, post},
};
use serde_json::{json, Value};

use crate::exchange::auth::sign_request;

/// Shared state for the REST proxy.
pub struct ProxyState {
    pub api_key: String,
    pub api_secret: String,
    pub proxy_token: String,
    pub kraken_base_url: String,
    pub client: reqwest::Client,
}

type AppState = Arc<ProxyState>;

fn check_auth(headers: &HeaderMap, token: &str) -> Result<(), (StatusCode, Json<Value>)> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if auth == format!("Bearer {}", token) {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))))
    }
}

pub fn build_proxy_router(state: Arc<ProxyState>) -> Router {
    Router::new()
        // Private endpoints (proxy signs them)
        .route("/0/private/GetWebSocketsToken", post(proxy_private))
        .route("/0/private/Balance", post(proxy_private))
        .route("/0/private/TradesHistory", post(proxy_private))
        // Public endpoints (pass-through)
        .route("/0/public/AssetPairs", get(proxy_public))
        .route("/0/public/Ticker", get(proxy_public))
        .route("/0/public/OHLC", get(proxy_public))
        // Health check
        .route("/health", get(health))
        .with_state(state)
}

async fn health() -> (StatusCode, Json<Value>) {
    (StatusCode::OK, Json(json!({"status": "ok"})))
}

/// Proxy private (authenticated) endpoints to Kraken.
/// Adds API-Key + API-Sign headers.
async fn proxy_private(
    headers: HeaderMap,
    State(state): State<AppState>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    body: String,
) -> (StatusCode, Json<Value>) {
    if let Err(e) = check_auth(&headers, &state.proxy_token) {
        return e;
    }

    let urlpath = uri.path();
    let nonce = millis_nonce();

    // Build the POST body: always include nonce, append any extra params
    let post_data = if body.is_empty() {
        format!("nonce={}", nonce)
    } else {
        format!("nonce={}&{}", nonce, body)
    };

    let signature = match sign_request(urlpath, &nonce, &post_data, &state.api_secret) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Signing failed: {}", e)})),
            );
        }
    };

    let url = format!("{}{}", state.kraken_base_url, urlpath);
    match state
        .client
        .post(&url)
        .header("API-Key", &state.api_key)
        .header("API-Sign", &signature)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(post_data)
        .send()
        .await
    {
        Ok(resp) => match resp.json::<Value>().await {
            Ok(data) => (StatusCode::OK, Json(data)),
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": e.to_string()})),
            ),
        },
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": e.to_string()})),
        ),
    }
}

/// Proxy public (unauthenticated) endpoints to Kraken.
async fn proxy_public(
    headers: HeaderMap,
    State(state): State<AppState>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
) -> (StatusCode, Json<Value>) {
    if let Err(e) = check_auth(&headers, &state.proxy_token) {
        return e;
    }

    let url = format!(
        "{}{}",
        state.kraken_base_url,
        uri.path_and_query().map(|pq| pq.as_str()).unwrap_or(uri.path())
    );

    match state.client.get(&url).send().await {
        Ok(resp) => match resp.json::<Value>().await {
            Ok(data) => (StatusCode::OK, Json(data)),
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": e.to_string()})),
            ),
        },
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": e.to_string()})),
        ),
    }
}

fn millis_nonce() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .to_string()
}
