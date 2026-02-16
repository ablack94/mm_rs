use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub exchange: ExchangeConfig,
    pub trading: TradingConfig,
    pub risk: RiskConfig,
    pub persistence: PersistenceConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeConfig {
    pub api_key: String,
    pub api_secret: String,
    pub ws_public_url: String,
    pub ws_private_url: String,
    pub rest_base_url: String,
    pub book_depth: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradingConfig {
    pub pairs: Vec<String>,
    pub order_size_usd: Decimal,
    pub min_spread_bps: Decimal,
    pub spread_capture_pct: Decimal,
    pub requote_threshold_pct: Decimal,
    pub maker_fee_pct: Decimal,
    /// Minimum profit margin on sells (as fraction of avg_cost).
    /// Sell price will be at least avg_cost * (1 + min_profit_pct).
    /// This guarantees every completed round-trip is profitable.
    pub min_profit_pct: Decimal,
    pub dry_run: bool,
    pub downtrend_threshold_pct: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskConfig {
    pub max_inventory_usd: Decimal,
    pub max_total_exposure_usd: Decimal,
    pub kill_switch_loss_usd: Decimal,
    pub stale_order_secs: u64,
    pub dms_timeout_secs: u64,
    pub dms_refresh_secs: u64,
    pub rate_limit_max_counter: u32,
    pub stop_loss_pct: Decimal,
    /// Take-profit threshold: liquidate if price rises this far above avg_cost.
    pub take_profit_pct: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistenceConfig {
    pub state_file: String,
    pub trade_log_file: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            exchange: ExchangeConfig {
                api_key: String::new(),
                api_secret: String::new(),
                ws_public_url: "wss://ws.kraken.com/v2".into(),
                ws_private_url: "wss://ws-auth.kraken.com/v2".into(),
                rest_base_url: "https://api.kraken.com".into(),
                book_depth: 10,
            },
            trading: TradingConfig {
                pairs: vec![],
                order_size_usd: dec!(100),
                min_spread_bps: dec!(100),
                spread_capture_pct: dec!(0.50),
                requote_threshold_pct: dec!(0.005),
                maker_fee_pct: dec!(0.0023),
                min_profit_pct: dec!(0.01),
                dry_run: true,
                downtrend_threshold_pct: dec!(-5.0),
            },
            risk: RiskConfig {
                max_inventory_usd: dec!(200),
                max_total_exposure_usd: dec!(2000),
                kill_switch_loss_usd: dec!(-100),
                stale_order_secs: 300,
                dms_timeout_secs: 60,
                dms_refresh_secs: 20,
                rate_limit_max_counter: 60,
                stop_loss_pct: dec!(0.03),
                take_profit_pct: dec!(0.10), // 10% — liquidate on insane profit
            },
            persistence: PersistenceConfig {
                state_file: "state.json".into(),
                trade_log_file: "logs/trades.csv".into(),
            },
        }
    }
}
