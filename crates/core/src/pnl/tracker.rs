use anyhow::Result;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::types::position::Position;
use crate::types::fill::Fill;
use crate::types::order::OrderSide;

/// A recorded trade for history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PnlTrade {
    pub timestamp: DateTime<Utc>,
    pub symbol: String,
    pub side: String,
    pub price: Decimal,
    pub qty: Decimal,
    pub fee: Decimal,
    pub pnl: Decimal,
}

/// Summary of P&L state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PnlSummary {
    pub realized_pnl: Decimal,
    pub unrealized_pnl: Decimal,
    pub total_fees: Decimal,
    pub net_pnl: Decimal,
    pub trade_count: u64,
    pub positions: HashMap<String, Position>,
}

/// Core P&L tracker. Maintains positions and trade history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PnlTracker {
    pub positions: HashMap<String, Position>,
    pub realized_pnl: Decimal,
    pub total_fees: Decimal,
    pub trade_count: u64,
    pub recent_trades: Vec<PnlTrade>,
    /// Maximum number of recent trades to keep in memory.
    #[serde(skip)]
    max_trades: usize,
}

impl PnlTracker {
    pub fn new() -> Self {
        Self {
            positions: HashMap::new(),
            realized_pnl: Decimal::ZERO,
            total_fees: Decimal::ZERO,
            trade_count: 0,
            recent_trades: Vec::new(),
            max_trades: 1000,
        }
    }

    /// Apply a fill to the tracker.
    pub fn apply_fill(&mut self, fill: &Fill) {
        let mut pnl = Decimal::ZERO;

        match fill.side {
            OrderSide::Buy => {
                let pos = self.positions.entry(fill.symbol.clone()).or_default();
                pos.apply_buy(fill.qty, fill.price);
            }
            OrderSide::Sell => {
                let pos = self.positions.entry(fill.symbol.clone()).or_default();
                pnl = pos.apply_sell(fill.qty, fill.price) - fill.fee;
                self.realized_pnl += pnl;
                if pos.is_empty() {
                    self.positions.remove(&fill.symbol);
                }
            }
        }

        self.total_fees += fill.fee;
        self.trade_count += 1;

        self.recent_trades.push(PnlTrade {
            timestamp: fill.timestamp,
            symbol: fill.symbol.clone(),
            side: fill.side.to_string(),
            price: fill.price,
            qty: fill.qty,
            fee: fill.fee,
            pnl,
        });

        // Trim old trades
        if self.recent_trades.len() > self.max_trades {
            let drain_count = self.recent_trades.len() - self.max_trades;
            self.recent_trades.drain(..drain_count);
        }
    }

    /// Compute unrealized P&L given current market prices.
    pub fn unrealized_pnl(&self, prices: &HashMap<String, Decimal>) -> Decimal {
        let mut total = Decimal::ZERO;
        for (symbol, pos) in &self.positions {
            if let Some(&price) = prices.get(symbol) {
                total += pos.qty * (price - pos.avg_cost);
            }
        }
        total
    }

    /// Get a full P&L summary.
    pub fn summary(&self, prices: &HashMap<String, Decimal>) -> PnlSummary {
        let unrealized = self.unrealized_pnl(prices);
        PnlSummary {
            realized_pnl: self.realized_pnl,
            unrealized_pnl: unrealized,
            total_fees: self.total_fees,
            net_pnl: self.realized_pnl + unrealized,
            trade_count: self.trade_count,
            positions: self.positions.clone(),
        }
    }

    /// Persist state to a JSON file.
    pub fn save(&self, path: &PathBuf) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_string_pretty(self)?;
        fs::write(&tmp, data)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Load state from a JSON file.
    pub fn load(path: &PathBuf) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let data = fs::read_to_string(path)?;
        let mut tracker: Self = serde_json::from_str(&data)?;
        tracker.max_trades = 1000;
        Ok(Some(tracker))
    }
}

impl Default for PnlTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn make_fill(symbol: &str, side: OrderSide, price: Decimal, qty: Decimal, fee: Decimal) -> Fill {
        Fill {
            order_id: "test".into(),
            cl_ord_id: "test".into(),
            symbol: symbol.into(),
            side,
            price,
            qty,
            fee,
            is_maker: true,
            is_fully_filled: true,
            timestamp: Utc::now(),
        }
    }

    #[test]
    fn test_buy_sell_pnl() {
        let mut tracker = PnlTracker::new();

        tracker.apply_fill(&make_fill("TEST/USD", OrderSide::Buy, dec!(100), dec!(10), dec!(0.23)));
        assert_eq!(tracker.positions["TEST/USD"].qty, dec!(10));

        tracker.apply_fill(&make_fill("TEST/USD", OrderSide::Sell, dec!(120), dec!(10), dec!(0.28)));
        assert!(tracker.positions.is_empty());

        // pnl = 10 * (120 - 100) - 0.28 = 199.72
        assert_eq!(tracker.realized_pnl, dec!(199.72));
        assert_eq!(tracker.total_fees, dec!(0.51));
        assert_eq!(tracker.trade_count, 2);
    }

    #[test]
    fn test_unrealized_pnl() {
        let mut tracker = PnlTracker::new();
        tracker.apply_fill(&make_fill("TEST/USD", OrderSide::Buy, dec!(100), dec!(10), dec!(0)));

        let mut prices = HashMap::new();
        prices.insert("TEST/USD".into(), dec!(110));
        assert_eq!(tracker.unrealized_pnl(&prices), dec!(100));
    }
}
