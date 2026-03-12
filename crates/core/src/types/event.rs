use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use trading_primitives::Ticker;
use super::fill::Fill;
use super::pair::PairInfo;

// Re-export LevelUpdate from trading-primitives.
pub use trading_primitives::book::LevelUpdate;

/// Events flowing into the engine. All timestamps come from the event source,
/// making the engine fully deterministic and testable.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    BookSnapshot {
        pair: Ticker,
        bids: Vec<LevelUpdate>,
        asks: Vec<LevelUpdate>,
        timestamp: DateTime<Utc>,
    },
    BookUpdate {
        pair: Ticker,
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
        pair: Ticker,
        /// Reason for cancellation from exchange (e.g., "Market price protection").
        reason: Option<String>,
    },
    OrderRejected {
        cl_ord_id: String,
        pair: Ticker,
        reason: String,
    },
    /// Periodic heartbeat for stale order checks and DMS refresh.
    Tick {
        timestamp: DateTime<Utc>,
    },
    /// Command from the REST API.
    ApiCommand(ApiAction),
    /// Command from the state store WS client.
    StateStoreCommand(StateStoreAction),
    /// Pair info fetched for newly discovered pairs (from state store).
    PairInfoFetched {
        info: HashMap<Ticker, PairInfo>,
    },
    /// Periodic balance snapshot from exchange. Used to reconcile the engine's
    /// position tracker with actual exchange balances, catching any fills that
    /// were dropped by WS or reconciliation.
    BalanceUpdate {
        /// Asset name → balance (e.g., "PEPE" → 29455081)
        balances: HashMap<String, rust_decimal::Decimal>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ApiAction {
    CancelAll,
    CancelOrder { cl_ord_id: String },
    Pause,
    Resume,
    Shutdown,
    Liquidate { pair: Ticker },
    /// Disable a pair: cancel its orders, stop quoting.
    DisablePair { pair: Ticker },
    /// Re-enable a disabled pair (clears cooldown too).
    EnablePair { pair: Ticker },
    /// Add a new pair at runtime.
    AddPair { pair: Ticker },
    /// Remove a pair: liquidate position + permanently disable.
    RemovePair { pair: Ticker },
}

/// Actions received from the state store WS client.
#[derive(Debug, Clone)]
pub enum StateStoreAction {
    /// Full snapshot of all pairs and global defaults.
    Snapshot {
        pairs: Vec<crate::state_store::PairRecord>,
        defaults: super::managed_pair::GlobalDefaults,
    },
    /// A single pair was created or updated.
    PairUpdated(crate::state_store::PairRecord),
    /// A pair was removed.
    PairRemoved { pair: Ticker },
    /// Global defaults were updated.
    DefaultsUpdated(super::managed_pair::GlobalDefaults),
}
