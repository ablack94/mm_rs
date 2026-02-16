use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::engine::quoter::Quoter;
use crate::types::pair::PairInfo;

/// Per-pair lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairState {
    /// No quoting, no orders. Inert.
    Disabled,
    /// Sell-only (limit sells). Transitions to Disabled when position=0.
    WindDown,
    /// Market sell in-flight. Transitions to Disabled on completion.
    Liquidating,
    /// Normal market-making.
    Active,
}

impl Default for PairState {
    fn default() -> Self {
        PairState::Active
    }
}

impl PairState {
    /// Whether this state allows placing buy orders.
    pub fn allows_buys(&self) -> bool {
        matches!(self, PairState::Active)
    }

    /// Whether this state allows placing sell orders (limit sells).
    pub fn allows_sells(&self) -> bool {
        matches!(self, PairState::Active | PairState::WindDown)
    }

    /// Whether this state allows any quoting at all.
    pub fn allows_quoting(&self) -> bool {
        matches!(self, PairState::Active | PairState::WindDown)
    }
}

/// Per-pair config overrides. `None` means "use global default".
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PairConfig {
    pub order_size_usd: Option<Decimal>,
    pub max_inventory_usd: Option<Decimal>,
    pub min_spread_bps: Option<Decimal>,
    pub spread_capture_pct: Option<Decimal>,
    pub min_profit_pct: Option<Decimal>,
    pub stop_loss_pct: Option<Decimal>,
    pub take_profit_pct: Option<Decimal>,
}

/// Global defaults used when a pair's config field is None.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalDefaults {
    pub order_size_usd: Decimal,
    pub max_inventory_usd: Decimal,
    pub min_spread_bps: Decimal,
    pub spread_capture_pct: Decimal,
    pub min_profit_pct: Decimal,
    pub stop_loss_pct: Decimal,
    pub take_profit_pct: Decimal,
}

impl Default for GlobalDefaults {
    fn default() -> Self {
        use rust_decimal_macros::dec;
        Self {
            order_size_usd: dec!(50),
            max_inventory_usd: dec!(200),
            min_spread_bps: dec!(100),
            spread_capture_pct: dec!(0.50),
            min_profit_pct: dec!(0.01),
            stop_loss_pct: dec!(0.03),
            take_profit_pct: dec!(0.10),
        }
    }
}

/// Fully resolved config for a pair (no Options — all values filled in).
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub order_size_usd: Decimal,
    pub max_inventory_usd: Decimal,
    pub min_spread_bps: Decimal,
    pub spread_capture_pct: Decimal,
    pub min_profit_pct: Decimal,
    pub stop_loss_pct: Decimal,
    pub take_profit_pct: Decimal,
}

/// A self-contained managed pair with state, config, quoter, position, and info.
pub struct ManagedPair {
    pub symbol: String,
    pub state: PairState,
    pub config: PairConfig,
    pub quoter: Quoter,
    pub pair_info: Option<PairInfo>,
    /// Retry counter for liquidation attempts.
    pub liq_retry_count: u32,
}

impl ManagedPair {
    /// Create a new managed pair with default config and Active state.
    pub fn new(symbol: String, pair_info: Option<PairInfo>) -> Self {
        let quoter = Quoter::new(symbol.clone());
        Self {
            symbol,
            state: PairState::Active,
            config: PairConfig::default(),
            quoter,
            pair_info,
            liq_retry_count: 0,
        }
    }

    /// Create a new managed pair with specific state and config.
    pub fn with_state_and_config(
        symbol: String,
        state: PairState,
        config: PairConfig,
        pair_info: Option<PairInfo>,
    ) -> Self {
        let quoter = Quoter::new(symbol.clone());
        Self {
            symbol,
            state,
            config,
            quoter,
            pair_info,
            liq_retry_count: 0,
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
