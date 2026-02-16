use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use crate::config::TradingConfig;
use crate::types::PairInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteState {
    Idle,
    Quoting,
    BidFilled,
    AskFilled,
}

/// Per-pair quoting state and logic.
#[derive(Debug, Clone)]
pub struct Quoter {
    pub symbol: String,
    pub state: QuoteState,
    pub bid_cl_ord_id: Option<String>,
    pub ask_cl_ord_id: Option<String>,
    pub last_mid: Option<Decimal>,
    pub last_quote_time: Option<DateTime<Utc>>,
}

impl Quoter {
    pub fn new(symbol: String) -> Self {
        Self {
            symbol,
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

    pub fn mark_quoted(
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
        self.state = QuoteState::Quoting;
    }

    pub fn mark_bid_filled(&mut self) {
        self.bid_cl_ord_id = None;
        if self.ask_cl_ord_id.is_none() {
            self.state = QuoteState::Idle;
        } else {
            self.state = QuoteState::BidFilled;
        }
    }

    pub fn mark_ask_filled(&mut self) {
        self.ask_cl_ord_id = None;
        if self.bid_cl_ord_id.is_none() {
            self.state = QuoteState::Idle;
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
}
