use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use tokio::sync::mpsc;

use crate::exchange::messages::{parse_ws_message, WsMessage};
use crate::traits::EventSource;
use crate::types::*;

/// Recorded WS message with timestamp.
#[derive(Debug, serde::Deserialize)]
struct RecordedMessage {
    timestamp: DateTime<Utc>,
    raw: String,
}

/// Replays recorded WS data through the event channel.
/// Used for deterministic integration testing.
pub struct ReplaySource {
    path: PathBuf,
    /// Playback speed multiplier (1.0 = realtime, 0.0 = instant)
    speed: f64,
}

impl ReplaySource {
    pub fn new(path: impl Into<PathBuf>, speed: f64) -> Self {
        Self {
            path: path.into(),
            speed,
        }
    }

    /// Instant replay (no delays).
    pub fn instant(path: impl Into<PathBuf>) -> Self {
        Self::new(path, 0.0)
    }
}

#[async_trait]
impl EventSource for ReplaySource {
    async fn run(&mut self, tx: mpsc::Sender<EngineEvent>) -> Result<()> {
        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut last_ts: Option<DateTime<Utc>> = None;

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }

            let recorded: RecordedMessage = serde_json::from_str(&line)?;

            // Simulate timing
            if self.speed > 0.0 {
                if let Some(prev) = last_ts {
                    let delta = (recorded.timestamp - prev).to_std().unwrap_or_default();
                    let sleep_dur = delta.mul_f64(1.0 / self.speed);
                    tokio::time::sleep(sleep_dur).await;
                }
            }
            last_ts = Some(recorded.timestamp);

            let msg = parse_ws_message(&recorded.raw);
            let event = match msg {
                WsMessage::BookSnapshot {
                    pair,
                    bids,
                    asks,
                } => Some(EngineEvent::BookSnapshot {
                    pair,
                    bids,
                    asks,
                    timestamp: recorded.timestamp,
                }),
                WsMessage::BookUpdate {
                    pair,
                    bid_updates,
                    ask_updates,
                } => Some(EngineEvent::BookUpdate {
                    pair,
                    bid_updates,
                    ask_updates,
                    timestamp: recorded.timestamp,
                }),
                WsMessage::Execution(report) => {
                    let side = if report.side == "buy" {
                        OrderSide::Buy
                    } else {
                        OrderSide::Sell
                    };
                    match report.exec_type.as_str() {
                        "trade" | "filled" => Some(EngineEvent::Fill(Fill {
                            order_id: report.order_id,
                            cl_ord_id: report.cl_ord_id,
                            pair: report.pair,
                            side,
                            price: report.last_price,
                            qty: report.last_qty,
                            fee: report.fee,
                            is_maker: report.is_maker,
                            is_fully_filled: report.order_status == "filled",
                            timestamp: report.timestamp,
                        })),
                        _ => None,
                    }
                }
                _ => None,
            };

            if let Some(e) = event {
                if tx.send(e).await.is_err() {
                    break;
                }
            }
        }
        Ok(())
    }
}
