//! WebSocket client for connecting to the state store service.
//!
//! The state store is an external CRUD service that manages pair configs/states.
//! This client connects via WS, receives pair config/state updates, and sends
//! heartbeats and pair reports back.
//!
//! The bot should still work WITHOUT a state store connection -- this client
//! gracefully handles disconnects and reconnects with exponential backoff.

use std::time::Duration;

use chrono::Utc;
use futures::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::messages::{BotMessage, StateStoreMessage};

/// Commands that the state store client sends to the engine.
#[derive(Debug, Clone)]
pub enum StateStoreCommand {
    /// Full snapshot of pairs and defaults received on connect.
    Snapshot {
        pairs: Vec<super::messages::PairRecord>,
        defaults: crate::types::GlobalDefaults,
    },
    /// A single pair was updated.
    PairUpdated(super::messages::PairRecord),
    /// A pair was removed -- engine should cancel orders and stop quoting.
    PairRemoved { symbol: String },
    /// Global defaults changed.
    DefaultsUpdated(crate::types::GlobalDefaults),
}

/// Configuration for the state store connection.
#[derive(Debug, Clone)]
pub struct StateStoreConfig {
    pub url: String,
    pub token: String,
    pub heartbeat_interval: Duration,
    pub report_interval: Duration,
    pub reconnect_base_delay: Duration,
    pub reconnect_max_delay: Duration,
}

impl Default for StateStoreConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            token: String::new(),
            heartbeat_interval: Duration::from_secs(30),
            report_interval: Duration::from_secs(10),
            reconnect_base_delay: Duration::from_secs(1),
            reconnect_max_delay: Duration::from_secs(60),
        }
    }
}

/// Data for a single pair report, collected from the engine.
#[derive(Debug, Clone)]
pub struct PairReportData {
    pub symbol: String,
    pub position_qty: Decimal,
    pub position_avg_cost: Decimal,
    pub exposure_usd: Decimal,
    pub quoter_state: String,
    pub has_open_orders: bool,
}

/// Snapshot of engine state used to build heartbeats and pair reports.
/// Sent from bot main loop to the WS client task via channel.
#[derive(Debug, Clone)]
pub struct EngineSnapshot {
    pub active_pairs: u32,
    pub total_exposure_usd: Decimal,
    pub pair_reports: Vec<PairReportData>,
}

/// Build the WS URL from the state store HTTP URL and token.
///
/// Converts "http://host:port" -> "ws://host:port/ws?token=..."
/// Converts "https://host:port" -> "wss://host:port/ws?token=..."
/// If no scheme, assumes ws://.
fn build_ws_url(base_url: &str, token: &str) -> String {
    let ws_url = if base_url.starts_with("https://") {
        base_url.replacen("https://", "wss://", 1)
    } else if base_url.starts_with("http://") {
        base_url.replacen("http://", "ws://", 1)
    } else if base_url.starts_with("ws://") || base_url.starts_with("wss://") {
        base_url.to_string()
    } else {
        format!("ws://{}", base_url)
    };

    let ws_url = ws_url.trim_end_matches('/');
    format!("{}/ws?token={}", ws_url, token)
}

/// State store WebSocket client.
///
/// Runs as a long-lived tokio task. Connects to the state store,
/// receives config/state updates, and sends them to the engine
/// via an mpsc channel. Sends heartbeats and pair reports back
/// to the state store periodically.
pub struct StateStoreClient {
    config: StateStoreConfig,
    cmd_tx: mpsc::Sender<StateStoreCommand>,
    /// Channel to receive engine snapshots for heartbeat/report generation.
    snapshot_rx: mpsc::Receiver<EngineSnapshot>,
}

impl StateStoreClient {
    /// Create a new state store client. Does not connect until `run()` is called.
    ///
    /// Returns the client and a sender for pushing engine snapshots into it.
    pub fn new(
        config: StateStoreConfig,
        cmd_tx: mpsc::Sender<StateStoreCommand>,
        snapshot_rx: mpsc::Receiver<EngineSnapshot>,
    ) -> Self {
        Self {
            config,
            cmd_tx,
            snapshot_rx,
        }
    }

    /// Whether the state store URL is configured (non-empty).
    pub fn is_configured(&self) -> bool {
        !self.config.url.is_empty()
    }

    /// Run the state store client. This is a long-running task that should be
    /// spawned in a tokio task. It will reconnect on disconnect with
    /// exponential backoff.
    pub async fn run(mut self) {
        if !self.is_configured() {
            tracing::info!("State store not configured -- running without external config");
            return;
        }

        let ws_url = build_ws_url(&self.config.url, &self.config.token);
        tracing::info!(url = %self.config.url, "State store client starting");

        let mut backoff = self.config.reconnect_base_delay;

        loop {
            tracing::info!(ws_url = %ws_url, "Connecting to state store...");

            match tokio_tungstenite::connect_async(&ws_url).await {
                Ok((ws_stream, _response)) => {
                    tracing::info!("Connected to state store");
                    backoff = self.config.reconnect_base_delay; // reset on success

                    let disconnect_reason = self.run_connected(ws_stream).await;
                    tracing::warn!(reason = %disconnect_reason, "Disconnected from state store");
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        backoff_secs = backoff.as_secs(),
                        "Failed to connect to state store"
                    );
                }
            }

            // Exponential backoff before reconnecting
            tracing::info!(
                delay_secs = backoff.as_secs(),
                "Reconnecting to state store after delay"
            );
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(self.config.reconnect_max_delay);
        }
    }

    /// Run the connected session. Returns a reason string when disconnected.
    async fn run_connected(
        &mut self,
        ws_stream: tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> String {
        let (mut ws_write, mut ws_read) = ws_stream.split();

        let mut heartbeat_interval =
            tokio::time::interval(self.config.heartbeat_interval);
        heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // Tracks the latest engine snapshot for heartbeat/report generation
        let mut latest_snapshot: Option<EngineSnapshot> = None;
        let mut report_interval = tokio::time::interval(self.config.report_interval);
        report_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                // Receive messages from the state store
                ws_msg = ws_read.next() => {
                    match ws_msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Err(e) = self.handle_message(&text).await {
                                tracing::warn!(error = %e, raw = %text, "Failed to handle state store message");
                            }
                        }
                        Some(Ok(Message::Ping(data))) => {
                            if let Err(e) = ws_write.send(Message::Pong(data)).await {
                                return format!("Failed to send pong: {}", e);
                            }
                        }
                        Some(Ok(Message::Close(_))) => {
                            return "Server sent close frame".to_string();
                        }
                        Some(Err(e)) => {
                            return format!("WebSocket error: {}", e);
                        }
                        None => {
                            return "WebSocket stream ended".to_string();
                        }
                        _ => {} // Binary, Pong, Frame -- ignore
                    }
                }

                // Receive engine snapshots from the bot main loop
                snapshot = self.snapshot_rx.recv() => {
                    match snapshot {
                        Some(snap) => {
                            latest_snapshot = Some(snap);
                        }
                        None => {
                            return "Engine snapshot channel closed".to_string();
                        }
                    }
                }

                // Send heartbeat periodically
                _ = heartbeat_interval.tick() => {
                    if let Some(ref snap) = latest_snapshot {
                        let msg = BotMessage::Heartbeat {
                            timestamp: Utc::now(),
                            active_pairs: snap.active_pairs,
                            total_exposure_usd: snap.total_exposure_usd,
                        };
                        if let Err(e) = send_json(&mut ws_write, &msg).await {
                            return format!("Failed to send heartbeat: {}", e);
                        }
                        tracing::debug!(
                            active_pairs = snap.active_pairs,
                            exposure = %snap.total_exposure_usd.round_dp(2),
                            "Sent heartbeat to state store"
                        );
                    }
                }

                // Send pair reports periodically
                _ = report_interval.tick() => {
                    if let Some(ref snap) = latest_snapshot {
                        for report in &snap.pair_reports {
                            let msg = BotMessage::PairReport {
                                symbol: report.symbol.clone(),
                                position_qty: report.position_qty,
                                position_avg_cost: report.position_avg_cost,
                                exposure_usd: report.exposure_usd,
                                quoter_state: report.quoter_state.clone(),
                                has_open_orders: report.has_open_orders,
                            };
                            if let Err(e) = send_json(&mut ws_write, &msg).await {
                                return format!("Failed to send pair report: {}", e);
                            }
                        }
                        tracing::debug!(
                            count = snap.pair_reports.len(),
                            "Sent pair reports to state store"
                        );
                    }
                }
            }
        }
    }

    /// Parse and dispatch an incoming state store message.
    async fn handle_message(&self, text: &str) -> anyhow::Result<()> {
        let msg: StateStoreMessage = serde_json::from_str(text)?;

        match msg {
            StateStoreMessage::Snapshot { pairs, defaults } => {
                tracing::info!(
                    pair_count = pairs.len(),
                    "Received snapshot from state store"
                );
                for p in &pairs {
                    tracing::info!(
                        symbol = p.symbol,
                        state = ?p.state,
                        "  pair in snapshot"
                    );
                }
                self.cmd_tx
                    .send(StateStoreCommand::Snapshot { pairs, defaults })
                    .await
                    .map_err(|_| anyhow::anyhow!("Engine command channel closed"))?;
            }
            StateStoreMessage::PairUpdated { pair } => {
                tracing::info!(
                    symbol = pair.symbol,
                    state = ?pair.state,
                    "Received pair_updated from state store"
                );
                self.cmd_tx
                    .send(StateStoreCommand::PairUpdated(pair))
                    .await
                    .map_err(|_| anyhow::anyhow!("Engine command channel closed"))?;
            }
            StateStoreMessage::PairRemoved { symbol } => {
                tracing::info!(symbol, "Received pair_removed from state store");
                self.cmd_tx
                    .send(StateStoreCommand::PairRemoved { symbol })
                    .await
                    .map_err(|_| anyhow::anyhow!("Engine command channel closed"))?;
            }
            StateStoreMessage::DefaultsUpdated { defaults } => {
                tracing::info!("Received defaults_updated from state store");
                self.cmd_tx
                    .send(StateStoreCommand::DefaultsUpdated(defaults))
                    .await
                    .map_err(|_| anyhow::anyhow!("Engine command channel closed"))?;
            }
        }

        Ok(())
    }
}

/// Helper: serialize and send a JSON message over the WS write half.
async fn send_json<S>(
    ws_write: &mut futures::stream::SplitSink<S, Message>,
    msg: &impl serde::Serialize,
) -> anyhow::Result<()>
where
    S: futures::Sink<Message> + Unpin,
    <S as futures::Sink<Message>>::Error: std::fmt::Display,
{
    let text = serde_json::to_string(msg)?;
    ws_write
        .send(Message::Text(text.into()))
        .await
        .map_err(|e| anyhow::anyhow!("WS send failed: {}", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_ws_url_http() {
        let url = build_ws_url("http://localhost:3040", "secret123");
        assert_eq!(url, "ws://localhost:3040/ws?token=secret123");
    }

    #[test]
    fn test_build_ws_url_https() {
        let url = build_ws_url("https://example.com:3040", "tok");
        assert_eq!(url, "wss://example.com:3040/ws?token=tok");
    }

    #[test]
    fn test_build_ws_url_already_ws() {
        let url = build_ws_url("ws://localhost:3040", "abc");
        assert_eq!(url, "ws://localhost:3040/ws?token=abc");
    }

    #[test]
    fn test_build_ws_url_trailing_slash() {
        let url = build_ws_url("http://localhost:3040/", "t");
        assert_eq!(url, "ws://localhost:3040/ws?token=t");
    }

    #[test]
    fn test_build_ws_url_bare_host() {
        let url = build_ws_url("localhost:3040", "t");
        assert_eq!(url, "ws://localhost:3040/ws?token=t");
    }
}
