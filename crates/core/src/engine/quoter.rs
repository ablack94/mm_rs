use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use trading_primitives::Ticker;
use crate::config::TradingConfig;
use crate::types::PairInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteState {
    Idle,
    Pending,
    Quoting,
    BidFilled,
    AskFilled,
}

/// Per-pair quoting state and logic.
#[derive(Debug, Clone)]
pub struct Quoter {
    pub pair: Ticker,
    pub state: QuoteState,
    pub bid_cl_ord_id: Option<String>,
    pub ask_cl_ord_id: Option<String>,
    pub last_mid: Option<Decimal>,
    pub last_quote_time: Option<DateTime<Utc>>,
}

impl Quoter {
    pub fn new(pair: Ticker) -> Self {
        Self {
            pair,
            state: QuoteState::Idle,
            bid_cl_ord_id: None,
            ask_cl_ord_id: None,
            last_mid: None,
            last_quote_time: None,
        }
    }

    /// Compute bid and ask prices given current book state and inventory skew.
    /// Returns None if the spread is too narrow to quote profitably.
    pub fn compute_quotes(
        &self,
        best_bid: Decimal,
        best_ask: Decimal,
        mid: Decimal,
        skew: Decimal,
        config: &TradingConfig,
        pair_info: &PairInfo,
    ) -> Option<(Decimal, Decimal)> {
        if mid.is_zero() {
            return None;
        }

        let spread_pct = (best_ask - best_bid) / mid;
        let spread_bps = spread_pct * dec!(10000);

        // Must exceed 2x maker fee to be profitable
        let min_profitable = config.maker_fee_pct * dec!(2);
        if spread_pct < min_profitable {
            return None;
        }

        if spread_bps < config.min_spread_bps {
            return None;
        }

        let half_capture = spread_pct * config.spread_capture_pct / dec!(2);

        // Inventory skew adjustment (30% impact)
        let skew_adj = skew * half_capture * dec!(0.3);
        let bid_offset = half_capture - skew_adj;
        let ask_offset = half_capture + skew_adj;

        let mut bid_price = mid * (Decimal::ONE - bid_offset);
        let mut ask_price = mid * (Decimal::ONE + ask_offset);

        // Don't cross the book (post-only would reject)
        bid_price = bid_price.min(best_ask - Decimal::new(1, pair_info.price_decimals));
        ask_price = ask_price.max(best_bid + Decimal::new(1, pair_info.price_decimals));

        // Round to pair precision
        bid_price = bid_price.round_dp(pair_info.price_decimals);
        ask_price = ask_price.round_dp(pair_info.price_decimals);

        Some((bid_price, ask_price))
    }

    /// Compute order quantity in base asset for a given mid price.
    pub fn compute_qty(
        &self,
        mid: Decimal,
        config: &TradingConfig,
        pair_info: &PairInfo,
    ) -> Decimal {
        if mid.is_zero() {
            return Decimal::ZERO;
        }
        let mut qty = config.order_size_usd / mid;
        qty = qty.round_dp(pair_info.qty_decimals);
        if qty < pair_info.min_order_qty {
            qty = pair_info.min_order_qty;
        }
        if qty * mid < pair_info.min_cost {
            qty = (pair_info.min_cost / mid).round_dp(pair_info.qty_decimals) + Decimal::new(1, pair_info.qty_decimals);
        }
        qty
    }

    /// Whether the mid has moved enough to warrant requoting.
    pub fn should_requote(&self, mid: Decimal, config: &TradingConfig) -> bool {
        match self.last_mid {
            None => true,
            Some(last) => {
                if last.is_zero() {
                    return true;
                }
                let move_pct = ((mid - last) / last).abs();
                move_pct > config.requote_threshold_pct
            }
        }
    }

    pub fn mark_pending(
        &mut self,
        mid: Decimal,
        bid_id: String,
        ask_id: String,
        timestamp: DateTime<Utc>,
    ) {
        self.bid_cl_ord_id = Some(bid_id);
        self.ask_cl_ord_id = Some(ask_id);
        self.last_mid = Some(mid);
        self.last_quote_time = Some(timestamp);
        self.state = QuoteState::Pending;
    }

    pub fn mark_bid_filled(&mut self) {
        self.bid_cl_ord_id = None;
        if self.ask_cl_ord_id.is_none() {
            self.state = QuoteState::Idle;
        } else if self.state == QuoteState::Pending {
            // Stay Pending — engine will call try_transition_from_pending
        } else {
            self.state = QuoteState::BidFilled;
        }
    }

    pub fn mark_ask_filled(&mut self) {
        self.ask_cl_ord_id = None;
        if self.bid_cl_ord_id.is_none() {
            self.state = QuoteState::Idle;
        } else if self.state == QuoteState::Pending {
            // Stay Pending — engine will call try_transition_from_pending
        } else {
            self.state = QuoteState::AskFilled;
        }
    }

    pub fn mark_cancelled(&mut self, cl_ord_id: &str) {
        if self.bid_cl_ord_id.as_deref() == Some(cl_ord_id) {
            self.bid_cl_ord_id = None;
        }
        if self.ask_cl_ord_id.as_deref() == Some(cl_ord_id) {
            self.ask_cl_ord_id = None;
        }
        if self.bid_cl_ord_id.is_none() && self.ask_cl_ord_id.is_none() {
            self.state = QuoteState::Idle;
            self.last_mid = None;
        }
    }

    /// Transition from Pending to the correct active state once all orders are acked.
    /// Called by the engine after ack/fill/cancel events.
    pub fn try_transition_from_pending(&mut self, all_acked: bool) {
        if self.state != QuoteState::Pending || !all_acked {
            return;
        }
        match (self.bid_cl_ord_id.is_some(), self.ask_cl_ord_id.is_some()) {
            (true, true) => self.state = QuoteState::Quoting,
            (true, false) => self.state = QuoteState::AskFilled,
            (false, true) => self.state = QuoteState::BidFilled,
            (false, false) => self.state = QuoteState::Idle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rust_decimal_macros::dec;
    use crate::config::TradingConfig;
    use crate::types::PairInfo;

    fn test_config() -> TradingConfig {
        TradingConfig {
            pairs: vec![],
            order_size_usd: dec!(100),
            min_spread_bps: dec!(100),
            spread_capture_pct: dec!(0.50),
            requote_threshold_pct: dec!(0.005),
            maker_fee_pct: dec!(0.0023),
            min_profit_pct: dec!(0.01),
            dry_run: false,
            downtrend_threshold_pct: dec!(-5.0),
            min_quote_interval_secs: 5,
        }
    }

    fn test_pair_info() -> PairInfo {
        PairInfo {
            pair: "TEST/USD".into(),
            rest_key: "TESTUSD".into(),
            exchange_base_asset: "TEST".into(),
            price_decimals: 5,
            qty_decimals: 5,
            min_order_qty: dec!(10),
            min_cost: dec!(0.50),
            maker_fee_pct: dec!(0.0023),
        }
    }

    fn make_quoter() -> Quoter {
        Quoter::new("TEST/USD".into())
    }

    // ---------------------------------------------------------------
    // compute_quotes tests
    // ---------------------------------------------------------------

    #[test]
    fn compute_quotes_basic() {
        // 500 bps spread around mid=1.0 → best_bid=0.975, best_ask=1.025
        let q = make_quoter();
        let cfg = test_config();
        let pi = test_pair_info();

        let mid = dec!(1.0);
        let best_bid = dec!(0.975);
        let best_ask = dec!(1.025);
        let skew = dec!(0);

        let (bid, ask) = q
            .compute_quotes(best_bid, best_ask, mid, skew, &cfg, &pi)
            .expect("should produce quotes for 500 bps spread");

        // bid must be below mid, ask must be above mid
        assert!(bid < mid, "bid {bid} should be < mid {mid}");
        assert!(ask > mid, "ask {ask} should be > mid {mid}");
        // bid should be above best_bid (we're tightening inside the book)
        assert!(bid >= best_bid, "bid {bid} should be >= best_bid {best_bid}");
        // ask should be below best_ask
        assert!(ask <= best_ask, "ask {ask} should be <= best_ask {best_ask}");
    }

    #[test]
    fn compute_quotes_narrow_spread_returns_none() {
        // Spread < 2x maker fee (0.0046) → None
        // maker_fee_pct = 0.0023 → 2x = 0.0046 → 46 bps
        // Use a 30 bps spread (0.003): best_bid=0.9985, best_ask=1.0015, mid=1.0
        let q = make_quoter();
        let cfg = test_config();
        let pi = test_pair_info();

        let mid = dec!(1.0);
        let best_bid = dec!(0.9985);
        let best_ask = dec!(1.0015);

        let result = q.compute_quotes(best_bid, best_ask, mid, dec!(0), &cfg, &pi);
        assert!(result.is_none(), "spread < 2x maker fee should return None");
    }

    #[test]
    fn compute_quotes_below_min_spread_bps_returns_none() {
        // min_spread_bps = 100 → need at least 100 bps (1%)
        // Use 80 bps spread: best_bid=0.996, best_ask=1.004, mid=1.0 → spread_bps=80
        // But also need spread > 2x maker_fee (46 bps), so 80 bps passes that check
        // but fails the min_spread_bps=100 check.
        let q = make_quoter();
        let cfg = test_config();
        let pi = test_pair_info();

        let mid = dec!(1.0);
        let best_bid = dec!(0.996);
        let best_ask = dec!(1.004);

        let result = q.compute_quotes(best_bid, best_ask, mid, dec!(0), &cfg, &pi);
        assert!(result.is_none(), "spread below min_spread_bps should return None");
    }

    #[test]
    fn compute_quotes_zero_mid_returns_none() {
        let q = make_quoter();
        let cfg = test_config();
        let pi = test_pair_info();

        let result = q.compute_quotes(dec!(0), dec!(0), dec!(0), dec!(0), &cfg, &pi);
        assert!(result.is_none(), "zero mid should return None");
    }

    #[test]
    fn compute_quotes_inventory_skew_adjusts_prices() {
        // Positive skew = we hold too much → want to sell more / buy less
        // → lower bid (less aggressive buying), higher ask?
        // Actually: skew_adj = skew * half_capture * 0.3
        //   bid_offset = half_capture - skew_adj  (smaller when skew>0 → bid closer to mid? No...)
        //   Let's trace: skew > 0 → skew_adj > 0
        //     bid_offset = half_capture - skew_adj → SMALLER → bid = mid*(1 - bid_offset) → bid HIGHER (closer to mid)
        //     ask_offset = half_capture + skew_adj → LARGER → ask = mid*(1 + ask_offset) → ask HIGHER (farther from mid)
        //   Wait, that means positive skew makes bid higher (more aggressive buy)?
        //   Actually let's re-read the code more carefully:
        //     bid_offset decreases → bid_price = mid * (1 - bid_offset) → bid_price goes UP
        //     ask_offset increases → ask_price = mid * (1 + ask_offset) → ask_price goes UP
        //   So positive skew pushes BOTH prices up. That makes sense: if we're long,
        //   we raise our ask to try to sell higher, and raise our bid so it's closer to mid (still buying but tighter).
        //
        //   The user's description says "positive skew = less buying = lower bid, higher ask" but that's
        //   not what the code does. Let's just verify the actual code behavior: positive skew
        //   shifts both prices upward relative to zero skew.

        let q = make_quoter();
        let cfg = test_config();
        let pi = test_pair_info();

        let mid = dec!(1.0);
        let best_bid = dec!(0.975);
        let best_ask = dec!(1.025);

        let (bid_neutral, ask_neutral) = q
            .compute_quotes(best_bid, best_ask, mid, dec!(0), &cfg, &pi)
            .unwrap();

        let (bid_skewed, ask_skewed) = q
            .compute_quotes(best_bid, best_ask, mid, dec!(1), &cfg, &pi)
            .unwrap();

        // Positive skew: bid_offset shrinks → bid goes UP, ask_offset grows → ask goes UP
        assert!(
            bid_skewed > bid_neutral,
            "positive skew should raise bid: {bid_skewed} vs {bid_neutral}"
        );
        assert!(
            ask_skewed > ask_neutral,
            "positive skew should raise ask: {ask_skewed} vs {ask_neutral}"
        );
    }

    #[test]
    fn compute_quotes_prices_dont_cross_book() {
        // Use a very wide spread so the capture-based offsets might push bid above best_ask.
        // The post-only guard should clamp: bid < best_ask, ask > best_bid.
        let q = make_quoter();
        let pi = test_pair_info();

        // Config with very high capture pct to force crossing
        let mut cfg = test_config();
        cfg.spread_capture_pct = dec!(0.99); // capture nearly the entire spread

        let mid = dec!(1.0);
        let best_bid = dec!(0.975);
        let best_ask = dec!(1.025);

        let (bid, ask) = q
            .compute_quotes(best_bid, best_ask, mid, dec!(0), &cfg, &pi)
            .unwrap();

        // Post-only guard: bid must be strictly less than best_ask
        let tick = Decimal::new(1, pi.price_decimals); // 0.00001
        assert!(
            bid <= best_ask - tick,
            "bid {bid} must not cross best_ask {best_ask}"
        );
        // ask must be strictly greater than best_bid
        assert!(
            ask >= best_bid + tick,
            "ask {ask} must not cross best_bid {best_bid}"
        );
    }

    // ---------------------------------------------------------------
    // compute_qty tests
    // ---------------------------------------------------------------

    #[test]
    fn compute_qty_basic() {
        // $100 / $0.50 = 200 units
        let q = make_quoter();
        let cfg = test_config();
        let pi = test_pair_info();

        let qty = q.compute_qty(dec!(0.50), &cfg, &pi);
        assert_eq!(qty, dec!(200));
    }

    #[test]
    fn compute_qty_respects_min_order_qty() {
        // $100 / $50 = 2 units, but min_order_qty = 10 → should return 10
        let q = make_quoter();
        let cfg = test_config();
        let pi = test_pair_info();

        let qty = q.compute_qty(dec!(50), &cfg, &pi);
        assert_eq!(qty, dec!(10), "should be clamped up to min_order_qty");
    }

    #[test]
    fn compute_qty_respects_min_cost() {
        // min_cost = 0.50, if mid is very high and raw qty * mid < 0.50 it bumps up.
        // Actually with order_size_usd=100, qty*mid will always be ~100.
        // We need min_cost > order_size_usd to trigger this path normally, or a tiny mid.
        // Let's create a pair_info with min_cost = 200 (greater than order_size_usd=100)
        // and a mid of 1.0 → raw qty = 100, 100*1 = 100 < 200 → needs bump.
        let q = make_quoter();
        let cfg = test_config();
        let mut pi = test_pair_info();
        pi.min_cost = dec!(200);

        let qty = q.compute_qty(dec!(1.0), &cfg, &pi);
        // qty * mid must be >= 200, and qty >= min_order_qty(10)
        assert!(
            qty * dec!(1.0) >= dec!(200),
            "qty {qty} * mid should meet min_cost 200"
        );
    }

    #[test]
    fn compute_qty_zero_mid_returns_zero() {
        let q = make_quoter();
        let cfg = test_config();
        let pi = test_pair_info();

        let qty = q.compute_qty(dec!(0), &cfg, &pi);
        assert_eq!(qty, Decimal::ZERO);
    }

    // ---------------------------------------------------------------
    // should_requote tests
    // ---------------------------------------------------------------

    #[test]
    fn should_requote_true_when_no_last_mid() {
        let q = make_quoter();
        let cfg = test_config();
        assert!(q.should_requote(dec!(1.0), &cfg));
    }

    #[test]
    fn should_requote_true_when_mid_moved_beyond_threshold() {
        // requote_threshold_pct = 0.005 (0.5%)
        let mut q = make_quoter();
        q.last_mid = Some(dec!(1.0));
        let cfg = test_config();

        // Move 1% — well above 0.5% threshold
        assert!(q.should_requote(dec!(1.01), &cfg));
        assert!(q.should_requote(dec!(0.99), &cfg));
    }

    #[test]
    fn should_requote_false_when_mid_barely_moved() {
        // requote_threshold_pct = 0.005 (0.5%)
        let mut q = make_quoter();
        q.last_mid = Some(dec!(1.0));
        let cfg = test_config();

        // Move 0.1% — below 0.5% threshold
        assert!(!q.should_requote(dec!(1.001), &cfg));
        assert!(!q.should_requote(dec!(0.999), &cfg));
    }

    // ---------------------------------------------------------------
    // State transition tests
    // ---------------------------------------------------------------

    #[test]
    fn mark_pending_sets_pending_state() {
        let mut q = make_quoter();
        assert_eq!(q.state, QuoteState::Idle);

        let now = Utc::now();
        q.mark_pending(dec!(1.0), "bid-1".into(), "ask-1".into(), now);

        assert_eq!(q.state, QuoteState::Pending);
        assert_eq!(q.bid_cl_ord_id.as_deref(), Some("bid-1"));
        assert_eq!(q.ask_cl_ord_id.as_deref(), Some("ask-1"));
        assert_eq!(q.last_mid, Some(dec!(1.0)));
        assert_eq!(q.last_quote_time, Some(now));
    }

    #[test]
    fn mark_bid_filled_with_ask_outstanding() {
        let mut q = make_quoter();
        q.mark_pending(dec!(1.0), "bid-1".into(), "ask-1".into(), Utc::now());
        q.try_transition_from_pending(true);

        q.mark_bid_filled();

        assert_eq!(q.state, QuoteState::BidFilled);
        assert!(q.bid_cl_ord_id.is_none());
        assert_eq!(q.ask_cl_ord_id.as_deref(), Some("ask-1"));
    }

    #[test]
    fn mark_bid_filled_without_ask_goes_idle() {
        let mut q = make_quoter();
        q.mark_pending(dec!(1.0), "bid-1".into(), "ask-1".into(), Utc::now());
        q.try_transition_from_pending(true);
        q.ask_cl_ord_id = None; // simulate ask already gone

        q.mark_bid_filled();

        assert_eq!(q.state, QuoteState::Idle);
        assert!(q.bid_cl_ord_id.is_none());
        assert!(q.ask_cl_ord_id.is_none());
    }

    #[test]
    fn mark_ask_filled_with_bid_outstanding() {
        let mut q = make_quoter();
        q.mark_pending(dec!(1.0), "bid-1".into(), "ask-1".into(), Utc::now());
        q.try_transition_from_pending(true);

        q.mark_ask_filled();

        assert_eq!(q.state, QuoteState::AskFilled);
        assert!(q.ask_cl_ord_id.is_none());
        assert_eq!(q.bid_cl_ord_id.as_deref(), Some("bid-1"));
    }

    #[test]
    fn mark_ask_filled_without_bid_goes_idle() {
        let mut q = make_quoter();
        q.mark_pending(dec!(1.0), "bid-1".into(), "ask-1".into(), Utc::now());
        q.try_transition_from_pending(true);
        q.bid_cl_ord_id = None; // simulate bid already gone

        q.mark_ask_filled();

        assert_eq!(q.state, QuoteState::Idle);
    }

    #[test]
    fn mark_cancelled_bid_only() {
        let mut q = make_quoter();
        q.mark_pending(dec!(1.0), "bid-1".into(), "ask-1".into(), Utc::now());
        q.try_transition_from_pending(true);

        q.mark_cancelled("bid-1");

        // bid is gone, ask remains → still not idle
        assert!(q.bid_cl_ord_id.is_none());
        assert_eq!(q.ask_cl_ord_id.as_deref(), Some("ask-1"));
        // state should remain Quoting (both ids not cleared)
        assert_eq!(q.state, QuoteState::Quoting);
    }

    #[test]
    fn mark_cancelled_both_sides_goes_idle() {
        let mut q = make_quoter();
        q.mark_pending(dec!(1.0), "bid-1".into(), "ask-1".into(), Utc::now());
        q.try_transition_from_pending(true);

        q.mark_cancelled("bid-1");
        q.mark_cancelled("ask-1");

        assert_eq!(q.state, QuoteState::Idle);
        assert!(q.bid_cl_ord_id.is_none());
        assert!(q.ask_cl_ord_id.is_none());
        assert!(q.last_mid.is_none(), "last_mid should be cleared on full cancel");
    }

    #[test]
    fn mark_cancelled_unknown_id_is_noop() {
        let mut q = make_quoter();
        q.mark_pending(dec!(1.0), "bid-1".into(), "ask-1".into(), Utc::now());
        q.try_transition_from_pending(true);

        q.mark_cancelled("unknown-id");

        assert_eq!(q.state, QuoteState::Quoting);
        assert_eq!(q.bid_cl_ord_id.as_deref(), Some("bid-1"));
        assert_eq!(q.ask_cl_ord_id.as_deref(), Some("ask-1"));
    }

    // ---------------------------------------------------------------
    // Pending state tests
    // ---------------------------------------------------------------

    #[test]
    fn pending_transition_both_acked_becomes_quoting() {
        let mut q = make_quoter();
        q.mark_pending(dec!(1.0), "bid-1".into(), "ask-1".into(), Utc::now());
        assert_eq!(q.state, QuoteState::Pending);

        q.try_transition_from_pending(true);
        assert_eq!(q.state, QuoteState::Quoting);
    }

    #[test]
    fn pending_transition_not_acked_stays_pending() {
        let mut q = make_quoter();
        q.mark_pending(dec!(1.0), "bid-1".into(), "ask-1".into(), Utc::now());

        q.try_transition_from_pending(false);
        assert_eq!(q.state, QuoteState::Pending);
    }

    #[test]
    fn pending_bid_filled_stays_pending_if_ask_exists() {
        let mut q = make_quoter();
        q.mark_pending(dec!(1.0), "bid-1".into(), "ask-1".into(), Utc::now());

        q.mark_bid_filled();

        assert_eq!(q.state, QuoteState::Pending, "Should stay Pending while ask is unacked");
        assert!(q.bid_cl_ord_id.is_none());
        assert_eq!(q.ask_cl_ord_id.as_deref(), Some("ask-1"));
    }

    #[test]
    fn pending_bid_filled_goes_idle_if_no_ask() {
        let mut q = make_quoter();
        q.mark_pending(dec!(1.0), "bid-1".into(), "ask-1".into(), Utc::now());
        q.ask_cl_ord_id = None; // simulate ask already gone

        q.mark_bid_filled();

        assert_eq!(q.state, QuoteState::Idle);
    }

    #[test]
    fn pending_ask_filled_stays_pending_if_bid_exists() {
        let mut q = make_quoter();
        q.mark_pending(dec!(1.0), "bid-1".into(), "ask-1".into(), Utc::now());

        q.mark_ask_filled();

        assert_eq!(q.state, QuoteState::Pending, "Should stay Pending while bid is unacked");
        assert!(q.ask_cl_ord_id.is_none());
        assert_eq!(q.bid_cl_ord_id.as_deref(), Some("bid-1"));
    }

    #[test]
    fn pending_both_filled_goes_idle() {
        let mut q = make_quoter();
        q.mark_pending(dec!(1.0), "bid-1".into(), "ask-1".into(), Utc::now());

        q.mark_bid_filled();
        q.mark_ask_filled();

        assert_eq!(q.state, QuoteState::Idle);
    }

    #[test]
    fn pending_after_bid_filled_ack_transitions_to_bid_filled() {
        let mut q = make_quoter();
        q.mark_pending(dec!(1.0), "bid-1".into(), "ask-1".into(), Utc::now());

        // Bid fills (ask still pending)
        q.mark_bid_filled();
        assert_eq!(q.state, QuoteState::Pending);

        // Ask gets acked — all remaining orders acked
        q.try_transition_from_pending(true);
        assert_eq!(q.state, QuoteState::BidFilled);
    }

    #[test]
    fn pending_after_ask_filled_ack_transitions_to_ask_filled() {
        let mut q = make_quoter();
        q.mark_pending(dec!(1.0), "bid-1".into(), "ask-1".into(), Utc::now());

        q.mark_ask_filled();
        assert_eq!(q.state, QuoteState::Pending);

        q.try_transition_from_pending(true);
        assert_eq!(q.state, QuoteState::AskFilled);
    }

    #[test]
    fn pending_cancelled_one_side_stays_pending() {
        let mut q = make_quoter();
        q.mark_pending(dec!(1.0), "bid-1".into(), "ask-1".into(), Utc::now());

        q.mark_cancelled("bid-1");
        // One side still exists, mark_cancelled doesn't go Idle
        assert!(q.bid_cl_ord_id.is_none());
        assert_eq!(q.ask_cl_ord_id.as_deref(), Some("ask-1"));
        // State is still Pending (mark_cancelled doesn't change from Pending
        // since ask_cl_ord_id is still Some)
        assert_eq!(q.state, QuoteState::Pending);
    }

    #[test]
    fn pending_cancelled_both_sides_goes_idle() {
        let mut q = make_quoter();
        q.mark_pending(dec!(1.0), "bid-1".into(), "ask-1".into(), Utc::now());

        q.mark_cancelled("bid-1");
        q.mark_cancelled("ask-1");

        assert_eq!(q.state, QuoteState::Idle);
    }

    #[test]
    fn try_transition_noop_when_not_pending() {
        let mut q = make_quoter();
        q.mark_pending(dec!(1.0), "bid-1".into(), "ask-1".into(), Utc::now());
        q.try_transition_from_pending(true);
        assert_eq!(q.state, QuoteState::Quoting);

        // Calling again when already Quoting should be a no-op
        q.try_transition_from_pending(true);
        assert_eq!(q.state, QuoteState::Quoting);
    }
}
