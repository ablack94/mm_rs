use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use crate::state::ExchangeState;

/// Handle a public WebSocket connection.
/// Listens for subscription requests, then relays book updates from the broadcast channel.
pub async fn handle_public_ws(
    socket: WebSocket,
    state: Arc<RwLock<ExchangeState>>,
    mut book_rx: broadcast::Receiver<String>,
) {
    let (mut ws_write, mut ws_read) = socket.split();

    // Wait for the client to subscribe to book
    let mut subscribed_symbols: Vec<String> = Vec::new();

    // Spawn a task to handle incoming messages and collect subscriptions
    let state_clone = state.clone();
    let (sub_tx, mut sub_rx) = tokio::sync::mpsc::channel::<Vec<String>>(16);

    // Reader task: parse subscription requests
    let reader = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_read.next().await {
            if let Message::Text(text) = msg {
                let text_str = text.to_string();
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text_str) {
                    if v["method"].as_str() == Some("subscribe")
                        && v["params"]["channel"].as_str() == Some("book")
                    {
                        let symbols: Vec<String> = v["params"]["symbol"]
                            .as_array()
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|s| s.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default();

                        let _ = sub_tx.send(symbols).await;
                    }
                }
            }
        }
    });

    // Writer task: wait for subscriptions then relay book updates
    let writer = tokio::spawn(async move {
        // Wait for subscription
        if let Some(symbols) = sub_rx.recv().await {
            subscribed_symbols = symbols;
        }

        // Send subscription confirmations
        for sym in &subscribed_symbols {
            let confirm = crate::messages::subscribe_book_confirm(sym);
            let msg = serde_json::to_string(&confirm).unwrap();
            if ws_write.send(Message::Text(msg.into())).await.is_err() {
                return;
            }
        }

        // Send initial book snapshots
        {
            let st = state_clone.read().await;
            for sym in &subscribed_symbols {
                if let Some(book) = st.books.get(sym) {
                    let snapshot = crate::messages::book_snapshot(sym, book);
                    let msg = serde_json::to_string(&snapshot).unwrap();
                    if ws_write.send(Message::Text(msg.into())).await.is_err() {
                        return;
                    }
                }
            }
        }

        // Relay book updates from broadcast channel
        loop {
            match book_rx.recv().await {
                Ok(msg_str) => {
                    // Check if this update is for a subscribed symbol, or a heartbeat
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg_str) {
                        let channel = v["channel"].as_str().unwrap_or("");
                        let should_send = if channel == "heartbeat" {
                            true
                        } else {
                            let sym = v["data"][0]["symbol"].as_str().unwrap_or("");
                            subscribed_symbols.iter().any(|s| s == sym)
                        };
                        if should_send {
                            if ws_write
                                .send(Message::Text(msg_str.into()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    tokio::select! {
        _ = reader => {}
        _ = writer => {}
    }
}
