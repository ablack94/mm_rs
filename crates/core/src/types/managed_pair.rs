use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use trading_primitives::Ticker;

use crate::engine::quoter::Quoter;
use crate::types::pair::PairInfo;

// Re-export config types from trading-primitives.
pub use trading_primitives::config::{PairState, PairConfig, GlobalDefaults, ResolvedConfig};

/// A self-contained managed pair with state, config, quoter, position, and info.
pub struct ManagedPair {
    pub pair: Ticker,
    pub state: PairState,
    pub config: PairConfig,
    pub quoter: Quoter,
    pub pair_info: Option<PairInfo>,
    /// Retry counter for liquidation attempts.
    pub liq_retry_count: u32,
    /// Consecutive buy fills without any intervening sell fill.
    pub buys_without_sell: u32,
    /// When the pair entered WindDown for stop-loss (for escalation timing).
    pub winddown_since: Option<DateTime<Utc>>,
}

impl ManagedPair {
    /// Create a new managed pair with default config and Active state.
    pub fn new(pair: Ticker, pair_info: Option<PairInfo>) -> Self {
        let quoter = Quoter::new(pair.clone());
        Self {
            pair,
            state: PairState::Active,
            config: PairConfig::default(),
            quoter,
            pair_info,
            liq_retry_count: 0,
            buys_without_sell: 0,
            winddown_since: None,
        }
    }

    /// Create a new managed pair with specific state and config.
    pub fn with_state_and_config(
        pair: Ticker,
        state: PairState,
        config: PairConfig,
        pair_info: Option<PairInfo>,
    ) -> Self {
        let quoter = Quoter::new(pair.clone());
        Self {
            pair,
            state,
            config,
            quoter,
            pair_info,
            liq_retry_count: 0,
            buys_without_sell: 0,
            winddown_since: None,
        }
    }

    /// Resolve this pair's config against global defaults.
    pub fn resolved_config(&self, defaults: &GlobalDefaults) -> ResolvedConfig {
        ResolvedConfig {
            order_size_usd: self.config.order_size_usd.unwrap_or(defaults.order_size_usd),
            max_inventory_usd: self.config.max_inventory_usd.unwrap_or(defaults.max_inventory_usd),
            min_spread_bps: self.config.min_spread_bps.unwrap_or(defaults.min_spread_bps),
            spread_capture_pct: self.config.spread_capture_pct.unwrap_or(defaults.spread_capture_pct),
            min_profit_pct: self.config.min_profit_pct.unwrap_or(defaults.min_profit_pct),
            stop_loss_pct: self.config.stop_loss_pct.unwrap_or(defaults.stop_loss_pct),
            take_profit_pct: self.config.take_profit_pct.unwrap_or(defaults.take_profit_pct),
            max_buys_before_sell: self.config.max_buys_before_sell.unwrap_or(defaults.max_buys_before_sell),
            use_winddown_for_stoploss: self.config.use_winddown_for_stoploss.unwrap_or(defaults.use_winddown_for_stoploss),
            limit_unwind_on_stoploss: self.config.limit_unwind_on_stoploss.unwrap_or(defaults.limit_unwind_on_stoploss),
        }
    }

    /// Convenience: resolved order size.
    pub fn resolved_order_size(&self, defaults: &GlobalDefaults) -> Decimal {
        self.config.order_size_usd.unwrap_or(defaults.order_size_usd)
    }

    /// Convenience: resolved max inventory.
    pub fn resolved_max_inventory(&self, defaults: &GlobalDefaults) -> Decimal {
        self.config.max_inventory_usd.unwrap_or(defaults.max_inventory_usd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_pair_state_allows_buys() {
        assert!(PairState::Active.allows_buys());
        assert!(!PairState::WindDown.allows_buys());
        assert!(!PairState::Liquidating.allows_buys());
        assert!(!PairState::Disabled.allows_buys());
    }

    #[test]
    fn test_pair_state_allows_sells() {
        assert!(PairState::Active.allows_sells());
        assert!(PairState::WindDown.allows_sells());
        assert!(!PairState::Liquidating.allows_sells());
        assert!(!PairState::Disabled.allows_sells());
    }

    #[test]
    fn test_pair_state_allows_quoting() {
        assert!(PairState::Active.allows_quoting());
        assert!(PairState::WindDown.allows_quoting());
        assert!(!PairState::Liquidating.allows_quoting());
        assert!(!PairState::Disabled.allows_quoting());
    }

    #[test]
    fn test_resolved_config_uses_defaults() {
        let defaults = GlobalDefaults::default();
        let pair = ManagedPair::new("TEST/USD".into(), None);
        let resolved = pair.resolved_config(&defaults);
        assert_eq!(resolved.order_size_usd, defaults.order_size_usd);
        assert_eq!(resolved.max_inventory_usd, defaults.max_inventory_usd);
    }

    #[test]
    fn test_resolved_config_uses_overrides() {
        let defaults = GlobalDefaults::default();
        let mut pair = ManagedPair::new("TEST/USD".into(), None);
        pair.config.order_size_usd = Some(dec!(75));
        pair.config.max_inventory_usd = Some(dec!(500));
        let resolved = pair.resolved_config(&defaults);
        assert_eq!(resolved.order_size_usd, dec!(75));
        assert_eq!(resolved.max_inventory_usd, dec!(500));
        // Unset fields use defaults
        assert_eq!(resolved.min_spread_bps, defaults.min_spread_bps);
    }

    #[test]
    fn test_pair_state_serde() {
        let s = serde_json::to_string(&PairState::WindDown).unwrap();
        assert_eq!(s, "\"wind_down\"");
        let parsed: PairState = serde_json::from_str("\"liquidating\"").unwrap();
        assert_eq!(parsed, PairState::Liquidating);
    }
}
