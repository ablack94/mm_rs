use axum::{
    extract::{
        ws::{Message, WebSocket},
        Query, State, WebSocketUpgrade,
    },
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use tracing::{error, info, warn};

use crate::state::SharedState;
use crate::types::*;

// ---------------------------------------------------------------------------
// GET /ws — WebSocket upgrade
// ---------------------------------------------------------------------------

pub async fn ws_upgrade(
    State(state): State<SharedState>,
    Query(query): Query<WsQuery>,
    ws: WebSocketUpgrade,
    token: String,
) -> impl IntoResponse {
    // Auth: check token from query param
    let provided_token = query.token.as_deref().unwrap_or("");
    if provided_token != token {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "invalid token".to_string(),
            }),
        ));
    }

    Ok(ws.on_upgrade(move |socket| handle_ws(socket, state)))
}

// ---------------------------------------------------------------------------
// WebSocket connection handler
// ---------------------------------------------------------------------------

async fn handle_ws(socket: WebSocket, state: SharedState) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Increment bot count
    {
        let mut s = state.write().await;
        s.bot_count += 1;
        s.bot_last_seen = Some(Utc::now());
        info!(bot_count = s.bot_count, "bot connected via WebSocket");
    }

    // Send initial snapshot
    {
        let s = state.read().await;
        let snapshot = OutboundMessage::Snapshot {
            pairs: {
                let mut pairs: Vec<PairRecord> = s.store_data.pairs.values().cloned().collect();
                pairs.sort_by(|a, b| a.symbol.cmp(&b.symbol));
                pairs
            },
            defaults: s.store_data.defaults.clone(),
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        if let Err(e) = ws_tx.send(Message::Text(json.into())).await {
            error!(error = %e, "failed to send snapshot to bot");
            decrement_bot_count(&state).await;
            return;
        }
        info!("sent snapshot to bot");
    }

    // Subscribe to broadcast channel for outgoing messages
    let mut rx = state.read().await.ws_broadcast.subscribe();

    // Spawn a task to forward broadcast messages to this WS client
    let mut send_task = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    let json = serde_json::to_string(&msg).unwrap();
                    if let Err(e) = ws_tx.send(Message::Text(json.into())).await {
                        warn!(error = %e, "failed to send WS message to bot, disconnecting");
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "bot WS client lagged, skipped messages");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    });

    // Process incoming messages from the bot
    let state_clone = state.clone();
    let mut recv_task = tokio::spawn(async move {
        while let Some(result) = ws_rx.next().await {
            match result {
                Ok(Message::Text(text)) => {
                    handle_bot_message(&state_clone, &text).await;
                }
                Ok(Message::Close(_)) => {
                    info!("bot sent close frame");
                    break;
                }
                Err(e) => {
                    warn!(error = %e, "WS receive error");
                    break;
                }
                _ => {
                    // Ignore ping/pong/binary
                }
            }
        }
    });

    // Wait for either task to finish, then abort the other
    tokio::select! {
        _ = &mut send_task => {
            recv_task.abort();
        }
        _ = &mut recv_task => {
            send_task.abort();
        }
    }

    decrement_bot_count(&state).await;
}

async fn decrement_bot_count(state: &SharedState) {
    let mut s = state.write().await;
    s.bot_count = s.bot_count.saturating_sub(1);
    info!(bot_count = s.bot_count, "bot disconnected from WebSocket");
}

// ---------------------------------------------------------------------------
// Handle inbound bot messages
// ---------------------------------------------------------------------------

async fn handle_bot_message(state: &SharedState, text: &str) {
    let msg: InboundMessage = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(e) => {
            warn!(error = %e, "failed to parse inbound WS message");
            return;
        }
    };

    match msg {
        InboundMessage::Heartbeat { .. } => {
            let mut s = state.write().await;
            s.bot_last_seen = Some(Utc::now());
            tracing::debug!("received heartbeat from bot");
        }
        InboundMessage::PairReport {
            symbol,
            position_qty,
            ..
        } => {
            handle_pair_report(state, &symbol, &position_qty).await;
        }
    }
}

/// If the pair is in wind_down or liquidating state AND position is zero,
/// auto-transition to disabled. This is the ONLY smart behavior.
async fn handle_pair_report(state: &SharedState, symbol: &str, position_qty: &str) {
    // Check if position is zero
    let qty: f64 = match position_qty.parse() {
        Ok(v) => v,
        Err(_) => {
            if position_qty == "0" {
                0.0
            } else {
                warn!(symbol, position_qty, "could not parse position_qty");
                return;
            }
        }
    };

    if qty != 0.0 {
        return;
    }

    let mut s = state.write().await;
    let pair = match s.store_data.pairs.get_mut(symbol) {
        Some(p) => p,
        None => return,
    };

    if pair.state == PairState::WindDown || pair.state == PairState::Liquidating {
        let old_state = pair.state.clone();
        pair.state = PairState::Disabled;
        pair.disabled_reason = Some(format!(
            "auto-disabled: position closed (was {:?})",
            old_state
        ));
        pair.updated_at = Utc::now();
        let pair = pair.clone();

        info!(
            symbol,
            old_state = ?old_state,
            "auto-transitioned pair to disabled (position is zero)"
        );

        if let Err(e) = s.persist().await {
            warn!(error = %e, "failed to persist after auto-transition");
        }

        s.broadcast(OutboundMessage::PairUpdated { pair });
    }
}
