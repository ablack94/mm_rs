use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::Utc;
use tracing::{info, warn};

use crate::state::SharedState;
use crate::types::*;

// ---------------------------------------------------------------------------
// Symbol normalization: "OMG-USD" -> "OMG/USD", "OMG%2FUSD" already decoded by axum
// ---------------------------------------------------------------------------

pub fn normalize_symbol(raw: &str) -> String {
    raw.replace('-', "/").to_uppercase()
}

// ---------------------------------------------------------------------------
// GET /health
// ---------------------------------------------------------------------------

pub async fn health(State(state): State<SharedState>) -> impl IntoResponse {
    let s = state.read().await;
    let uptime = s.started_at.elapsed().as_secs();
    let bot_connected = s.bot_count > 0;

    Json(HealthResponse {
        status: "ok".to_string(),
        bot_connected,
        pairs_count: s.store_data.pairs.len(),
        uptime_secs: uptime,
    })
}

// ---------------------------------------------------------------------------
// GET /pairs
// ---------------------------------------------------------------------------

pub async fn list_pairs(
    State(state): State<SharedState>,
    Query(query): Query<PairsQuery>,
) -> impl IntoResponse {
    let s = state.read().await;

    let filter_state: Option<PairState> = query.state.as_deref().and_then(parse_state_filter);

    let mut pairs: Vec<PairRecord> = s
        .store_data
        .pairs
        .values()
        .filter(|p| match &filter_state {
            Some(fs) => p.state == *fs,
            None => true,
        })
        .cloned()
        .collect();

    // Sort by symbol for deterministic output
    pairs.sort_by(|a, b| a.symbol.cmp(&b.symbol));

    Json(PairsListResponse { pairs })
}

fn parse_state_filter(s: &str) -> Option<PairState> {
    match s {
        "active" => Some(PairState::Active),
        "disabled" => Some(PairState::Disabled),
        "wind_down" => Some(PairState::WindDown),
        "liquidating" => Some(PairState::Liquidating),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// GET /pairs/:symbol
// ---------------------------------------------------------------------------

pub async fn get_pair(
    State(state): State<SharedState>,
    Path(symbol): Path<String>,
) -> impl IntoResponse {
    let symbol = normalize_symbol(&symbol);
    let s = state.read().await;

    match s.store_data.pairs.get(&symbol) {
        Some(pair) => Ok(Json(pair.clone())),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "pair not found".to_string(),
            }),
        )),
    }
}

// ---------------------------------------------------------------------------
// PUT /pairs/:symbol
// ---------------------------------------------------------------------------

pub async fn put_pair(
    State(state): State<SharedState>,
    Path(symbol): Path<String>,
    Json(body): Json<PutPairRequest>,
) -> impl IntoResponse {
    let symbol = normalize_symbol(&symbol);
    let now = Utc::now();

    let mut s = state.write().await;
    let is_new = !s.store_data.pairs.contains_key(&symbol);

    let pair = PairRecord {
        symbol: symbol.clone(),
        state: body.state.unwrap_or(PairState::Disabled),
        config: body.config.unwrap_or_default(),
        disabled_reason: None,
        auto_enable_at: None,
        created_at: if is_new {
            now
        } else {
            s.store_data.pairs[&symbol].created_at
        },
        updated_at: now,
    };

    s.store_data.pairs.insert(symbol.clone(), pair.clone());

    if let Err(e) = s.persist().await {
        warn!(error = %e, "failed to persist state");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("persist failed: {e}"),
            }),
        ));
    }

    s.broadcast(OutboundMessage::PairUpdated { pair: pair.clone() });

    let status = if is_new {
        info!(symbol = %symbol, "created pair");
        StatusCode::CREATED
    } else {
        info!(symbol = %symbol, "replaced pair");
        StatusCode::OK
    };

    Ok((status, Json(pair)))
}

// ---------------------------------------------------------------------------
// PATCH /pairs/:symbol
// ---------------------------------------------------------------------------

pub async fn patch_pair(
    State(state): State<SharedState>,
    Path(symbol): Path<String>,
    Json(body): Json<PatchPairRequest>,
) -> impl IntoResponse {
    let symbol = normalize_symbol(&symbol);
    let mut s = state.write().await;

    let pair = match s.store_data.pairs.get_mut(&symbol) {
        Some(p) => p,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "pair not found".to_string(),
                }),
            ));
        }
    };

    // Merge state
    if let Some(new_state) = body.state {
        pair.state = new_state;
    }

    // Merge config (only provided fields)
    if let Some(cfg) = body.config {
        if let Some(v) = cfg.order_size_usd {
            pair.config.order_size_usd = v;
        }
        if let Some(v) = cfg.max_inventory_usd {
            pair.config.max_inventory_usd = v;
        }
        if let Some(v) = cfg.min_spread_bps {
            pair.config.min_spread_bps = v;
        }
        if let Some(v) = cfg.spread_capture_pct {
            pair.config.spread_capture_pct = v;
        }
        if let Some(v) = cfg.min_profit_pct {
            pair.config.min_profit_pct = v;
        }
        if let Some(v) = cfg.stop_loss_pct {
            pair.config.stop_loss_pct = v;
        }
        if let Some(v) = cfg.take_profit_pct {
            pair.config.take_profit_pct = v;
        }
        if let Some(v) = cfg.max_buys_before_sell {
            pair.config.max_buys_before_sell = v;
        }
        if let Some(v) = cfg.use_winddown_for_stoploss {
            pair.config.use_winddown_for_stoploss = v;
        }
        if let Some(v) = cfg.limit_unwind_on_stoploss {
            pair.config.limit_unwind_on_stoploss = v;
        }
    }

    // Merge disabled_reason: if the field is present in the JSON body, update it
    // Since PatchPairRequest has Option<String>, a present "null" will be Some(None)
    // when using a raw JSON approach. But with serde defaults, absent = None.
    // We handle it simply: if the body has the field, set it.
    if body.disabled_reason.is_some() {
        pair.disabled_reason = body.disabled_reason;
    }

    if body.auto_enable_at.is_some() {
        pair.auto_enable_at = body.auto_enable_at;
    }

    pair.updated_at = Utc::now();
    let pair = pair.clone();

    if let Err(e) = s.persist().await {
        warn!(error = %e, "failed to persist state");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("persist failed: {e}"),
            }),
        ));
    }

    s.broadcast(OutboundMessage::PairUpdated { pair: pair.clone() });
    info!(symbol = %symbol, "patched pair");

    Ok((StatusCode::OK, Json(pair)))
}

// ---------------------------------------------------------------------------
// DELETE /pairs/:symbol
// ---------------------------------------------------------------------------

pub async fn delete_pair(
    State(state): State<SharedState>,
    Path(symbol): Path<String>,
) -> impl IntoResponse {
    let symbol = normalize_symbol(&symbol);
    let mut s = state.write().await;

    if s.store_data.pairs.remove(&symbol).is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "pair not found".to_string(),
            }),
        ));
    }

    if let Err(e) = s.persist().await {
        warn!(error = %e, "failed to persist state");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("persist failed: {e}"),
            }),
        ));
    }

    s.broadcast(OutboundMessage::PairRemoved {
        symbol: symbol.clone(),
    });
    info!(symbol = %symbol, "deleted pair");

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// GET /defaults
// ---------------------------------------------------------------------------

pub async fn get_defaults(State(state): State<SharedState>) -> impl IntoResponse {
    let s = state.read().await;
    Json(s.store_data.defaults.clone())
}

// ---------------------------------------------------------------------------
// PATCH /defaults
// ---------------------------------------------------------------------------

pub async fn patch_defaults(
    State(state): State<SharedState>,
    Json(body): Json<PatchDefaultsRequest>,
) -> impl IntoResponse {
    let mut s = state.write().await;

    if let Some(v) = body.order_size_usd {
        s.store_data.defaults.order_size_usd = v;
    }
    if let Some(v) = body.max_inventory_usd {
        s.store_data.defaults.max_inventory_usd = v;
    }
    if let Some(v) = body.min_spread_bps {
        s.store_data.defaults.min_spread_bps = v;
    }
    if let Some(v) = body.spread_capture_pct {
        s.store_data.defaults.spread_capture_pct = v;
    }
    if let Some(v) = body.min_profit_pct {
        s.store_data.defaults.min_profit_pct = v;
    }
    if let Some(v) = body.stop_loss_pct {
        s.store_data.defaults.stop_loss_pct = v;
    }
    if let Some(v) = body.take_profit_pct {
        s.store_data.defaults.take_profit_pct = v;
    }
    if let Some(v) = body.max_buys_before_sell {
        s.store_data.defaults.max_buys_before_sell = v;
    }
    if let Some(v) = body.use_winddown_for_stoploss {
        s.store_data.defaults.use_winddown_for_stoploss = v;
    }
    if let Some(v) = body.limit_unwind_on_stoploss {
        s.store_data.defaults.limit_unwind_on_stoploss = v;
    }

    if let Err(e) = s.persist().await {
        warn!(error = %e, "failed to persist state");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("persist failed: {e}"),
            }),
        ));
    }

    let defaults = s.store_data.defaults.clone();
    s.broadcast(OutboundMessage::DefaultsUpdated {
        defaults: defaults.clone(),
    });
    info!("patched global defaults");

    Ok(Json(defaults))
}
