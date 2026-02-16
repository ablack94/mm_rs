use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::{
    cost_min_for, order_min_for, price_decimals_for, qty_decimals_for,
    symbol_to_kraken_asset, symbol_to_rest_key, SharedState,
};

/// GET /health
pub async fn health() -> (StatusCode, Json<Value>) {
    (StatusCode::OK, Json(json!({"status": "ok"})))
}

/// POST /0/private/GetWebSocketsToken
pub async fn get_ws_token() -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK,
        Json(json!({
            "error": [],
            "result": {
                "token": "mock-ws-token-12345",
                "expires": 900
            }
        })),
    )
}

/// POST /0/private/Balance
pub async fn get_balance(State(state): State<SharedState>) -> (StatusCode, Json<Value>) {
    let state = state.read().await;
    let mut result = serde_json::Map::new();
    for (asset, balance) in &state.balances {
        result.insert(asset.clone(), json!(format!("{:.8}", balance)));
    }
    (
        StatusCode::OK,
        Json(json!({
            "error": [],
            "result": result,
        })),
    )
}

#[derive(Debug, Deserialize)]
pub struct PairQuery {
    pub pair: Option<String>,
}

/// GET /0/public/AssetPairs
pub async fn get_asset_pairs(
    State(state): State<SharedState>,
) -> (StatusCode, Json<Value>) {
    let state = state.read().await;
    let mut result = serde_json::Map::new();

    for pc in &state.pair_configs {
        let rest_key = symbol_to_rest_key(&pc.symbol);
        let base_asset = symbol_to_kraken_asset(&pc.symbol);
        let pd = price_decimals_for(pc.starting_price);
        let qd = qty_decimals_for(pc.starting_price);
        let ordermin = order_min_for(pc.starting_price);
        let costmin = cost_min_for(pc.starting_price);

        result.insert(
            rest_key,
            json!({
                "wsname": pc.symbol,
                "pair_decimals": pd,
                "lot_decimals": qd,
                "ordermin": format!("{}", ordermin),
                "costmin": format!("{:.2}", costmin),
                "base": base_asset,
                "quote": "ZUSD",
                "fees_maker": [[0, 0.26]],
                "fees": [[0, 0.40]],
                "status": "online",
                "lot": "unit",
                "lot_multiplier": 1,
            }),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "error": [],
            "result": result,
        })),
    )
}

/// GET /0/public/Ticker
pub async fn get_ticker(
    State(state): State<SharedState>,
    Query(query): Query<PairQuery>,
) -> (StatusCode, Json<Value>) {
    let state = state.read().await;
    let mut result = serde_json::Map::new();

    // If a pair query is given, filter to those pairs
    let requested_rest_keys: Vec<String> = query
        .pair
        .as_deref()
        .map(|p| p.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();

    for pc in &state.pair_configs {
        let rest_key = symbol_to_rest_key(&pc.symbol);

        // Filter if specific pairs requested
        if !requested_rest_keys.is_empty() && !requested_rest_keys.contains(&rest_key) {
            continue;
        }

        let book = match state.books.get(&pc.symbol) {
            Some(b) => b,
            None => continue,
        };

        let best_bid = book.best_bid().unwrap_or(0.0);
        let best_ask = book.best_ask().unwrap_or(0.0);
        let mid = book.mid_price();
        let pd = price_decimals_for(mid);
        let qd = qty_decimals_for(mid);

        let price_str = format!("{:.prec$}", mid, prec = pd as usize);
        let volume = 100000.0; // synthetic volume

        result.insert(
            rest_key,
            json!({
                "a": [format!("{:.prec$}", best_ask, prec = pd as usize), "1", "1.000"],
                "b": [format!("{:.prec$}", best_bid, prec = pd as usize), "1", "1.000"],
                "c": [&price_str, "1.000"],
                "v": ["50000.000", format!("{:.prec$}", volume, prec = qd as usize)],
                "p": [&price_str, &price_str],
                "t": [100, 500],
                "l": [&price_str, &price_str],
                "h": [&price_str, &price_str],
                "o": &price_str,
            }),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "error": [],
            "result": result,
        })),
    )
}
