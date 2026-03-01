use std::sync::Arc;
use tokio::sync::Mutex;

use proxy_common::order_tracking::{OrderRegistry, FillLedger, PositionTracker};

/// Shared proxy state for order/fill/position tracking.
/// Wrapped in Arc and shared between REST and WS handlers.
pub struct ProxyOrderState {
    pub orders: Mutex<OrderRegistry>,
    pub fills: Mutex<FillLedger>,
    pub positions: Mutex<PositionTracker>,
}

impl ProxyOrderState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            orders: Mutex::new(OrderRegistry::new()),
            fills: Mutex::new(FillLedger::new()),
            positions: Mutex::new(PositionTracker::new()),
        })
    }
}
