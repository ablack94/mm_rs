use rust_decimal::Decimal;
use std::str::FromStr;

/// Configurable thresholds for the PnL analyzer evaluation loop.
#[derive(Debug, Clone)]
pub struct AnalyzerConfig {
    /// How often the evaluation loop runs (seconds).
    pub eval_interval_secs: u64,

    /// Edge threshold (as fraction, e.g. 0.005 = 0.5%) above which we promote a pair.
    pub high_edge_threshold: Decimal,

    /// Edge threshold below which we demote a pair (reduce exposure).
    pub low_edge_threshold: Decimal,

    /// Sustained negative edge threshold that triggers wind_down.
    pub negative_edge_threshold: Decimal,

    /// Hours with zero trades before disabling a pair.
    pub zero_trades_disable_hours: u64,

    /// Max inventory USD for promoted (high-edge) pairs.
    pub promoted_max_inventory_usd: Decimal,

    /// Max inventory USD for demoted (low-edge) pairs.
    pub demoted_max_inventory_usd: Decimal,

    /// Min spread bps for promoted pairs (tighter spread).
    pub promoted_min_spread_bps: Decimal,

    /// Min spread bps for demoted pairs (wider spread).
    pub demoted_min_spread_bps: Decimal,

    /// Which rolling window (in hours) to use for the primary edge evaluation.
    pub eval_window_hours: u64,

    /// State store base URL.
    pub state_store_url: String,

    /// State store bearer token.
    pub state_store_token: String,
}

impl AnalyzerConfig {
    pub fn from_env() -> Self {
        Self {
            eval_interval_secs: env_u64("EVAL_INTERVAL_SECS", 60),
            high_edge_threshold: env_decimal("HIGH_EDGE_THRESHOLD", "0.005"),
            low_edge_threshold: env_decimal("LOW_EDGE_THRESHOLD", "0.0"),
            negative_edge_threshold: env_decimal("NEGATIVE_EDGE_THRESHOLD", "-0.002"),
            zero_trades_disable_hours: env_u64("ZERO_TRADES_DISABLE_HOURS", 12),
            promoted_max_inventory_usd: env_decimal("PROMOTED_MAX_INVENTORY_USD", "500"),
            demoted_max_inventory_usd: env_decimal("DEMOTED_MAX_INVENTORY_USD", "50"),
            promoted_min_spread_bps: env_decimal("PROMOTED_MIN_SPREAD_BPS", "50"),
            demoted_min_spread_bps: env_decimal("DEMOTED_MIN_SPREAD_BPS", "200"),
            eval_window_hours: env_u64("EVAL_WINDOW_HOURS", 6),
            state_store_url: std::env::var("STATE_STORE_URL")
                .unwrap_or_else(|_| "http://localhost:3040".to_string()),
            state_store_token: std::env::var("STATE_STORE_TOKEN").unwrap_or_default(),
        }
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_decimal(key: &str, default: &str) -> Decimal {
    std::env::var(key)
        .ok()
        .and_then(|v| Decimal::from_str(&v).ok())
        .unwrap_or_else(|| Decimal::from_str(default).unwrap())
}
