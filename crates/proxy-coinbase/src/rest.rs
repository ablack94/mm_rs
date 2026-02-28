use std::sync::Arc;

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::{get, post},
};
use serde_json::{json, Value};

use crate::auth::build_jwt;
use proxy_common::auth::check_auth;

/// Shared state for the Coinbase REST proxy.
pub struct CoinbaseRestState {
    pub api_key: String,
    pub api_secret: String,
    pub proxy_token: String,
    pub coinbase_base_url: String,
    pub client: reqwest::Client,
    /// Maker fee percentage (e.g., 0.006 = 0.6%). Configurable via COINBASE_MAKER_FEE env.
    pub maker_fee_pct: f64,
    /// Taker fee percentage. Configurable via COINBASE_TAKER_FEE env.
    pub taker_fee_pct: f64,
}

pub fn build_coinbase_rest_router(state: Arc<CoinbaseRestState>) -> Router {
    Router::new()
        // Map Kraken-style paths to Coinbase equivalents
        .route("/0/private/Balance", post(handle_balance))
        .route("/0/public/AssetPairs", get(handle_asset_pairs))
        .route("/0/public/Ticker", get(handle_ticker))
        .route("/0/private/GetWebSocketsToken", post(handle_ws_token))
        .route("/0/private/OpenOrders", post(handle_open_orders))
        .route("/0/private/TradesHistory", post(handle_trades_history))
        // Capabilities & health
        .route("/capabilities", get(capabilities))
        .route("/health", get(health))
        .with_state(state)
}

async fn capabilities() -> (StatusCode, Json<Value>) {
    (StatusCode::OK, Json(json!({"dead_man_switch": false})))
}

async fn health() -> (StatusCode, Json<Value>) {
    (StatusCode::OK, Json(json!({"status": "ok"})))
}

/// Extract the host from the base URL for JWT URI claim.
fn api_host(base_url: &str) -> &str {
    base_url
        .strip_prefix("https://")
        .or_else(|| base_url.strip_prefix("http://"))
        .unwrap_or(base_url)
        .trim_end_matches('/')
}

/// Helper to make authenticated GET requests to Coinbase.
async fn coinbase_get(
    state: &CoinbaseRestState,
    path: &str,
) -> Result<Value, (StatusCode, Json<Value>)> {
    let host = api_host(&state.coinbase_base_url);
    let jwt = build_jwt(&state.api_key, &state.api_secret, "GET", host, path)
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to build JWT");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e})))
        })?;

    let url = format!("{}{}", state.coinbase_base_url, path);
    let resp = state
        .client
        .get(&url)
        .header("Authorization", format!("Bearer {}", jwt))
        .header("Content-Type", "application/json")
        .send()
        .await
        .map_err(|e| {
            tracing::error!(path, error = %e, "Coinbase request failed");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": e.to_string()})),
            )
        })?;

    let status = resp.status();
    let body = resp.text().await.map_err(|e| {
        tracing::error!(path, error = %e, "Failed to read Coinbase response body");
        (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("read body: {}", e)})),
        )
    })?;

    if !status.is_success() {
        tracing::error!(path, %status, body = &body[..body.len().min(500)], "Coinbase API error");
        return Err((
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("Coinbase {} {}: {}", status, path, &body[..body.len().min(200)])})),
        ));
    }

    serde_json::from_str(&body).map_err(|e| {
        tracing::error!(path, error = %e, body = &body[..body.len().min(200)], "Failed to parse Coinbase JSON");
        (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("parse json: {}", e)})),
        )
    })
}

/// Helper to make authenticated POST requests to Coinbase.
#[allow(dead_code)]
async fn coinbase_post(
    state: &CoinbaseRestState,
    path: &str,
    body: &Value,
) -> Result<Value, (StatusCode, Json<Value>)> {
    let body_str = serde_json::to_string(body).unwrap_or_default();
    let host = api_host(&state.coinbase_base_url);
    let jwt = build_jwt(&state.api_key, &state.api_secret, "POST", host, path)
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to build JWT");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e})))
        })?;

    let url = format!("{}{}", state.coinbase_base_url, path);
    let resp = state
        .client
        .post(&url)
        .header("Authorization", format!("Bearer {}", jwt))
        .header("Content-Type", "application/json")
        .body(body_str)
        .send()
        .await
        .map_err(|e| {
            tracing::error!(path, error = %e, "Coinbase POST request failed");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": e.to_string()})),
            )
        })?;

    let status = resp.status();
    let resp_body = resp.text().await.map_err(|e| {
        tracing::error!(path, error = %e, "Failed to read Coinbase POST response body");
        (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("read body: {}", e)})),
        )
    })?;

    if !status.is_success() {
        tracing::error!(path, %status, body = &resp_body[..resp_body.len().min(500)], "Coinbase POST API error");
        return Err((
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("Coinbase {} {}: {}", status, path, &resp_body[..resp_body.len().min(200)])})),
        ));
    }

    serde_json::from_str(&resp_body).map_err(|e| {
        tracing::error!(path, error = %e, body = &resp_body[..resp_body.len().min(200)], "Failed to parse Coinbase POST JSON");
        (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("parse json: {}", e)})),
        )
    })
}

/// GET /0/private/Balance → GET /api/v3/brokerage/accounts
///
/// Translates Coinbase accounts response to Kraken balance format:
/// Kraken: {"result": {"ZUSD": "10000.00", "XXBT": "0.5"}}
async fn handle_balance(
    headers: HeaderMap,
    State(state): State<Arc<CoinbaseRestState>>,
) -> (StatusCode, Json<Value>) {
    if let Err(e) = check_auth(&headers, &state.proxy_token) {
        return e;
    }

    let path = "/api/v3/brokerage/accounts?limit=250";
    match coinbase_get(&state, path).await {
        Ok(data) => {
            let mut balances = serde_json::Map::new();
            if let Some(accounts) = data.get("accounts").and_then(|a| a.as_array()) {
                for account in accounts {
                    let currency = account
                        .get("currency")
                        .and_then(|c| c.as_str())
                        .unwrap_or("");
                    let available = account
                        .get("available_balance")
                        .and_then(|b| b.get("value"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("0");
                    let hold = account
                        .get("hold")
                        .and_then(|b| b.get("value"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("0");

                    // Total balance = available + hold
                    let total: f64 = available.parse::<f64>().unwrap_or(0.0)
                        + hold.parse::<f64>().unwrap_or(0.0);

                    if total > 0.0 {
                        balances.insert(currency.to_string(), json!(total.to_string()));
                    }
                }
            }
            (StatusCode::OK, Json(json!({"error": [], "result": balances})))
        }
        Err(e) => e,
    }
}

/// GET /0/public/AssetPairs → GET /api/v3/brokerage/products
///
/// Translates Coinbase products to Kraken AssetPairs format.
/// The bot uses: wsname, pair_decimals, lot_decimals, ordermin, fees_maker
async fn handle_asset_pairs(
    headers: HeaderMap,
    State(state): State<Arc<CoinbaseRestState>>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
) -> (StatusCode, Json<Value>) {
    if let Err(e) = check_auth(&headers, &state.proxy_token) {
        return e;
    }

    // Check if specific pairs were requested via query param
    let query = uri.query().unwrap_or("");
    let requested_pairs: Vec<&str> = if query.contains("pair=") {
        query
            .split('&')
            .find(|p| p.starts_with("pair="))
            .map(|p| p.trim_start_matches("pair=").split(',').collect())
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let path = "/api/v3/brokerage/products?product_type=SPOT&limit=250";
    match coinbase_get(&state, path).await {
        Ok(data) => {
            let mut result = serde_json::Map::new();
            if let Some(products) = data.get("products").and_then(|p| p.as_array()) {
                for product in products {
                    let product_id = product
                        .get("product_id")
                        .and_then(|p| p.as_str())
                        .unwrap_or("");

                    // Filter if specific pairs requested
                    if !requested_pairs.is_empty()
                        && !requested_pairs.contains(&product_id)
                    {
                        continue;
                    }

                    // Skip pairs not quoted in USD or USDC, and disabled products
                    let quote = product
                        .get("quote_currency_id")
                        .and_then(|q| q.as_str())
                        .unwrap_or("");
                    if quote != "USD" && quote != "USDC" && !requested_pairs.contains(&product_id) {
                        continue;
                    }

                    let is_disabled = product
                        .get("is_disabled")
                        .and_then(|d| d.as_bool())
                        .unwrap_or(false);
                    if is_disabled {
                        continue;
                    }

                    let base = product
                        .get("base_currency_id")
                        .and_then(|b| b.as_str())
                        .unwrap_or("");

                    // Internal format: BASE/QUOTE — normalize USD to USDC
                    let norm_quote = if quote == "USD" { "USDC" } else { quote };
                    let wsname = format!("{}/{}", base, norm_quote);

                    // Calculate decimal places from increment strings
                    let quote_increment = product
                        .get("quote_increment")
                        .and_then(|q| q.as_str())
                        .unwrap_or("0.01");
                    let base_increment = product
                        .get("base_increment")
                        .and_then(|b| b.as_str())
                        .unwrap_or("0.00000001");
                    let base_min = product
                        .get("base_min_size")
                        .and_then(|m| m.as_str())
                        .unwrap_or("0.00000001");

                    let pair_decimals = decimal_places(quote_increment);
                    let lot_decimals = decimal_places(base_increment);

                    // Use product_id as the REST key (Coinbase doesn't have separate REST/WS names)
                    // Coinbase uses quote_min_size as minimum order cost
                    let quote_min = product
                        .get("quote_min_size")
                        .and_then(|m| m.as_str())
                        .unwrap_or("1");

                    result.insert(
                        product_id.to_string(),
                        json!({
                            "wsname": wsname,
                            "pair_decimals": pair_decimals,
                            "lot_decimals": lot_decimals,
                            "ordermin": base_min,
                            "costmin": quote_min,
                            "fees_maker": [[0, state.maker_fee_pct * 100.0]],
                            "fees": [[0, state.taker_fee_pct * 100.0]],
                            "base": base,
                            "quote": quote
                        }),
                    );
                }
            }
            (StatusCode::OK, Json(json!({"error": [], "result": result})))
        }
        Err(e) => e,
    }
}

/// GET /0/public/Ticker → GET /api/v3/brokerage/best_bid_ask
///
/// Translates to Kraken ticker format. The bot uses: a (ask), b (bid), c (last).
async fn handle_ticker(
    headers: HeaderMap,
    State(state): State<Arc<CoinbaseRestState>>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
) -> (StatusCode, Json<Value>) {
    if let Err(e) = check_auth(&headers, &state.proxy_token) {
        return e;
    }

    let query = uri.query().unwrap_or("");
    let pair = query
        .split('&')
        .find(|p| p.starts_with("pair="))
        .map(|p| p.trim_start_matches("pair="))
        .unwrap_or("");

    // Coinbase uses product_ids parameter
    let path = if pair.is_empty() {
        "/api/v3/brokerage/best_bid_ask".to_string()
    } else {
        format!(
            "/api/v3/brokerage/best_bid_ask?product_ids={}",
            pair
        )
    };

    match coinbase_get(&state, &path).await {
        Ok(data) => {
            let mut result = serde_json::Map::new();
            if let Some(pricebooks) = data.get("pricebooks").and_then(|p| p.as_array()) {
                for pb in pricebooks {
                    let product_id = pb
                        .get("product_id")
                        .and_then(|p| p.as_str())
                        .unwrap_or("");
                    let best_bid = pb
                        .get("bids")
                        .and_then(|b| b.as_array())
                        .and_then(|a| a.first())
                        .and_then(|b| b.get("price"))
                        .and_then(|p| p.as_str())
                        .unwrap_or("0");
                    let best_ask = pb
                        .get("asks")
                        .and_then(|a| a.as_array())
                        .and_then(|a| a.first())
                        .and_then(|a| a.get("price"))
                        .and_then(|p| p.as_str())
                        .unwrap_or("0");

                    // Kraken ticker format: a=[ask_price, ...], b=[bid_price, ...], c=[last_price, ...]
                    let mid: f64 = (best_bid.parse::<f64>().unwrap_or(0.0)
                        + best_ask.parse::<f64>().unwrap_or(0.0))
                        / 2.0;

                    result.insert(
                        product_id.to_string(),
                        json!({
                            "a": [best_ask, "0", "0"],
                            "b": [best_bid, "0", "0"],
                            "c": [mid.to_string(), "0"],
                            "o": mid.to_string(),
                            "v": ["0", "0"]
                        }),
                    );
                }
            }
            (StatusCode::OK, Json(json!({"error": [], "result": result})))
        }
        Err(e) => e,
    }
}

/// POST /0/private/GetWebSocketsToken
///
/// Coinbase doesn't use WS tokens — auth is done per-subscribe via JWT/HMAC.
/// Return a dummy token; the proxy handles auth internally.
async fn handle_ws_token(
    headers: HeaderMap,
    State(state): State<Arc<CoinbaseRestState>>,
) -> (StatusCode, Json<Value>) {
    if let Err(e) = check_auth(&headers, &state.proxy_token) {
        return e;
    }

    // Return a dummy token — the proxy handles Coinbase auth internally
    (
        StatusCode::OK,
        Json(json!({"error": [], "result": {"token": "coinbase-proxy-managed"}})),
    )
}

/// POST /0/private/OpenOrders → GET /api/v3/brokerage/orders/historical/batch?order_status=OPEN
async fn handle_open_orders(
    headers: HeaderMap,
    State(state): State<Arc<CoinbaseRestState>>,
) -> (StatusCode, Json<Value>) {
    if let Err(e) = check_auth(&headers, &state.proxy_token) {
        return e;
    }

    let path = "/api/v3/brokerage/orders/historical/batch?order_status=OPEN";
    match coinbase_get(&state, path).await {
        Ok(data) => {
            let mut open_orders = serde_json::Map::new();
            if let Some(orders) = data.get("orders").and_then(|o| o.as_array()) {
                for order in orders {
                    let order_id = order
                        .get("order_id")
                        .and_then(|id| id.as_str())
                        .unwrap_or("");
                    let product_id = order
                        .get("product_id")
                        .and_then(|p| p.as_str())
                        .unwrap_or("");

                    let wsname = crate::pairs::to_internal(product_id);

                    open_orders.insert(
                        order_id.to_string(),
                        json!({
                            "descr": {
                                "pair": wsname,
                                "type": order.get("side").and_then(|s| s.as_str()).unwrap_or("buy").to_lowercase()
                            }
                        }),
                    );
                }
            }
            (
                StatusCode::OK,
                Json(json!({"error": [], "result": {"open": open_orders}})),
            )
        }
        Err(e) => e,
    }
}

/// POST /0/private/TradesHistory → GET /api/v3/brokerage/orders/historical/fills
async fn handle_trades_history(
    headers: HeaderMap,
    State(state): State<Arc<CoinbaseRestState>>,
) -> (StatusCode, Json<Value>) {
    if let Err(e) = check_auth(&headers, &state.proxy_token) {
        return e;
    }

    let path = "/api/v3/brokerage/orders/historical/fills?limit=100";
    match coinbase_get(&state, path).await {
        Ok(data) => {
            let mut trades = serde_json::Map::new();
            if let Some(fills) = data.get("fills").and_then(|f| f.as_array()) {
                for fill in fills {
                    let trade_id = fill
                        .get("trade_id")
                        .and_then(|id| id.as_str())
                        .unwrap_or("");
                    let product_id = fill
                        .get("product_id")
                        .and_then(|p| p.as_str())
                        .unwrap_or("");
                    let wsname = crate::pairs::to_internal(product_id);

                    trades.insert(
                        trade_id.to_string(),
                        json!({
                            "pair": wsname,
                            "price": fill.get("price").and_then(|p| p.as_str()).unwrap_or("0"),
                            "vol": fill.get("size").and_then(|s| s.as_str()).unwrap_or("0"),
                            "type": fill.get("side").and_then(|s| s.as_str()).unwrap_or("buy").to_lowercase(),
                            "fee": fill.get("commission").and_then(|c| c.as_str()).unwrap_or("0")
                        }),
                    );
                }
            }
            (
                StatusCode::OK,
                Json(json!({"error": [], "result": {"trades": trades}})),
            )
        }
        Err(e) => e,
    }
}

/// Count decimal places in a string like "0.01" → 2, "0.00000001" → 8
fn decimal_places(s: &str) -> u32 {
    if let Some(dot_pos) = s.find('.') {
        let after_dot = &s[dot_pos + 1..];
        after_dot.len() as u32
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decimal_places() {
        assert_eq!(decimal_places("0.01"), 2);
        assert_eq!(decimal_places("0.00000001"), 8);
        assert_eq!(decimal_places("1"), 0);
        assert_eq!(decimal_places("0.1"), 1);
    }
}
