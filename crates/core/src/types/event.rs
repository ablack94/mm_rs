use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use super::fill::Fill;

/// A price-level update from the order book.
#[derive(Debug, Clone)]
pub struct LevelUpdate {
    pub price: Decimal,
    pub qty: Decimal,
}

/// Events flowing into the engine. All timestamps come from the event source,
/// making the engine fully deterministic and testable.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    BookSnapshot {
        symbol: String,
        bids: Vec<LevelUpdate>,
        asks: Vec<LevelUpdate>,
        timestamp: DateTime<Utc>,
    },
    BookUpdate {
        symbol: String,
        bid_updates: Vec<LevelUpdate>,
        ask_updates: Vec<LevelUpdate>,
        timestamp: DateTime<Utc>,
    },
    Fill(Fill),
    OrderAcknowledged {
        cl_ord_id: String,
        order_id: String,
    },
    OrderCancelled {
        cl_ord_id: String,
        symbol: String,
    },
    OrderRejected {
        cl_ord_id: String,
        symbol: String,
        reason: String,
    },
    /// Periodic heartbeat for stale order checks and DMS refresh.
    Tick {
        timestamp: DateTime<Utc>,
    },
    /// Command from the REST API.
    ApiCommand(ApiAction),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ApiAction {
    CancelAll,
    CancelOrder { cl_ord_id: String },
    Pause,
    Resume,
    Shutdown,
    Liquidate { symbol: String },
    /// Disable a pair: cancel its orders, stop quoting.
    DisablePair { symbol: String },
    /// Re-enable a disabled pair (clears cooldown too).
    EnablePair { symbol: String },
    /// Add a new pair at runtime.
    AddPair { symbol: String },
    /// Remove a pair: liquidate position + permanently disable.
    RemovePair { symbol: String },
}
