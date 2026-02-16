use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::types::{GlobalDefaults, PairConfig, PairState};

/// A pair record as received from the state store.
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

/// Messages sent from the state store to the bot via WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StateStoreMessage {
    /// Initial snapshot of all pairs and defaults.
    Snapshot {
        pairs: Vec<PairRecord>,
        defaults: GlobalDefaults,
    },
    /// A pair was created or updated.
    PairUpdated {
        pair: PairRecord,
    },
    /// A pair was removed.
    PairRemoved {
        symbol: String,
    },
    /// Global defaults were updated.
    DefaultsUpdated {
        defaults: GlobalDefaults,
    },
}

/// Messages sent from the bot to the state store.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BotMessage {
    /// Periodic heartbeat with summary stats.
    Heartbeat {
        timestamp: DateTime<Utc>,
        active_pairs: u32,
        total_exposure_usd: Decimal,
    },
    /// Per-pair position/status report.
    PairReport {
        symbol: String,
        position_qty: Decimal,
        position_avg_cost: Decimal,
        exposure_usd: Decimal,
        quoter_state: String,
        has_open_orders: bool,
    },
}
