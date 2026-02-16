use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::types::event::ApiAction;
use crate::types::EngineEvent;
use super::shared::SharedState;

type AppState = Arc<SharedState>;

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

pub fn build_router(shared: Arc<SharedState>, api_token: String) -> Router {
    let token = Arc::new(api_token);

    Router::new()
        .route("/api/state", get(get_state))
        .route("/api/positions", get(get_positions))
        .route("/api/orders", get(get_orders))
        .route("/api/trades", get(get_trades))
        .route("/api/config", get(get_config))
        .route("/api/ticker/{pair}", get(get_ticker))
        .route("/api/ohlc/{pair}", get(get_ohlc))
        .route("/api/cancel-all", post(cancel_all))
        .route("/api/cancel/{cl_ord_id}", post(cancel_order))
        .route("/api/pause", post(pause))
        .route("/api/resume", post(resume))
        .route("/api/shutdown", post(shutdown))
        .route("/api/liquidate/{symbol}", post(liquidate))
        .with_state((shared, token))
}

type AuthState = (AppState, Arc<String>);

macro_rules! require_auth {
    ($headers:expr, $token:expr) => {
        if let Err(e) = check_auth(&$headers, &$token) {
            return e;
        }
    };
}

async fn get_state(
    headers: HeaderMap,
    State((shared, token)): State<AuthState>,
) -> (StatusCode, Json<Value>) {
    require_auth!(headers, token);
    let state = shared.bot_state.read().await;
    (StatusCode::OK, Json(json!(&*state)))
}

async fn get_positions(
    headers: HeaderMap,
    State((shared, token)): State<AuthState>,
) -> (StatusCode, Json<Value>) {
    require_auth!(headers, token);
    let state = shared.bot_state.read().await;
    (StatusCode::OK, Json(json!(&state.positions)))
}

async fn get_orders(
    headers: HeaderMap,
    State((shared, token)): State<AuthState>,
) -> (StatusCode, Json<Value>) {
    require_auth!(headers, token);
    let state = shared.bot_state.read().await;
    (StatusCode::OK, Json(json!(&state.open_orders)))
}

#[derive(Deserialize)]
struct TradesQuery {
    limit: Option<usize>,
}

async fn get_trades(
    headers: HeaderMap,
    State((shared, token)): State<AuthState>,
    Query(query): Query<TradesQuery>,
) -> (StatusCode, Json<Value>) {
    require_auth!(headers, token);
    let limit = query.limit.unwrap_or(50);
    let path = &shared.trade_log_path;

    match std::fs::read_to_string(path) {
        Ok(content) => {
            let lines: Vec<&str> = content.lines().collect();
            // Skip header, take last N lines
            let data_lines = if lines.len() > 1 { &lines[1..] } else { &[] };
            let start = data_lines.len().saturating_sub(limit);
            let recent: Vec<&str> = data_lines[start..].to_vec();
            (StatusCode::OK, Json(json!({
                "header": lines.first().unwrap_or(&""),
                "trades": recent,
                "total": data_lines.len(),
                "showing": recent.len(),
            })))
        }
        Err(_) => (StatusCode::OK, Json(json!({
            "trades": [],
            "total": 0,
            "showing": 0,
        }))),
    }
}

async fn get_config(
    headers: HeaderMap,
    State((shared, token)): State<AuthState>,
) -> (StatusCode, Json<Value>) {
    require_auth!(headers, token);
    let config = &shared.config;
    // Redact secrets
    (StatusCode::OK, Json(json!({
        "trading": &config.trading,
        "risk": &config.risk,
        "persistence": &config.persistence,
        "exchange": {
            "ws_public_url": &config.exchange.ws_public_url,
            "ws_private_url": &config.exchange.ws_private_url,
            "rest_base_url": &config.exchange.rest_base_url,
            "book_depth": config.exchange.book_depth,
            "api_key": "***REDACTED***",
            "api_secret": "***REDACTED***",
        },
    })))
}

async fn get_ticker(
    headers: HeaderMap,
    State((shared, token)): State<AuthState>,
    Path(pair): Path<String>,
) -> (StatusCode, Json<Value>) {
    require_auth!(headers, token);
    let url = format!(
        "{}/0/public/Ticker?pair={}",
        shared.config.exchange.rest_base_url, pair
    );
    match reqwest::get(&url).await {
        Ok(resp) => match resp.json::<Value>().await {
            Ok(data) => (StatusCode::OK, Json(data)),
            Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e.to_string()}))),
        },
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e.to_string()}))),
    }
}

async fn get_ohlc(
    headers: HeaderMap,
    State((shared, token)): State<AuthState>,
    Path(pair): Path<String>,
) -> (StatusCode, Json<Value>) {
    require_auth!(headers, token);
    let url = format!(
        "{}/0/public/OHLC?pair={}&interval=60",
        shared.config.exchange.rest_base_url, pair
    );
    match reqwest::get(&url).await {
        Ok(resp) => match resp.json::<Value>().await {
            Ok(data) => (StatusCode::OK, Json(data)),
            Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e.to_string()}))),
        },
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e.to_string()}))),
    }
}

async fn send_api_action(shared: &SharedState, action: ApiAction) -> (StatusCode, Json<Value>) {
    match shared.event_tx.send(EngineEvent::ApiCommand(action)).await {
        Ok(_) => (StatusCode::OK, Json(json!({"status": "ok"}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
    }
}

async fn cancel_all(
    headers: HeaderMap,
    State((shared, token)): State<AuthState>,
) -> (StatusCode, Json<Value>) {
    require_auth!(headers, token);
    send_api_action(&shared, ApiAction::CancelAll).await
}

async fn cancel_order(
    headers: HeaderMap,
    State((shared, token)): State<AuthState>,
    Path(cl_ord_id): Path<String>,
) -> (StatusCode, Json<Value>) {
    require_auth!(headers, token);
    send_api_action(&shared, ApiAction::CancelOrder { cl_ord_id }).await
}

async fn pause(
    headers: HeaderMap,
    State((shared, token)): State<AuthState>,
) -> (StatusCode, Json<Value>) {
    require_auth!(headers, token);
    send_api_action(&shared, ApiAction::Pause).await
}

async fn resume(
    headers: HeaderMap,
    State((shared, token)): State<AuthState>,
) -> (StatusCode, Json<Value>) {
    require_auth!(headers, token);
    send_api_action(&shared, ApiAction::Resume).await
}

async fn shutdown(
    headers: HeaderMap,
    State((shared, token)): State<AuthState>,
) -> (StatusCode, Json<Value>) {
    require_auth!(headers, token);
    send_api_action(&shared, ApiAction::Shutdown).await
}

async fn liquidate(
    headers: HeaderMap,
    State((shared, token)): State<AuthState>,
    Path(symbol): Path<String>,
) -> (StatusCode, Json<Value>) {
    require_auth!(headers, token);
    // URL-decode: convert CAMP_USD to CAMP/USD
    let symbol = symbol.replace('_', "/");
    send_api_action(&shared, ApiAction::Liquidate { symbol }).await
}
