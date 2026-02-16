use chrono::{DateTime, Utc};
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use crate::store::Store;
use crate::types::{OutboundMessage, StoreData};

// ---------------------------------------------------------------------------
// Shared application state
// ---------------------------------------------------------------------------

pub struct AppState {
    pub store_data: StoreData,
    pub store: Box<dyn Store>,
    pub ws_broadcast: broadcast::Sender<OutboundMessage>,
    pub bot_last_seen: Option<DateTime<Utc>>,
    pub bot_count: usize,
    pub started_at: std::time::Instant,
}

pub type SharedState = Arc<RwLock<AppState>>;

impl AppState {
    pub fn new(store_data: StoreData, store: Box<dyn Store>) -> (Self, broadcast::Receiver<OutboundMessage>) {
        // Buffer of 64 should be plenty for outbound WS messages
        let (tx, rx) = broadcast::channel(64);
        let state = Self {
            store_data,
            store,
            ws_broadcast: tx,
            bot_last_seen: None,
            bot_count: 0,
            started_at: std::time::Instant::now(),
        };
        (state, rx)
    }

    /// Persist current state to storage backend.
    pub async fn persist(&self) -> anyhow::Result<()> {
        self.store.save(&self.store_data).await
    }

    /// Broadcast a message to all connected WS clients.
    /// Returns Ok even if there are no receivers (no bots connected).
    pub fn broadcast(&self, msg: OutboundMessage) {
        // send returns Err if there are no receivers, which is fine
        let _ = self.ws_broadcast.send(msg);
    }
}
