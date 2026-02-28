use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use trading_primitives::Ticker;
use crate::types::{OrderSide, Position};

/// An order the engine is tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedOrder {
    pub cl_ord_id: String,
    pub pair: Ticker,
    pub side: OrderSide,
    pub price: Decimal,
    pub qty: Decimal,
    pub placed_at: DateTime<Utc>,
    /// Whether the exchange has acknowledged this order (safe to amend).
    #[serde(default)]
    pub acked: bool,
}

/// Full bot state, serializable for persistence.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BotState {
    pub positions: HashMap<Ticker, Position>,
    pub open_orders: HashMap<String, TrackedOrder>,
    pub realized_pnl: Decimal,
    pub total_fees: Decimal,
    pub trade_count: u64,
    #[serde(default = "Utc::now")]
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub paused: bool,
    /// Pairs on post-liquidation cooldown: pair → cooldown expiry time.
    #[serde(default)]
    pub cooldown_until: HashMap<Ticker, DateTime<Utc>>,
    /// Pairs explicitly disabled (via API or liquidation).
    #[serde(default)]
    pub disabled_pairs: HashSet<Ticker>,
}

impl BotState {
    pub fn position(&self, pair: &Ticker) -> Position {
        self.positions.get(pair).cloned().unwrap_or_default()
    }

    pub fn pair_exposure_usd(&self, pair: &Ticker, price: Decimal) -> Decimal {
        self.position(pair).value_at(price)
    }

    pub fn total_exposure_usd(&self, prices: &HashMap<Ticker, Decimal>) -> Decimal {
        let mut total = Decimal::ZERO;
        for (pair, pos) in &self.positions {
            if let Some(&price) = prices.get(pair) {
                total += pos.value_at(price);
            }
        }
        total
    }
}
