use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use tokio::sync::mpsc;

use crate::exchange::messages::parse_proxy_event;
use crate::traits::EventSource;
use crate::types::*;
use exchange_api::ProxyEvent;
use trading_primitives::book::LevelUpdate;

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

            let proxy_event = match parse_proxy_event(&recorded.raw) {
                Ok(e) => e,
                Err(_) => continue,
            };
            let event = match proxy_event {
                ProxyEvent::BookSnapshot { symbol, bids, asks } => {
                    let pair = trading_primitives::Ticker::from(symbol.as_str());
                    Some(EngineEvent::BookSnapshot {
                        pair,
                        bids: bids.into_iter().map(|l| LevelUpdate { price: l.price, qty: l.qty }).collect(),
                        asks: asks.into_iter().map(|l| LevelUpdate { price: l.price, qty: l.qty }).collect(),
                        timestamp: recorded.timestamp,
                    })
                }
                ProxyEvent::BookUpdate { symbol, bids, asks } => {
                    let pair = trading_primitives::Ticker::from(symbol.as_str());
                    Some(EngineEvent::BookUpdate {
                        pair,
                        bid_updates: bids.into_iter().map(|l| LevelUpdate { price: l.price, qty: l.qty }).collect(),
                        ask_updates: asks.into_iter().map(|l| LevelUpdate { price: l.price, qty: l.qty }).collect(),
                        timestamp: recorded.timestamp,
                    })
                }
                ProxyEvent::Fill {
                    order_id,
                    cl_ord_id,
                    symbol,
                    side,
                    qty,
                    price,
                    fee,
                    is_maker,
                    timestamp,
                    is_fully_filled,
                } => {
                    let order_side = if side == "buy" {
                        OrderSide::Buy
                    } else {
                        OrderSide::Sell
                    };
                    let ts = timestamp
                        .parse()
                        .unwrap_or(recorded.timestamp);
                    Some(EngineEvent::Fill(Fill {
                        order_id,
                        cl_ord_id,
                        pair: trading_primitives::Ticker::from(symbol.as_str()),
                        side: order_side,
                        price,
                        qty,
                        fee,
                        is_maker,
                        is_fully_filled,
                        timestamp: ts,
                    }))
                }
                ProxyEvent::OrderAccepted { cl_ord_id, order_id, .. } => {
                    Some(EngineEvent::OrderAcknowledged { cl_ord_id, order_id })
                }
                ProxyEvent::OrderCancelled { cl_ord_id, reason, symbol } => {
                    let pair = symbol
                        .map(|s| trading_primitives::Ticker::from(s.as_str()))
                        .unwrap_or_else(|| trading_primitives::Ticker::from("UNKNOWN/UNKNOWN"));
                    Some(EngineEvent::OrderCancelled { cl_ord_id, pair, reason })
                }
                ProxyEvent::OrderRejected { cl_ord_id, error, symbol, .. } => {
                    let pair = symbol
                        .map(|s| trading_primitives::Ticker::from(s.as_str()))
                        .unwrap_or_else(|| trading_primitives::Ticker::from("UNKNOWN/UNKNOWN"));
                    Some(EngineEvent::OrderRejected { cl_ord_id, pair, reason: error })
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
