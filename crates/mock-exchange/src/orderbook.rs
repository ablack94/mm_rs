use ordered_float::OrderedFloat;
use rand::Rng;
use std::collections::BTreeMap;

/// Simulated order book for a single pair.
///
/// Uses BTreeMap for sorted price levels. Mid price is derived from
/// best bid/ask, not stored.
///
/// Bids: ascending BTreeMap, best bid = last entry.
/// Asks: ascending BTreeMap, best ask = first entry.
#[derive(Debug)]
pub struct OrderBook {
    pub bids: BTreeMap<OrderedFloat<f64>, f64>,
    pub asks: BTreeMap<OrderedFloat<f64>, f64>,
    /// Spread percentage used for level generation.
    spread_pct: f64,
    /// Target mid for the random walk simulation driver.
    /// Not the "true" mid — use `mid_price()` for the derived value.
    target_mid: f64,
}

impl OrderBook {
    pub fn new(starting_price: f64, spread_pct: f64) -> Self {
        let mut book = OrderBook {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            spread_pct,
            target_mid: starting_price,
        };
        book.regenerate_levels();
        book
    }

    /// Derived mid price from best bid and best ask.
    pub fn mid_price(&self) -> f64 {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => (bid + ask) / 2.0,
            _ => self.target_mid,
        }
    }

    /// Best bid price (highest bid).
    pub fn best_bid(&self) -> Option<f64> {
        self.bids.keys().next_back().map(|p| p.into_inner())
    }

    /// Best ask price (lowest ask).
    pub fn best_ask(&self) -> Option<f64> {
        self.asks.keys().next().map(|p| p.into_inner())
    }

    /// Regenerate the full book with 10 levels on each side.
    fn regenerate_levels(&mut self) {
        let half_spread = self.target_mid * self.spread_pct / 200.0;
        let top_bid = self.target_mid - half_spread;
        let top_ask = self.target_mid + half_spread;
        let level_step = self.target_mid * 0.005;

        self.bids.clear();
        self.asks.clear();

        let mut rng = rand::thread_rng();
        let base_qty = base_qty_for_price(self.target_mid);

        for i in 0..10 {
            let bid_price = top_bid - (i as f64) * level_step;
            if bid_price > 0.0 {
                let qty = base_qty * rng.gen_range(0.5..3.0);
                self.bids.insert(OrderedFloat(bid_price), qty);
            }

            let ask_price = top_ask + (i as f64) * level_step;
            let qty = base_qty * rng.gen_range(0.5..3.0);
            self.asks.insert(OrderedFloat(ask_price), qty);
        }
    }

    /// Set the mid price to a specific value and regenerate levels.
    /// Used by the deterministic scenario driver.
    pub fn set_mid(&mut self, mid: f64, spread_pct_override: Option<f64>) {
        self.target_mid = mid.max(0.0000001);
        if let Some(sp) = spread_pct_override {
            self.spread_pct = sp;
        }
        self.regenerate_levels();
    }

    /// Apply a random walk to the mid price and regenerate the book.
    pub fn random_walk(&mut self, volatility_pct: f64, rng: &mut impl Rng) {
        let change = rng.gen_range(-volatility_pct..volatility_pct) / 100.0;
        self.target_mid *= 1.0 + change;
        if self.target_mid < 0.0000001 {
            self.target_mid = 0.0000001;
        }
        self.regenerate_levels();
    }
}

/// Generate realistic base quantity for a given price level.
fn base_qty_for_price(price: f64) -> f64 {
    if price >= 1.0 {
        50.0
    } else if price >= 0.01 {
        5000.0
    } else if price >= 0.001 {
        50000.0
    } else {
        500000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_book_has_levels() {
        let book = OrderBook::new(0.50, 5.0);
        assert_eq!(book.bids.len(), 10);
        assert_eq!(book.asks.len(), 10);
    }

    #[test]
    fn test_mid_price_derived() {
        let book = OrderBook::new(0.50, 5.0);
        let mid = book.mid_price();
        assert!((mid - 0.50).abs() < 0.05, "mid={mid}, expected ~0.50");
    }

    #[test]
    fn test_bids_descending_asks_ascending() {
        let book = OrderBook::new(0.004, 5.0);

        // BTreeMap is ascending. Best bid = last key.
        let bid_prices: Vec<f64> = book.bids.keys().map(|p| p.into_inner()).collect();
        for i in 1..bid_prices.len() {
            assert!(bid_prices[i] > bid_prices[i - 1], "bids not ascending in BTreeMap");
        }
        // Best bid should be the highest
        assert_eq!(book.best_bid(), Some(bid_prices[bid_prices.len() - 1]));

        // Best ask = first key
        let ask_prices: Vec<f64> = book.asks.keys().map(|p| p.into_inner()).collect();
        for i in 1..ask_prices.len() {
            assert!(ask_prices[i] > ask_prices[i - 1], "asks not ascending in BTreeMap");
        }
        assert_eq!(book.best_ask(), Some(ask_prices[0]));
    }

    #[test]
    fn test_best_bid_below_best_ask() {
        let book = OrderBook::new(0.004, 5.0);
        let bid = book.best_bid().unwrap();
        let ask = book.best_ask().unwrap();
        assert!(bid < ask, "bid={bid} should be < ask={ask}");
    }

    #[test]
    fn test_random_walk_changes_mid() {
        let mut book = OrderBook::new(0.50, 5.0);
        let mid_before = book.mid_price();
        let mut rng = rand::thread_rng();
        // Walk 100 times — extremely unlikely to end at exact same mid
        for _ in 0..100 {
            book.random_walk(1.0, &mut rng);
        }
        let mid_after = book.mid_price();
        assert!((mid_before - mid_after).abs() > 0.0001, "mid didn't change after 100 walks");
    }

    #[test]
    fn test_empty_book_fallback_mid() {
        let mut book = OrderBook::new(0.50, 5.0);
        book.bids.clear();
        book.asks.clear();
        // Falls back to target_mid when book is empty
        assert!((book.mid_price() - 0.50).abs() < 0.05);
    }

    #[test]
    fn test_camp_price_range() {
        let book = OrderBook::new(0.004, 5.0);
        let bid = book.best_bid().unwrap();
        let ask = book.best_ask().unwrap();
        // With mid=0.004, 5% spread: bid≈0.0039, ask≈0.0041
        assert!((bid - 0.0039).abs() < 0.0002, "CAMP bid={bid}");
        assert!((ask - 0.0041).abs() < 0.0002, "CAMP ask={ask}");
    }

    #[test]
    fn test_set_mid_updates_book() {
        let mut book = OrderBook::new(0.50, 5.0);
        book.set_mid(0.60, None);
        let mid = book.mid_price();
        assert!(
            (mid - 0.60).abs() < 0.05,
            "mid={mid}, expected ~0.60 after set_mid"
        );
        let bid = book.best_bid().unwrap();
        let ask = book.best_ask().unwrap();
        assert!(bid < ask, "bid={bid} should be < ask={ask}");
    }

    #[test]
    fn test_set_mid_with_spread_override() {
        let mut book_wide = OrderBook::new(0.50, 5.0);
        book_wide.set_mid(0.50, Some(10.0));
        let spread_wide = book_wide.best_ask().unwrap() - book_wide.best_bid().unwrap();

        let mut book_narrow = OrderBook::new(0.50, 5.0);
        book_narrow.set_mid(0.50, Some(2.0));
        let spread_narrow = book_narrow.best_ask().unwrap() - book_narrow.best_bid().unwrap();

        assert!(
            spread_wide > spread_narrow,
            "wide spread {spread_wide} should be > narrow {spread_narrow}"
        );
    }
}
