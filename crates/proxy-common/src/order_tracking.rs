use serde::Serialize;
use std::collections::HashMap;

/// Status of an order through its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Pending,
    Open,
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected,
}

/// A tracked order with full lifecycle data.
#[derive(Debug, Clone, Serialize)]
pub struct TrackedOrder {
    pub cl_ord_id: String,
    pub exchange_id: String,
    pub pair: String,
    pub side: String,
    pub price: f64,
    pub original_qty: f64,
    pub filled_qty: f64,
    pub status: OrderStatus,
}

/// Source of a fill record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FillSource {
    WebSocket,
    Reconciliation,
}

/// An immutable fill entry.
#[derive(Debug, Clone, Serialize)]
pub struct FillRecord {
    pub fill_id: String,
    pub cl_ord_id: String,
    pub pair: String,
    pub side: String,
    pub price: f64,
    pub qty: f64,
    pub fee: f64,
    pub timestamp: String,
    pub source: FillSource,
}

/// A position derived from fills.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ProxyPosition {
    pub qty: f64,
    pub avg_cost: f64,
    pub realized_pnl: f64,
}

/// Registry of orders indexed by cl_ord_id with reverse lookup by exchange_id.
#[derive(Debug, Default)]
pub struct OrderRegistry {
    orders: HashMap<String, TrackedOrder>,
    /// exchange_id → cl_ord_id reverse lookup
    exchange_to_cl: HashMap<String, String>,
}

impl OrderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new order.
    pub fn insert(&mut self, order: TrackedOrder) {
        if !order.exchange_id.is_empty() {
            self.exchange_to_cl
                .insert(order.exchange_id.clone(), order.cl_ord_id.clone());
        }
        self.orders.insert(order.cl_ord_id.clone(), order);
    }

    /// Look up an order by cl_ord_id.
    pub fn get(&self, cl_ord_id: &str) -> Option<&TrackedOrder> {
        self.orders.get(cl_ord_id)
    }

    /// Look up an order by cl_ord_id (mutable).
    pub fn get_mut(&mut self, cl_ord_id: &str) -> Option<&mut TrackedOrder> {
        self.orders.get_mut(cl_ord_id)
    }

    /// Look up cl_ord_id by exchange_id.
    pub fn cl_ord_id_for_exchange_id(&self, exchange_id: &str) -> Option<&str> {
        self.exchange_to_cl.get(exchange_id).map(|s| s.as_str())
    }

    /// Update order status. Cleans up reverse lookup on terminal states.
    pub fn update_status(&mut self, cl_ord_id: &str, status: OrderStatus) {
        if let Some(order) = self.orders.get_mut(cl_ord_id) {
            order.status = status;
            if matches!(status, OrderStatus::Filled | OrderStatus::Cancelled | OrderStatus::Rejected) {
                self.exchange_to_cl.remove(&order.exchange_id);
            }
        }
    }

    /// Update price and/or qty for an amended order.
    pub fn update_price_qty(&mut self, cl_ord_id: &str, price: Option<f64>, qty: Option<f64>) {
        if let Some(order) = self.orders.get_mut(cl_ord_id) {
            if let Some(p) = price {
                order.price = p;
            }
            if let Some(q) = qty {
                order.original_qty = q;
            }
        }
    }

    /// Add to filled_qty for partial fills.
    pub fn add_fill(&mut self, cl_ord_id: &str, fill_qty: f64) {
        if let Some(order) = self.orders.get_mut(cl_ord_id) {
            order.filled_qty += fill_qty;
            if order.filled_qty >= order.original_qty - 1e-12 {
                order.status = OrderStatus::Filled;
                self.exchange_to_cl.remove(&order.exchange_id);
            } else {
                order.status = OrderStatus::PartiallyFilled;
            }
        }
    }

    /// Mark all orders as cancelled and clear the registry.
    pub fn cancel_all(&mut self) {
        for order in self.orders.values_mut() {
            order.status = OrderStatus::Cancelled;
        }
        self.exchange_to_cl.clear();
    }

    /// Get all orders (for REST endpoint).
    pub fn all_orders(&self) -> Vec<&TrackedOrder> {
        self.orders.values().collect()
    }
}

/// Append-only fill log with dedup by fill_id.
#[derive(Debug, Default)]
pub struct FillLedger {
    fills: Vec<FillRecord>,
    seen_ids: std::collections::HashSet<String>,
}

impl FillLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a fill. Returns false if the fill_id was already seen (duplicate).
    pub fn record(&mut self, fill: FillRecord) -> bool {
        if !self.seen_ids.insert(fill.fill_id.clone()) {
            return false;
        }
        self.fills.push(fill);
        true
    }

    /// Get recent fills, newest first, up to `limit`.
    pub fn recent(&self, limit: usize) -> Vec<&FillRecord> {
        self.fills.iter().rev().take(limit).collect()
    }
}

/// Tracks positions derived from fills.
#[derive(Debug, Default)]
pub struct PositionTracker {
    positions: HashMap<String, ProxyPosition>,
}

impl PositionTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a fill to update the position for a pair.
    pub fn apply_fill(&mut self, pair: &str, side: &str, qty: f64, price: f64) {
        let pos = self.positions.entry(pair.to_string()).or_default();
        let is_buy = side == "buy";

        if is_buy {
            // Average cost calculation: weighted average
            let new_total_cost = pos.avg_cost * pos.qty + price * qty;
            pos.qty += qty;
            if pos.qty > 1e-12 {
                pos.avg_cost = new_total_cost / pos.qty;
            }
        } else {
            // Sell: realize PnL
            if pos.qty > 1e-12 {
                let sell_qty = qty.min(pos.qty);
                pos.realized_pnl += sell_qty * (price - pos.avg_cost);
                pos.qty -= sell_qty;
                if pos.qty < 1e-12 {
                    pos.qty = 0.0;
                    pos.avg_cost = 0.0;
                }
            }
        }
    }

    /// Seed a position from exchange balances (no avg_cost available).
    pub fn seed_from_balance(&mut self, pair: &str, qty: f64) {
        if qty > 1e-12 {
            let pos = self.positions.entry(pair.to_string()).or_default();
            if pos.qty < 1e-12 {
                pos.qty = qty;
                // avg_cost stays 0 — will be updated from fills
            }
        }
    }

    /// Get all positions.
    pub fn all_positions(&self) -> &HashMap<String, ProxyPosition> {
        &self.positions
    }

    /// Get position for a specific pair.
    pub fn get(&self, pair: &str) -> Option<&ProxyPosition> {
        self.positions.get(pair)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_order_registry_lifecycle() {
        let mut reg = OrderRegistry::new();

        let order = TrackedOrder {
            cl_ord_id: "bid-1".to_string(),
            exchange_id: "ex-123".to_string(),
            pair: "BTC/USD".to_string(),
            side: "buy".to_string(),
            price: 50000.0,
            original_qty: 0.01,
            filled_qty: 0.0,
            status: OrderStatus::Pending,
        };
        reg.insert(order);

        assert!(reg.get("bid-1").is_some());
        assert_eq!(reg.cl_ord_id_for_exchange_id("ex-123"), Some("bid-1"));

        reg.update_status("bid-1", OrderStatus::Open);
        assert_eq!(reg.get("bid-1").unwrap().status, OrderStatus::Open);

        reg.add_fill("bid-1", 0.005);
        assert_eq!(reg.get("bid-1").unwrap().status, OrderStatus::PartiallyFilled);

        reg.add_fill("bid-1", 0.005);
        assert_eq!(reg.get("bid-1").unwrap().status, OrderStatus::Filled);
        // Exchange reverse lookup cleaned up after filled
        assert_eq!(reg.cl_ord_id_for_exchange_id("ex-123"), None);
    }

    #[test]
    fn test_fill_ledger_dedup() {
        let mut ledger = FillLedger::new();

        let fill = FillRecord {
            fill_id: "fill-1".to_string(),
            cl_ord_id: "bid-1".to_string(),
            pair: "BTC/USD".to_string(),
            side: "buy".to_string(),
            price: 50000.0,
            qty: 0.01,
            fee: 0.5,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            source: FillSource::WebSocket,
        };
        assert!(ledger.record(fill.clone()));
        assert!(!ledger.record(fill)); // duplicate
        assert_eq!(ledger.recent(10).len(), 1);
    }

    #[test]
    fn test_position_tracker_buy_sell() {
        let mut tracker = PositionTracker::new();

        // Buy 1.0 at 100
        tracker.apply_fill("BTC/USD", "buy", 1.0, 100.0);
        let pos = tracker.get("BTC/USD").unwrap();
        assert!((pos.qty - 1.0).abs() < 1e-10);
        assert!((pos.avg_cost - 100.0).abs() < 1e-10);

        // Buy 1.0 at 200 → avg_cost should be 150
        tracker.apply_fill("BTC/USD", "buy", 1.0, 200.0);
        let pos = tracker.get("BTC/USD").unwrap();
        assert!((pos.qty - 2.0).abs() < 1e-10);
        assert!((pos.avg_cost - 150.0).abs() < 1e-10);

        // Sell 1.0 at 180 → realized_pnl = 1.0 * (180 - 150) = 30
        tracker.apply_fill("BTC/USD", "sell", 1.0, 180.0);
        let pos = tracker.get("BTC/USD").unwrap();
        assert!((pos.qty - 1.0).abs() < 1e-10);
        assert!((pos.realized_pnl - 30.0).abs() < 1e-10);
        assert!((pos.avg_cost - 150.0).abs() < 1e-10);
    }
}
