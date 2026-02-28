use anyhow::Result;
use axum::{
    extract::{
        ws::{Message, WebSocket},
        Path, Query, State, WebSocketUpgrade,
    },
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::{collections::HashMap, sync::Arc};
use tokio_tungstenite::tungstenite;
use tower_http::cors::CorsLayer;

const INDEX_HTML: &str = include_str!("static/index.html");

#[derive(Clone)]
struct AppState {
    state_store_url: String,
    state_store_token: String,
    pnl_analyzer_url: String,
    pnl_api_token: String,
    http: reqwest::Client,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dashboard=info".into()),
        )
        .init();

    let state_store_url =
        std::env::var("STATE_STORE_URL").unwrap_or_else(|_| "http://localhost:3040".into());
    let state_store_token = std::env::var("STATE_STORE_TOKEN").unwrap_or_default();
    let pnl_analyzer_url =
        std::env::var("PNL_ANALYZER_URL").unwrap_or_else(|_| "http://localhost:3031".into());
    let pnl_api_token = std::env::var("PNL_API_TOKEN").unwrap_or_default();
    let port: u16 = std::env::var("DASHBOARD_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3050);

    let state = Arc::new(AppState {
        state_store_url,
        state_store_token,
        pnl_analyzer_url,
        pnl_api_token,
        http: reqwest::Client::new(),
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/health", get(proxy_ss_health))
        .route("/api/pairs", get(proxy_ss_pairs))
        .route("/api/pairs/{symbol}", get(proxy_ss_pair).patch(proxy_ss_patch_pair).put(proxy_ss_put_pair).delete(proxy_ss_delete_pair))
        .route("/api/defaults", get(proxy_ss_defaults).patch(proxy_ss_patch_defaults))
        .route("/api/pnl", get(proxy_pnl))
        .route("/api/pnl/pairs", get(proxy_pnl_pairs))
        .route("/api/pnl/pairs/{symbol}", get(proxy_pnl_pair))
        .route("/api/pnl/trades", get(proxy_pnl_trades))
        .route("/ws", get(ws_handler))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    tracing::info!("Dashboard listening on http://0.0.0.0:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

// --- State Store proxies ---

async fn proxy_ss_health(State(s): State<Arc<AppState>>) -> Response {
    proxy_get(&s, &format!("{}/health", s.state_store_url), &s.state_store_token).await
}

async fn proxy_ss_pairs(
    State(s): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let mut url = format!("{}/pairs", s.state_store_url);
    if let Some(state_filter) = params.get("state") {
        url = format!("{}?state={}", url, state_filter);
    }
    proxy_get(&s, &url, &s.state_store_token).await
}

async fn proxy_ss_pair(
    State(s): State<Arc<AppState>>,
    Path(symbol): Path<String>,
) -> Response {
    proxy_get(
        &s,
        &format!("{}/pairs/{}", s.state_store_url, symbol),
        &s.state_store_token,
    )
    .await
}

async fn proxy_ss_patch_pair(
    State(s): State<Arc<AppState>>,
    Path(symbol): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    proxy_patch(
        &s,
        &format!("{}/pairs/{}", s.state_store_url, symbol),
        &s.state_store_token,
        body,
    )
    .await
}

async fn proxy_ss_put_pair(
    State(s): State<Arc<AppState>>,
    Path(symbol): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    proxy_put(
        &s,
        &format!("{}/pairs/{}", s.state_store_url, symbol),
        &s.state_store_token,
        body,
    )
    .await
}

async fn proxy_ss_delete_pair(
    State(s): State<Arc<AppState>>,
    Path(symbol): Path<String>,
) -> Response {
    proxy_delete(
        &s,
        &format!("{}/pairs/{}", s.state_store_url, symbol),
        &s.state_store_token,
    )
    .await
}

async fn proxy_ss_defaults(State(s): State<Arc<AppState>>) -> Response {
    proxy_get(&s, &format!("{}/defaults", s.state_store_url), &s.state_store_token).await
}

async fn proxy_ss_patch_defaults(
    State(s): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    proxy_patch(
        &s,
        &format!("{}/defaults", s.state_store_url),
        &s.state_store_token,
        body,
    )
    .await
}

// --- PnL Analyzer proxies ---

async fn proxy_pnl(State(s): State<Arc<AppState>>) -> Response {
    proxy_get(&s, &format!("{}/pnl", s.pnl_analyzer_url), &s.pnl_api_token).await
}

async fn proxy_pnl_pairs(State(s): State<Arc<AppState>>) -> Response {
    proxy_get(&s, &format!("{}/pairs", s.pnl_analyzer_url), &s.pnl_api_token).await
}

async fn proxy_pnl_pair(
    State(s): State<Arc<AppState>>,
    Path(symbol): Path<String>,
) -> Response {
    proxy_get(
        &s,
        &format!("{}/pairs/{}", s.pnl_analyzer_url, symbol),
        &s.pnl_api_token,
    )
    .await
}

#[derive(Deserialize)]
struct TradesQuery {
    limit: Option<u32>,
}

async fn proxy_pnl_trades(
    State(s): State<Arc<AppState>>,
    Query(q): Query<TradesQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(100);
    proxy_get(
        &s,
        &format!("{}/trades?limit={}", s.pnl_analyzer_url, limit),
        &s.pnl_api_token,
    )
    .await
}

// --- WebSocket relay to State Store ---

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(s): State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(move |socket| ws_relay(socket, s))
}

async fn ws_relay(browser_ws: WebSocket, state: Arc<AppState>) {
    let ws_url = state
        .state_store_url
        .replace("http://", "ws://")
        .replace("https://", "wss://");
    let url = if state.state_store_token.is_empty() {
        format!("{}/ws", ws_url)
    } else {
        format!("{}/ws?token={}", ws_url, state.state_store_token)
    };

    let upstream = match tokio_tungstenite::connect_async(&url).await {
        Ok((ws, _)) => ws,
        Err(e) => {
            tracing::error!("Failed to connect to state store WS: {e}");
            return;
        }
    };

    let (mut browser_tx, mut browser_rx) = browser_ws.split();
    let (mut upstream_tx, mut upstream_rx) = upstream.split();

    // Relay: upstream (state store) → browser
    let to_browser = tokio::spawn(async move {
        while let Some(Ok(msg)) = upstream_rx.next().await {
            let axum_msg = match msg {
                tungstenite::Message::Text(t) => Some(Message::Text(t.to_string().into())),
                tungstenite::Message::Binary(b) => Some(Message::Binary(b.into())),
                tungstenite::Message::Close(_) => break,
                tungstenite::Message::Ping(p) => Some(Message::Ping(p.into())),
                tungstenite::Message::Pong(p) => Some(Message::Pong(p.into())),
                _ => None,
            };
            if let Some(m) = axum_msg {
                if browser_tx.send(m).await.is_err() {
                    break;
                }
            }
        }
    });

    // Relay: browser → upstream (state store)
    let to_upstream = tokio::spawn(async move {
        while let Some(Ok(msg)) = browser_rx.next().await {
            let tung_msg = match msg {
                Message::Text(t) => Some(tungstenite::Message::Text(t.to_string().into())),
                Message::Binary(b) => Some(tungstenite::Message::Binary(b.to_vec().into())),
                Message::Close(_) => break,
                Message::Ping(p) => Some(tungstenite::Message::Ping(p.to_vec().into())),
                Message::Pong(p) => Some(tungstenite::Message::Pong(p.to_vec().into())),
            };
            if let Some(m) = tung_msg {
                if upstream_tx.send(m).await.is_err() {
                    break;
                }
            }
        }
    });

    tokio::select! {
        _ = to_browser => {},
        _ = to_upstream => {},
    }
}

// --- HTTP proxy helpers ---

async fn proxy_get(state: &AppState, url: &str, token: &str) -> Response {
    let mut req = state.http.get(url);
    if !token.is_empty() {
        req = req.bearer_auth(token);
    }
    match req.send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            match resp.text().await {
                Ok(body) => (
                    status,
                    [("content-type", "application/json")],
                    body,
                )
                    .into_response(),
                Err(e) => (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({"error": e.to_string()})),
                )
                    .into_response(),
            }
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn proxy_patch(state: &AppState, url: &str, token: &str, body: serde_json::Value) -> Response {
    let mut req = state.http.patch(url).json(&body);
    if !token.is_empty() {
        req = req.bearer_auth(token);
    }
    match req.send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            match resp.text().await {
                Ok(body) => (status, [("content-type", "application/json")], body).into_response(),
                Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
            }
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

async fn proxy_put(state: &AppState, url: &str, token: &str, body: serde_json::Value) -> Response {
    let mut req = state.http.put(url).json(&body);
    if !token.is_empty() {
        req = req.bearer_auth(token);
    }
    match req.send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            match resp.text().await {
                Ok(body) => (status, [("content-type", "application/json")], body).into_response(),
                Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
            }
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

async fn proxy_delete(state: &AppState, url: &str, token: &str) -> Response {
    let mut req = state.http.delete(url);
    if !token.is_empty() {
        req = req.bearer_auth(token);
    }
    match req.send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            match resp.text().await {
                Ok(body) => (status, [("content-type", "application/json")], body).into_response(),
                Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
            }
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}
