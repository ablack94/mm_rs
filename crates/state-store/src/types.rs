use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// Import shared types from trading-primitives.
pub use trading_primitives::{PairState, PairConfig, GlobalDefaults};

// ---------------------------------------------------------------------------
// Core data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairRecord {
    pub symbol: String,
    pub state: PairState,
    pub config: PairConfig,
    pub disabled_reason: Option<String>,
    pub auto_enable_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Store data (what gets persisted)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreData {
    pub defaults: GlobalDefaults,
    pub pairs: HashMap<String, PairRecord>,
    #[serde(default = "default_version")]
    pub version: u32,
}

fn default_version() -> u32 {
    1
}

impl Default for StoreData {
    fn default() -> Self {
        Self {
            defaults: GlobalDefaults::default(),
            pairs: HashMap::new(),
            version: 1,
        }
    }
}

// ---------------------------------------------------------------------------
// WebSocket messages: State Store -> Bot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutboundMessage {
    Snapshot {
        pairs: Vec<PairRecord>,
        defaults: GlobalDefaults,
    },
    PairUpdated {
        pair: PairRecord,
    },
    PairRemoved {
        symbol: String,
    },
    DefaultsUpdated {
        defaults: GlobalDefaults,
    },
}

// ---------------------------------------------------------------------------
// WebSocket messages: Bot -> State Store
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InboundMessage {
    Heartbeat {
        #[allow(dead_code)]
        timestamp: String,
        #[allow(dead_code)]
        active_pairs: u32,
        #[allow(dead_code)]
        total_exposure_usd: String,
    },
    PairReport {
        symbol: String,
        position_qty: String,
        #[allow(dead_code)]
        position_avg_cost: String,
        #[allow(dead_code)]
        exposure_usd: String,
        #[allow(dead_code)]
        quoter_state: String,
        #[allow(dead_code)]
        has_open_orders: bool,
    },
}

// ---------------------------------------------------------------------------
// REST request/response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PutPairRequest {
    pub state: Option<PairState>,
    pub config: Option<PairConfig>,
}

#[derive(Debug, Deserialize)]
pub struct PatchPairRequest {
    pub state: Option<PairState>,
    pub config: Option<PatchPairConfig>,
    pub disabled_reason: Option<String>,
    pub auto_enable_at: Option<DateTime<Utc>>,
}

/// For PATCH: we need to distinguish "field absent" (don't touch) from
/// "field set to null" (revert to global default).  We use a wrapper
/// that deserializes both cases.
#[derive(Debug, Deserialize)]
pub struct PatchPairConfig {
    #[serde(default, deserialize_with = "deserialize_optional_decimal")]
    pub order_size_usd: Option<Option<Decimal>>,
    #[serde(default, deserialize_with = "deserialize_optional_decimal")]
    pub max_inventory_usd: Option<Option<Decimal>>,
    #[serde(default, deserialize_with = "deserialize_optional_decimal")]
    pub min_spread_bps: Option<Option<Decimal>>,
    #[serde(default, deserialize_with = "deserialize_optional_decimal")]
    pub spread_capture_pct: Option<Option<Decimal>>,
    #[serde(default, deserialize_with = "deserialize_optional_decimal")]
    pub min_profit_pct: Option<Option<Decimal>>,
    #[serde(default, deserialize_with = "deserialize_optional_decimal")]
    pub stop_loss_pct: Option<Option<Decimal>>,
    #[serde(default, deserialize_with = "deserialize_optional_decimal")]
    pub take_profit_pct: Option<Option<Decimal>>,
    #[serde(default, deserialize_with = "deserialize_optional_u32")]
    pub max_buys_before_sell: Option<Option<u32>>,
    #[serde(default, deserialize_with = "deserialize_optional_bool")]
    pub use_winddown_for_stoploss: Option<Option<bool>>,
}

/// Deserialize a field that can be:
/// - absent from JSON -> outer None (don't touch)
/// - present as null  -> Some(None) (set to null / revert to default)
/// - present as value -> Some(Some(v)) (set to value)
fn deserialize_optional_decimal<'de, D>(deserializer: D) -> Result<Option<Option<Decimal>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let val: Option<Decimal> = Option::deserialize(deserializer)?;
    Ok(Some(val))
}

fn deserialize_optional_u32<'de, D>(deserializer: D) -> Result<Option<Option<u32>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let val: Option<u32> = Option::deserialize(deserializer)?;
    Ok(Some(val))
}

fn deserialize_optional_bool<'de, D>(deserializer: D) -> Result<Option<Option<bool>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let val: Option<bool> = Option::deserialize(deserializer)?;
    Ok(Some(val))
}

#[derive(Debug, Deserialize)]
pub struct PatchDefaultsRequest {
    pub order_size_usd: Option<Decimal>,
    pub max_inventory_usd: Option<Decimal>,
    pub min_spread_bps: Option<Decimal>,
    pub spread_capture_pct: Option<Decimal>,
    pub min_profit_pct: Option<Decimal>,
    pub stop_loss_pct: Option<Decimal>,
    pub take_profit_pct: Option<Decimal>,
    pub max_buys_before_sell: Option<u32>,
    pub use_winddown_for_stoploss: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct PairsListResponse {
    pub pairs: Vec<PairRecord>,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub bot_connected: bool,
    pub pairs_count: usize,
    pub uptime_secs: u64,
}

/// Query params for GET /pairs
#[derive(Debug, Deserialize)]
pub struct PairsQuery {
    pub state: Option<String>,
}

/// Query params for WS auth
#[derive(Debug, Deserialize)]
pub struct WsQuery {
    pub token: Option<String>,
}
