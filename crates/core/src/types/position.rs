use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Position {
    pub qty: Decimal,
    pub avg_cost: Decimal,
}

impl Position {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply_buy(&mut self, qty: Decimal, price: Decimal) {
        let new_qty = self.qty + qty;
        if new_qty > Decimal::ZERO {
            self.avg_cost = (self.qty * self.avg_cost + qty * price) / new_qty;
        }
        self.qty = new_qty;
    }

    /// Apply a sell. Returns the realized P&L (excluding fees).
    pub fn apply_sell(&mut self, qty: Decimal, price: Decimal) -> Decimal {
        let pnl = qty * (price - self.avg_cost);
        self.qty -= qty;
        if self.qty <= Decimal::ZERO {
            self.qty = Decimal::ZERO;
            self.avg_cost = Decimal::ZERO;
        }
        pnl
    }

    pub fn value_at(&self, price: Decimal) -> Decimal {
        self.qty * price
    }

    pub fn is_empty(&self) -> bool {
        self.qty <= Decimal::ZERO
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_buy_updates_avg_cost() {
        let mut pos = Position::new();
        pos.apply_buy(dec!(10), dec!(100));
        assert_eq!(pos.qty, dec!(10));
        assert_eq!(pos.avg_cost, dec!(100));

        pos.apply_buy(dec!(10), dec!(200));
        assert_eq!(pos.qty, dec!(20));
        assert_eq!(pos.avg_cost, dec!(150));
    }

    #[test]
    fn test_sell_returns_pnl() {
        let mut pos = Position::new();
        pos.apply_buy(dec!(10), dec!(100));
        let pnl = pos.apply_sell(dec!(5), dec!(120));
        assert_eq!(pnl, dec!(100));
        assert_eq!(pos.qty, dec!(5));
        assert_eq!(pos.avg_cost, dec!(100));
    }

    #[test]
    fn test_full_sell_resets() {
        let mut pos = Position::new();
        pos.apply_buy(dec!(10), dec!(50));
        pos.apply_sell(dec!(10), dec!(60));
        assert!(pos.is_empty());
        assert_eq!(pos.avg_cost, Decimal::ZERO);
    }
}
