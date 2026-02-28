use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::BTreeMap;

/// A price-level update from the order book.
#[derive(Debug, Clone)]
pub struct LevelUpdate {
    pub price: Decimal,
    pub qty: Decimal,
}

/// A single price level (best bid or best ask) with named fields.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BookLevel {
    pub price: Decimal,
    pub qty: Decimal,
}

/// Spread between best ask and best bid, with convenience methods.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spread {
    ask: Decimal,
    bid: Decimal,
}

impl Spread {
    pub fn new(ask: Decimal, bid: Decimal) -> Self {
        Self { ask, bid }
    }

    pub fn as_currency(&self) -> Decimal {
        self.ask - self.bid
    }

    pub fn mid(&self) -> Decimal {
        (self.ask + self.bid) / dec!(2)
    }

    pub fn as_pct(&self) -> Decimal {
        self.as_currency() / self.mid()
    }

    pub fn as_bps(&self) -> Decimal {
        self.as_pct() * dec!(10000)
    }
}

/// Order book backed by BTreeMap for sorted price levels.
///
/// Bids: highest price = best bid (last entry in ascending BTreeMap).
/// Asks: lowest price = best ask (first entry in ascending BTreeMap).
#[derive(Debug, Clone, Default)]
pub struct OrderBook {
    pub bids: BTreeMap<Decimal, Decimal>,
    pub asks: BTreeMap<Decimal, Decimal>,
}

impl OrderBook {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply_snapshot(
        &mut self,
        bids: impl IntoIterator<Item = (Decimal, Decimal)>,
        asks: impl IntoIterator<Item = (Decimal, Decimal)>,
    ) {
        self.bids.clear();
        self.asks.clear();
        for (price, qty) in bids {
            self.bids.insert(price, qty);
        }
        for (price, qty) in asks {
            self.asks.insert(price, qty);
        }
    }

    pub fn update_bid(&mut self, price: Decimal, qty: Decimal) {
        if qty.is_zero() {
            self.bids.remove(&price);
        } else {
            self.bids.insert(price, qty);
        }
    }

    pub fn update_ask(&mut self, price: Decimal, qty: Decimal) {
        if qty.is_zero() {
            self.asks.remove(&price);
        } else {
            self.asks.insert(price, qty);
        }
    }

    pub fn best_bid(&self) -> Option<BookLevel> {
        self.bids
            .iter()
            .next_back()
            .map(|(&price, &qty)| BookLevel { price, qty })
    }

    pub fn best_ask(&self) -> Option<BookLevel> {
        self.asks
            .iter()
            .next()
            .map(|(&price, &qty)| BookLevel { price, qty })
    }

    pub fn mid_price(&self) -> Option<Decimal> {
        let bid = self.best_bid()?.price;
        let ask = self.best_ask()?.price;
        Some((bid + ask) / dec!(2))
    }

    pub fn spread(&self) -> Option<Spread> {
        let bid = self.best_bid()?.price;
        let ask = self.best_ask()?.price;
        Some(Spread::new(ask, bid))
    }

    pub fn bid_depth(&self) -> usize {
        self.bids.len()
    }

    pub fn ask_depth(&self) -> usize {
        self.asks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bids.is_empty() || self.asks.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_and_best_prices() {
        let mut book = OrderBook::new();
        book.apply_snapshot(
            vec![(dec!(100), dec!(1)), (dec!(99), dec!(2)), (dec!(98), dec!(3))],
            vec![(dec!(101), dec!(1)), (dec!(102), dec!(2)), (dec!(103), dec!(3))],
        );
        assert_eq!(book.best_bid(), Some(BookLevel { price: dec!(100), qty: dec!(1) }));
        assert_eq!(book.best_ask(), Some(BookLevel { price: dec!(101), qty: dec!(1) }));
        assert_eq!(book.mid_price(), Some(dec!(100.5)));
        assert_eq!(book.spread().map(|s| s.as_currency()), Some(dec!(1)));
    }

    #[test]
    fn test_update_removes_zero_qty() {
        let mut book = OrderBook::new();
        book.apply_snapshot(
            vec![(dec!(100), dec!(1)), (dec!(99), dec!(2))],
            vec![(dec!(101), dec!(1))],
        );
        book.update_bid(dec!(100), dec!(0));
        assert_eq!(book.best_bid(), Some(BookLevel { price: dec!(99), qty: dec!(2) }));
    }

    #[test]
    fn test_spread_bps() {
        let mut book = OrderBook::new();
        book.apply_snapshot(
            vec![(dec!(0.10), dec!(100))],
            vec![(dec!(0.12), dec!(100))],
        );
        let bps = book.spread().unwrap().as_bps();
        assert!(bps > dec!(1800) && bps < dec!(1820));
    }

    #[test]
    fn test_empty_book() {
        let book = OrderBook::new();
        assert!(book.is_empty());
        assert_eq!(book.best_bid(), None);
        assert_eq!(book.mid_price(), None);
    }
}
