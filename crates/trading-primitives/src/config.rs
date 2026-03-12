use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

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
    /// Maximum consecutive buy fills before a sell is required.
    pub max_buys_before_sell: Option<u32>,
    /// Whether stop-loss should use WindDown (limit sells) instead of market liquidation.
    pub use_winddown_for_stoploss: Option<bool>,
    /// Whether to bypass cost floor during stop-loss-triggered WindDown (sell at market as limit order).
    pub limit_unwind_on_stoploss: Option<bool>,
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
    #[serde(default = "default_max_buys_before_sell")]
    pub max_buys_before_sell: u32,
    #[serde(default = "default_use_winddown_for_stoploss")]
    pub use_winddown_for_stoploss: bool,
    #[serde(default)]
    pub limit_unwind_on_stoploss: bool,
}

fn default_max_buys_before_sell() -> u32 {
    2
}

fn default_use_winddown_for_stoploss() -> bool {
    true
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
            max_buys_before_sell: 2,
            use_winddown_for_stoploss: true,
            limit_unwind_on_stoploss: false,
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
    pub max_buys_before_sell: u32,
    pub use_winddown_for_stoploss: bool,
    pub limit_unwind_on_stoploss: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_pair_state_serde() {
        let s = serde_json::to_string(&PairState::WindDown).unwrap();
        assert_eq!(s, "\"wind_down\"");
        let parsed: PairState = serde_json::from_str("\"liquidating\"").unwrap();
        assert_eq!(parsed, PairState::Liquidating);
    }

    #[test]
    fn test_global_defaults_serde_roundtrip() {
        let defaults = GlobalDefaults::default();
        let json = serde_json::to_string(&defaults).unwrap();
        let parsed: GlobalDefaults = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.order_size_usd, defaults.order_size_usd);
        assert_eq!(parsed.max_buys_before_sell, defaults.max_buys_before_sell);
    }
}
