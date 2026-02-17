use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PairState {
    Disabled,
    WindDown,
    Liquidating,
    Active,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PairConfig {
    pub order_size_usd: Option<f64>,
    pub max_inventory_usd: Option<f64>,
    pub min_spread_bps: Option<f64>,
    pub spread_capture_pct: Option<f64>,
    pub min_profit_pct: Option<f64>,
    pub stop_loss_pct: Option<f64>,
    pub take_profit_pct: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalDefaults {
    pub order_size_usd: f64,
    pub max_inventory_usd: f64,
    pub min_spread_bps: f64,
    pub spread_capture_pct: f64,
    pub min_profit_pct: f64,
    pub stop_loss_pct: f64,
    pub take_profit_pct: f64,
}

impl Default for GlobalDefaults {
    fn default() -> Self {
        Self {
            order_size_usd: 50.0,
            max_inventory_usd: 200.0,
            min_spread_bps: 100.0,
            spread_capture_pct: 0.5,
            min_profit_pct: 0.01,
            stop_loss_pct: 0.03,
            take_profit_pct: 0.10,
        }
    }
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
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    pub order_size_usd: Option<Option<f64>>,
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    pub max_inventory_usd: Option<Option<f64>>,
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    pub min_spread_bps: Option<Option<f64>>,
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    pub spread_capture_pct: Option<Option<f64>>,
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    pub min_profit_pct: Option<Option<f64>>,
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    pub stop_loss_pct: Option<Option<f64>>,
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    pub take_profit_pct: Option<Option<f64>>,
}

/// Deserialize a field that can be:
/// - absent from JSON -> outer None (don't touch)
/// - present as null  -> Some(None) (set to null / revert to default)
/// - present as value -> Some(Some(v)) (set to value)
fn deserialize_optional_field<'de, D>(deserializer: D) -> Result<Option<Option<f64>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let val: Option<f64> = Option::deserialize(deserializer)?;
    Ok(Some(val))
}

#[derive(Debug, Deserialize)]
pub struct PatchDefaultsRequest {
    pub order_size_usd: Option<f64>,
    pub max_inventory_usd: Option<f64>,
    pub min_spread_bps: Option<f64>,
    pub spread_capture_pct: Option<f64>,
    pub min_profit_pct: Option<f64>,
    pub stop_loss_pct: Option<f64>,
    pub take_profit_pct: Option<f64>,
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
