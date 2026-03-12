/// Translation between proxy protocol (bot) and Coinbase formats.
///
/// The bot speaks ProxyEvent/ProxyCommand protocol. This module translates:
/// - Coinbase level2 book data → ProxyEvent::BookSnapshot/BookUpdate
/// - Coinbase user channel events → ProxyEvent::Fill/OrderAccepted/OrderCancelled
/// - Subscribe messages → Coinbase subscribe format

use crate::pairs;
use exchange_api::{BookLevel, ProxyEvent};
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::HashMap;
use std::str::FromStr;

/// Tracks cumulative fill data per order to compute incremental fill deltas.
///
/// Coinbase reports cumulative quantities (total filled so far), but the bot
/// expects incremental quantities (just this fill). The tracker converts between
/// the two by remembering previous cumulative values per cl_ord_id.
pub struct FillTracker {
    /// cl_ord_id → (cumulative_qty, cumulative_cost, cumulative_fees)
    state: HashMap<String, (f64, f64, f64)>,
}

impl FillTracker {
    pub fn new() -> Self {
        Self {
            state: HashMap::new(),
        }
    }

    /// Seed the tracker with current cumulative values without emitting a fill.
    /// Used for snapshot events where we don't want to generate fills for
    /// already-existing partial fills.
    pub fn seed(&mut self, cl_ord_id: &str, cum_qty: f64, avg_price: f64, total_fees: f64) {
        if cum_qty > 0.0 {
            let cum_cost = avg_price * cum_qty;
            self.state
                .insert(cl_ord_id.to_string(), (cum_qty, cum_cost, total_fees));
        }
    }

    /// Given cumulative fill data, compute the incremental fill since last update.
    /// Returns Some((inc_qty, inc_price, inc_fees)) if there's a new fill.
    /// Returns None if no new fill (duplicate update or zero delta).
    pub fn incremental(
        &mut self,
        cl_ord_id: &str,
        cum_qty: f64,
        avg_price: f64,
        total_fees: f64,
    ) -> Option<(f64, f64, f64)> {
        let (prev_qty, prev_cost, prev_fees) =
            self.state.get(cl_ord_id).copied().unwrap_or((0.0, 0.0, 0.0));
        let inc_qty = cum_qty - prev_qty;
        if inc_qty < 1e-12 {
            return None; // no new fill
        }

        let cum_cost = avg_price * cum_qty;
        let inc_cost = cum_cost - prev_cost;
        let inc_price = if inc_qty > 0.0 {
            inc_cost / inc_qty
        } else {
            avg_price
        };
        let inc_fees = (total_fees - prev_fees).max(0.0);

        self.state
            .insert(cl_ord_id.to_string(), (cum_qty, cum_cost, total_fees));
        Some((inc_qty, inc_price, inc_fees))
    }

    /// Remove tracking for an order (on terminal states).
    pub fn remove(&mut self, cl_ord_id: &str) {
        self.state.remove(cl_ord_id);
    }

    /// Number of tracked orders.
    pub fn len(&self) -> usize {
        self.state.len()
    }
}

/// Convert f64 to Decimal for ProxyEvent fields.
fn to_decimal(f: f64) -> Decimal {
    Decimal::from_f64_retain(f).unwrap_or_default()
}

/// Translate a Coinbase level2 (l2_data) message to ProxyEvent book format.
#[cfg(test)]
pub fn coinbase_book_to_proxy_event(coinbase_msg: &Value) -> Option<String> {
    coinbase_book_to_proxy_events_mapped(coinbase_msg, &HashMap::new()).into_iter().next()
}

/// Translate a Coinbase level2 (l2_data) message to ProxyEvent book format,
/// using a product_id → internal symbol mapping.
///
/// Coinbase merges USD/USDC order books and always returns the base-USD product_id
/// in level2 data. The mapping allows relabeling (e.g., "DOGE-USD" → "DOGE/USDC").
///
/// Returns a Vec because Coinbase may batch multiple product events in a single
/// WebSocket message. Each event in the `events` array is translated independently.
/// Individual update entries that fail to parse are skipped rather than aborting the
/// entire message.
pub fn coinbase_book_to_proxy_events_mapped(
    coinbase_msg: &Value,
    product_id_map: &HashMap<String, String>,
) -> Vec<String> {
    let events = match coinbase_msg.get("events").and_then(|e| e.as_array()) {
        Some(e) => e,
        None => return Vec::new(),
    };

    let mut results = Vec::new();

    for event in events {
        let event_type = match event.get("type").and_then(|t| t.as_str()) {
            Some(t) => t,
            None => continue,
        };
        let product_id = match event.get("product_id").and_then(|p| p.as_str()) {
            Some(p) => p,
            None => continue,
        };
        // Use mapped symbol if available, otherwise fall back to simple conversion
        let symbol = product_id_map
            .get(product_id)
            .cloned()
            .unwrap_or_else(|| pairs::to_internal(product_id));
        let updates = match event.get("updates").and_then(|u| u.as_array()) {
            Some(u) => u,
            None => continue,
        };

        let mut bids = Vec::new();
        let mut asks = Vec::new();
        let mut skipped = 0u32;

        for update in updates {
            let side = match update.get("side").and_then(|s| s.as_str()) {
                Some(s) => s,
                None => { skipped += 1; continue; }
            };
            let price_str = match update.get("price_level").and_then(|p| p.as_str()) {
                Some(s) => s,
                None => { skipped += 1; continue; }
            };
            let qty_str = match update.get("new_quantity").and_then(|q| q.as_str()) {
                Some(s) => s,
                None => { skipped += 1; continue; }
            };

            // Try standard decimal parse first, fall back to scientific notation
            let price = match Decimal::from_str(price_str)
                .or_else(|_| Decimal::from_scientific(price_str))
            {
                Ok(d) => d,
                Err(_) => { skipped += 1; continue; }
            };
            let qty = match Decimal::from_str(qty_str)
                .or_else(|_| Decimal::from_scientific(qty_str))
            {
                Ok(d) => d,
                Err(_) => { skipped += 1; continue; }
            };

            let level = BookLevel { price, qty };

            match side {
                "bid" => bids.push(level),
                "ask" | "offer" => asks.push(level),
                _ => {}
            }
        }

        if skipped > 0 {
            tracing::warn!(
                product_id, skipped, total = updates.len(),
                "Skipped unparseable entries in level2 update"
            );
        }

        let proxy_event = match event_type {
            "snapshot" => ProxyEvent::book_snapshot(&symbol, bids, asks),
            "update" => ProxyEvent::book_update(&symbol, bids, asks),
            _ => continue,
        };

        results.push(proxy_event.to_json());
    }

    results
}

/// Translate a Coinbase user channel message to ProxyEvent format.
///
/// The `fill_tracker` converts Coinbase's cumulative fill data to incremental
/// quantities expected by the bot. For snapshot events, it seeds the tracker
/// without emitting fills; for updates, it computes deltas and emits trade events.
pub fn coinbase_user_to_proxy_events(
    coinbase_msg: &Value,
    fill_tracker: &mut FillTracker,
) -> Vec<String> {
    let mut results = Vec::new();

    let events = match coinbase_msg.get("events").and_then(|e| e.as_array()) {
        Some(e) => e,
        None => return results,
    };

    for event in events {
        let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let orders = match event.get("orders").and_then(|o| o.as_array()) {
            Some(o) => o,
            None => continue,
        };

        if event_type == "snapshot" {
            // Seed the fill tracker with current cumulative state but don't emit fills.
            // Snapshot orders reflect pre-existing state, not new fills.
            for order in orders {
                seed_tracker_from_order(order, fill_tracker);
                if let Some(proxy_event) = translate_order_to_event(order) {
                    results.push(proxy_event.to_json());
                }
            }
        } else {
            for order in orders {
                if let Some(proxy_event) =
                    translate_order_to_event_tracked(order, fill_tracker)
                {
                    results.push(proxy_event.to_json());
                }
            }
        }
    }

    results
}

/// Translate a single Coinbase order object to a ProxyEvent (snapshot mode, no fill tracking).
fn translate_order_to_event(order: &Value) -> Option<ProxyEvent> {
    let status = order.get("status")?.as_str()?;
    let product_id = order.get("product_id")?.as_str()?;
    let symbol = pairs::to_internal(product_id);
    let order_id = order.get("order_id")?.as_str().unwrap_or("");
    let client_order_id = order.get("client_order_id")?.as_str().unwrap_or("");
    let side = order.get("order_side")?.as_str().unwrap_or("").to_lowercase();

    let cancel_reason = order
        .get("cancel_reason")
        .and_then(|r| r.as_str())
        .filter(|r| !r.is_empty());

    match status {
        "FILLED" => {
            let avg_price: f64 = order.get("avg_price").and_then(|p| p.as_str()).and_then(|p| p.parse().ok()).unwrap_or(0.0);
            let filled_qty: f64 = order.get("cumulative_quantity").and_then(|q| q.as_str()).and_then(|q| q.parse().ok()).unwrap_or(0.0);
            let total_fees: f64 = order.get("total_fees").and_then(|f| f.as_str()).and_then(|f| f.parse().ok()).unwrap_or(0.0);
            let timestamp = order.get("creation_time").and_then(|t| t.as_str()).unwrap_or("1970-01-01T00:00:00Z");
            Some(ProxyEvent::fill(
                order_id, client_order_id, &symbol, &side,
                to_decimal(filled_qty), to_decimal(avg_price), to_decimal(total_fees),
                true, timestamp, true,
            ))
        }
        "OPEN" => {
            Some(ProxyEvent::order_accepted(0, client_order_id, order_id))
        }
        "PENDING" => {
            // Pending orders — emit as accepted (will get another ack when OPEN)
            Some(ProxyEvent::order_accepted(0, client_order_id, order_id))
        }
        "CANCELLED" | "EXPIRED" | "FAILED" => {
            Some(ProxyEvent::order_cancelled(client_order_id, cancel_reason, Some(&symbol)))
        }
        _ => None,
    }
}

/// Extract cumulative fill data from a Coinbase order and seed the fill tracker.
/// Used during snapshots to establish baseline state without emitting fills.
fn seed_tracker_from_order(order: &Value, tracker: &mut FillTracker) {
    let cl_ord_id = order
        .get("client_order_id")
        .and_then(|c| c.as_str())
        .unwrap_or("");
    if cl_ord_id.is_empty() {
        return;
    }
    let cum_qty: f64 = order
        .get("cumulative_quantity")
        .and_then(|q| q.as_str())
        .and_then(|q| q.parse().ok())
        .unwrap_or(0.0);
    let avg_price: f64 = order
        .get("avg_price")
        .and_then(|p| p.as_str())
        .and_then(|p| p.parse().ok())
        .unwrap_or(0.0);
    let total_fees: f64 = order
        .get("total_fees")
        .and_then(|f| f.as_str())
        .and_then(|f| f.parse().ok())
        .unwrap_or(0.0);
    tracker.seed(cl_ord_id, cum_qty, avg_price, total_fees);
}

/// Translate a Coinbase order to a ProxyEvent, using the fill tracker
/// to convert cumulative quantities to incremental.
///
/// Key behaviors:
/// - OPEN + cumulative_qty > 0 with new fill delta → Fill event (partial fill)
/// - OPEN + no new fill → OrderAccepted (order ack or duplicate update)
/// - FILLED → Fill event with incremental delta from last partial
/// - Terminal states (CANCELLED/EXPIRED/FAILED) → OrderCancelled + clean up tracker
fn translate_order_to_event_tracked(
    order: &Value,
    tracker: &mut FillTracker,
) -> Option<ProxyEvent> {
    let status = order.get("status")?.as_str()?;
    let product_id = order.get("product_id")?.as_str()?;
    let symbol = pairs::to_internal(product_id);
    let order_id = order.get("order_id")?.as_str().unwrap_or("");
    let client_order_id = order.get("client_order_id")?.as_str().unwrap_or("");
    let side = order.get("order_side")?.as_str().unwrap_or("").to_lowercase();

    let cum_qty: f64 = order
        .get("cumulative_quantity")
        .and_then(|q| q.as_str())
        .and_then(|q| q.parse().ok())
        .unwrap_or(0.0);
    let avg_price: f64 = order
        .get("avg_price")
        .and_then(|p| p.as_str())
        .and_then(|p| p.parse().ok())
        .unwrap_or(0.0);
    let total_fees: f64 = order
        .get("total_fees")
        .and_then(|f| f.as_str())
        .and_then(|f| f.parse().ok())
        .unwrap_or(0.0);

    let timestamp = order
        .get("creation_time")
        .and_then(|t| t.as_str())
        .unwrap_or("1970-01-01T00:00:00Z");
    let cancel_reason = order
        .get("cancel_reason")
        .and_then(|r| r.as_str())
        .filter(|r| !r.is_empty());

    match status {
        "FILLED" | "OPEN" => {
            // Check if there's a new incremental fill
            if let Some((inc_qty, inc_price, inc_fees)) =
                tracker.incremental(client_order_id, cum_qty, avg_price, total_fees)
            {
                // There's a new fill (partial or complete)
                let is_fully_filled = status == "FILLED";
                if is_fully_filled {
                    tracker.remove(client_order_id);
                }
                Some(ProxyEvent::fill(
                    order_id, client_order_id, &symbol, &side,
                    to_decimal(inc_qty), to_decimal(inc_price), to_decimal(inc_fees),
                    true, timestamp, is_fully_filled,
                ))
            } else if status == "OPEN" {
                // No fill delta — this is an order ack or duplicate update
                Some(ProxyEvent::order_accepted(0, client_order_id, order_id))
            } else {
                // FILLED with no delta (e.g., duplicate) — still report it
                tracker.remove(client_order_id);
                Some(ProxyEvent::fill(
                    order_id, client_order_id, &symbol, &side,
                    to_decimal(cum_qty), to_decimal(avg_price), to_decimal(total_fees),
                    true, timestamp, true,
                ))
            }
        }
        "PENDING" => Some(ProxyEvent::order_accepted(0, client_order_id, order_id)),
        "CANCELLED" | "EXPIRED" | "FAILED" => {
            tracker.remove(client_order_id);
            Some(ProxyEvent::order_cancelled(client_order_id, cancel_reason, Some(&symbol)))
        }
        _ => None,
    }
}

/// Translate a subscribe message to Coinbase format.
/// Handles both new ProxyCommand format and old Kraken WS v2 format.
/// Returns the Coinbase subscribe JSON (without auth fields — caller adds those).
pub fn subscribe_to_coinbase(channel: &str, symbols: &[String]) -> Option<Value> {
    let product_ids: Vec<String> = symbols
        .iter()
        .map(|s| pairs::to_coinbase(s))
        .collect();

    let coinbase_channel = match channel {
        "book" => "level2",
        "ticker" => "ticker",
        _ => return None,
    };

    Some(serde_json::json!({
        "type": "subscribe",
        "product_ids": product_ids,
        "channel": coinbase_channel
    }))
}

/// Build a ProxyEvent subscribe confirmation response.
pub fn subscribe_confirmed(channel: &str) -> String {
    ProxyEvent::subscribed(channel).to_json()
}

/// Build a ProxyEvent order response (success).
pub fn order_response_success(method: &str, req_id: u64, order_id: &str, cl_ord_id: &str) -> String {
    match method {
        "add_order" | "place_order" => ProxyEvent::order_accepted(req_id, cl_ord_id, order_id).to_json(),
        _ => ProxyEvent::command_ack(req_id, method).to_json(),
    }
}

/// Build a ProxyEvent order response (error).
pub fn order_response_error(_method: &str, req_id: u64, error: &str) -> String {
    ProxyEvent::order_rejected(req_id, "", error, None).to_json()
}

/// Build a ProxyEvent pong response.
pub fn pong_response(req_id: u64) -> String {
    ProxyEvent::pong(req_id).to_json()
}

/// Build a synthetic OrderAccepted for when an order is accepted via REST.
///
/// Coinbase's user WS may deliver an OPEN event later, but the bot needs an immediate
/// ack to track the order. Duplicate acks are harmless.
pub fn synthetic_exec_new(order_id: &str, cl_ord_id: &str, _symbol: &str, _side: &str) -> String {
    ProxyEvent::order_accepted(0, cl_ord_id, order_id).to_json()
}

/// Build a synthetic OrderCancelled for when a cancel succeeds via REST.
pub fn synthetic_exec_canceled(cl_ord_id: &str, symbol: &str, _side: &str) -> String {
    ProxyEvent::order_cancelled(cl_ord_id, Some("USER_CANCEL"), Some(symbol)).to_json()
}

/// Build a ProxyEvent command acknowledgement.
pub fn command_ack(req_id: u64, cmd: &str) -> String {
    ProxyEvent::command_ack(req_id, cmd).to_json()
}

/// Build a ProxyEvent heartbeat message.
pub fn heartbeat() -> String {
    ProxyEvent::heartbeat().to_json()
}

/// Build a ProxyEvent fill message (used by reconciliation).
pub fn build_fill_event(
    order_id: &str,
    cl_ord_id: &str,
    symbol: &str,
    side: &str,
    qty: f64,
    price: f64,
    fee: f64,
    timestamp: &str,
) -> String {
    ProxyEvent::fill(
        order_id, cl_ord_id, symbol, side,
        to_decimal(qty), to_decimal(price), to_decimal(fee),
        true, timestamp, true,
    ).to_json()
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange_api::parse_proxy_event;
    use serde_json::json;

    #[test]
    fn test_coinbase_book_snapshot_to_proxy_event() {
        let coinbase = json!({
            "channel": "l2_data",
            "timestamp": "2024-01-01T00:00:00Z",
            "events": [{
                "type": "snapshot",
                "product_id": "BTC-USD",
                "updates": [
                    {"side": "bid", "price_level": "50000.00", "new_quantity": "1.5", "event_time": "2024-01-01T00:00:00Z"},
                    {"side": "bid", "price_level": "49999.00", "new_quantity": "2.0", "event_time": "2024-01-01T00:00:00Z"},
                    {"side": "ask", "price_level": "50001.00", "new_quantity": "0.8", "event_time": "2024-01-01T00:00:00Z"}
                ]
            }]
        });

        let result = coinbase_book_to_proxy_event(&coinbase).unwrap();
        match parse_proxy_event(&result).unwrap() {
            ProxyEvent::BookSnapshot { symbol, bids, asks } => {
                assert_eq!(symbol, "BTC/USDC");
                assert_eq!(bids.len(), 2);
                assert_eq!(asks.len(), 1);
            }
            other => panic!("Expected BookSnapshot, got {:?}", other),
        }
    }

    #[test]
    fn test_coinbase_book_update_to_proxy_event() {
        let coinbase = json!({
            "channel": "l2_data",
            "events": [{
                "type": "update",
                "product_id": "ETH-USD",
                "updates": [
                    {"side": "bid", "price_level": "3000.00", "new_quantity": "10.0", "event_time": "2024-01-01T00:00:00Z"},
                    {"side": "ask", "price_level": "3001.00", "new_quantity": "0", "event_time": "2024-01-01T00:00:00Z"}
                ]
            }]
        });

        let result = coinbase_book_to_proxy_event(&coinbase).unwrap();
        match parse_proxy_event(&result).unwrap() {
            ProxyEvent::BookUpdate { symbol, .. } => {
                assert_eq!(symbol, "ETH/USDC");
            }
            other => panic!("Expected BookUpdate, got {:?}", other),
        }
    }

    #[test]
    fn test_coinbase_user_fill_to_proxy_event() {
        let coinbase = json!({
            "channel": "user",
            "events": [{
                "type": "update",
                "orders": [{
                    "order_id": "cb-order-123",
                    "client_order_id": "mm_buy_btc_001",
                    "product_id": "BTC-USD",
                    "order_side": "BUY",
                    "status": "FILLED",
                    "avg_price": "50000",
                    "cumulative_quantity": "0.001",
                    "total_fees": "0.115",
                    "creation_time": "2024-01-01T00:00:00Z",
                    "cancel_reason": ""
                }]
            }]
        });

        let mut tracker = FillTracker::new();
        let results = coinbase_user_to_proxy_events(&coinbase, &mut tracker);
        assert_eq!(results.len(), 1);
        match parse_proxy_event(&results[0]).unwrap() {
            ProxyEvent::Fill { cl_ord_id, symbol, side, .. } => {
                assert_eq!(cl_ord_id, "mm_buy_btc_001");
                assert_eq!(symbol, "BTC/USDC");
                assert_eq!(side, "buy");
            }
            other => panic!("Expected Fill, got {:?}", other),
        }
    }

    #[test]
    fn test_coinbase_user_cancel_to_proxy_event() {
        let coinbase = json!({
            "channel": "user",
            "events": [{
                "type": "update",
                "orders": [{
                    "order_id": "cb-order-456",
                    "client_order_id": "mm_sell_eth_002",
                    "product_id": "ETH-USD",
                    "order_side": "SELL",
                    "status": "CANCELLED",
                    "avg_price": "0",
                    "cumulative_quantity": "0",
                    "total_fees": "0",
                    "creation_time": "2024-01-01T00:00:00Z",
                    "cancel_reason": "USER_CANCEL"
                }]
            }]
        });

        let mut tracker = FillTracker::new();
        let results = coinbase_user_to_proxy_events(&coinbase, &mut tracker);
        assert_eq!(results.len(), 1);
        match parse_proxy_event(&results[0]).unwrap() {
            ProxyEvent::OrderCancelled { cl_ord_id, reason, .. } => {
                assert_eq!(cl_ord_id, "mm_sell_eth_002");
                assert_eq!(reason, Some("USER_CANCEL".to_string()));
            }
            other => panic!("Expected OrderCancelled, got {:?}", other),
        }
    }

    #[test]
    fn test_partial_fill_incremental_quantities() {
        let mut tracker = FillTracker::new();

        // First partial fill: 0.3 of 1.0 at price 100
        let msg1 = json!({
            "channel": "user",
            "events": [{
                "type": "update",
                "orders": [{
                    "order_id": "cb-order-pf",
                    "client_order_id": "mm_buy_pf_001",
                    "product_id": "BTC-USD",
                    "order_side": "BUY",
                    "status": "OPEN",
                    "avg_price": "100",
                    "cumulative_quantity": "0.3",
                    "total_fees": "0.03",
                    "creation_time": "2024-01-01T00:00:00Z",
                    "cancel_reason": ""
                }]
            }]
        });

        let results = coinbase_user_to_proxy_events(&msg1, &mut tracker);
        assert_eq!(results.len(), 1);
        match parse_proxy_event(&results[0]).unwrap() {
            ProxyEvent::Fill { qty, .. } => {
                let qty_f64: f64 = qty.to_string().parse().unwrap();
                assert!((qty_f64 - 0.3).abs() < 0.001,
                    "Expected incremental qty ~0.3, got {}", qty_f64);
            }
            other => panic!("Expected Fill, got {:?}", other),
        }

        // Second partial fill: cumulative 0.7 at weighted avg price 95
        let msg2 = json!({
            "channel": "user",
            "events": [{
                "type": "update",
                "orders": [{
                    "order_id": "cb-order-pf",
                    "client_order_id": "mm_buy_pf_001",
                    "product_id": "BTC-USD",
                    "order_side": "BUY",
                    "status": "OPEN",
                    "avg_price": "95",
                    "cumulative_quantity": "0.7",
                    "total_fees": "0.07",
                    "creation_time": "2024-01-01T00:00:00Z",
                    "cancel_reason": ""
                }]
            }]
        });

        let results = coinbase_user_to_proxy_events(&msg2, &mut tracker);
        assert_eq!(results.len(), 1);
        match parse_proxy_event(&results[0]).unwrap() {
            ProxyEvent::Fill { qty, .. } => {
                let qty_f64: f64 = qty.to_string().parse().unwrap();
                assert!((qty_f64 - 0.4).abs() < 0.001,
                    "Expected incremental qty ~0.4, got {}", qty_f64);
            }
            other => panic!("Expected Fill, got {:?}", other),
        }

        // Final fill: cumulative 1.0
        let msg3 = json!({
            "channel": "user",
            "events": [{
                "type": "update",
                "orders": [{
                    "order_id": "cb-order-pf",
                    "client_order_id": "mm_buy_pf_001",
                    "product_id": "BTC-USD",
                    "order_side": "BUY",
                    "status": "FILLED",
                    "avg_price": "90",
                    "cumulative_quantity": "1.0",
                    "total_fees": "0.10",
                    "creation_time": "2024-01-01T00:00:00Z",
                    "cancel_reason": ""
                }]
            }]
        });

        let results = coinbase_user_to_proxy_events(&msg3, &mut tracker);
        assert_eq!(results.len(), 1);
        match parse_proxy_event(&results[0]).unwrap() {
            ProxyEvent::Fill { qty, is_fully_filled, .. } => {
                let qty_f64: f64 = qty.to_string().parse().unwrap();
                assert!((qty_f64 - 0.3).abs() < 0.001,
                    "Expected incremental qty ~0.3, got {}", qty_f64);
                assert!(is_fully_filled);
            }
            other => panic!("Expected Fill, got {:?}", other),
        }

        // Tracker should have cleaned up after FILLED
        assert_eq!(tracker.len(), 0);
    }

    #[test]
    fn test_open_no_fill_emits_ack() {
        let mut tracker = FillTracker::new();

        let msg = json!({
            "channel": "user",
            "events": [{
                "type": "update",
                "orders": [{
                    "order_id": "cb-order-new",
                    "client_order_id": "mm_buy_new_001",
                    "product_id": "ETH-USD",
                    "order_side": "BUY",
                    "status": "OPEN",
                    "avg_price": "0",
                    "cumulative_quantity": "0",
                    "total_fees": "0",
                    "creation_time": "2024-01-01T00:00:00Z",
                    "cancel_reason": ""
                }]
            }]
        });

        let results = coinbase_user_to_proxy_events(&msg, &mut tracker);
        assert_eq!(results.len(), 1);
        match parse_proxy_event(&results[0]).unwrap() {
            ProxyEvent::OrderAccepted { cl_ord_id, .. } => {
                assert_eq!(cl_ord_id, "mm_buy_new_001");
            }
            other => panic!("Expected OrderAccepted, got {:?}", other),
        }
    }

    #[test]
    fn test_snapshot_seeds_tracker_no_fills() {
        let mut tracker = FillTracker::new();

        // Snapshot with an order that has partial fills
        let msg = json!({
            "channel": "user",
            "events": [{
                "type": "snapshot",
                "orders": [{
                    "order_id": "cb-order-existing",
                    "client_order_id": "mm_buy_exist_001",
                    "product_id": "BTC-USD",
                    "order_side": "BUY",
                    "status": "OPEN",
                    "avg_price": "50000",
                    "cumulative_quantity": "0.5",
                    "total_fees": "1.0",
                    "creation_time": "2024-01-01T00:00:00Z",
                    "cancel_reason": ""
                }]
            }]
        });

        let results = coinbase_user_to_proxy_events(&msg, &mut tracker);
        // Snapshot should produce an OrderAccepted message
        assert_eq!(results.len(), 1);
        assert!(matches!(parse_proxy_event(&results[0]).unwrap(), ProxyEvent::OrderAccepted { .. }));

        // Now if an update arrives with the SAME cumulative_quantity, no fill should be emitted
        let update = json!({
            "channel": "user",
            "events": [{
                "type": "update",
                "orders": [{
                    "order_id": "cb-order-existing",
                    "client_order_id": "mm_buy_exist_001",
                    "product_id": "BTC-USD",
                    "order_side": "BUY",
                    "status": "OPEN",
                    "avg_price": "50000",
                    "cumulative_quantity": "0.5",
                    "total_fees": "1.0",
                    "creation_time": "2024-01-01T00:00:00Z",
                    "cancel_reason": ""
                }]
            }]
        });

        let results = coinbase_user_to_proxy_events(&update, &mut tracker);
        assert_eq!(results.len(), 1);
        // No new fill, should be an ack
        assert!(matches!(parse_proxy_event(&results[0]).unwrap(), ProxyEvent::OrderAccepted { .. }));

        // But a NEW partial fill SHOULD be emitted
        let update2 = json!({
            "channel": "user",
            "events": [{
                "type": "update",
                "orders": [{
                    "order_id": "cb-order-existing",
                    "client_order_id": "mm_buy_exist_001",
                    "product_id": "BTC-USD",
                    "order_side": "BUY",
                    "status": "OPEN",
                    "avg_price": "49000",
                    "cumulative_quantity": "0.8",
                    "total_fees": "1.5",
                    "creation_time": "2024-01-01T00:00:00Z",
                    "cancel_reason": ""
                }]
            }]
        });

        let results = coinbase_user_to_proxy_events(&update2, &mut tracker);
        assert_eq!(results.len(), 1);
        match parse_proxy_event(&results[0]).unwrap() {
            ProxyEvent::Fill { qty, .. } => {
                let qty_f64: f64 = qty.to_string().parse().unwrap();
                assert!((qty_f64 - 0.3).abs() < 0.001,
                    "Expected incremental qty ~0.3, got {}", qty_f64);
            }
            other => panic!("Expected Fill, got {:?}", other),
        }
    }

    #[test]
    fn test_subscribe_to_coinbase() {
        let result = subscribe_to_coinbase("book", &["BTC/USD".to_string(), "ETH/USD".to_string()]).unwrap();
        assert_eq!(result["type"], "subscribe");
        assert_eq!(result["channel"], "level2");
        let product_ids = result["product_ids"].as_array().unwrap();
        assert_eq!(product_ids[0], "BTC-USD");
        assert_eq!(product_ids[1], "ETH-USD");
    }

    #[test]
    fn test_order_response_success() {
        let result = order_response_success("add_order", 42, "cb-123", "mm_buy_001");
        match parse_proxy_event(&result).unwrap() {
            ProxyEvent::OrderAccepted { req_id, cl_ord_id, order_id } => {
                assert_eq!(req_id, 42);
                assert_eq!(cl_ord_id, "mm_buy_001");
                assert_eq!(order_id, "cb-123");
            }
            other => panic!("Expected OrderAccepted, got {:?}", other),
        }
    }

    #[test]
    fn test_synthetic_exec_new() {
        let result = synthetic_exec_new("cb-order-789", "mm_buy_doge_001", "DOGE/USDC", "buy");
        match parse_proxy_event(&result).unwrap() {
            ProxyEvent::OrderAccepted { cl_ord_id, order_id, .. } => {
                assert_eq!(cl_ord_id, "mm_buy_doge_001");
                assert_eq!(order_id, "cb-order-789");
            }
            other => panic!("Expected OrderAccepted, got {:?}", other),
        }
    }

    #[test]
    fn test_synthetic_exec_canceled() {
        let result = synthetic_exec_canceled("mm_sell_xrp_002", "XRP/USDC", "sell");
        match parse_proxy_event(&result).unwrap() {
            ProxyEvent::OrderCancelled { cl_ord_id, reason, .. } => {
                assert_eq!(cl_ord_id, "mm_sell_xrp_002");
                assert_eq!(reason, Some("USER_CANCEL".to_string()));
            }
            other => panic!("Expected OrderCancelled, got {:?}", other),
        }
    }

    #[test]
    fn test_order_response_error() {
        let result = order_response_error("add_order", 99, "Insufficient funds");
        match parse_proxy_event(&result).unwrap() {
            ProxyEvent::OrderRejected { error, .. } => {
                assert_eq!(error, "Insufficient funds");
            }
            other => panic!("Expected OrderRejected, got {:?}", other),
        }
    }

    #[test]
    fn test_batched_multi_product_events() {
        // Coinbase can batch updates for multiple products in a single message.
        // Both products should be translated.
        let coinbase = json!({
            "channel": "l2_data",
            "events": [
                {
                    "type": "update",
                    "product_id": "BTC-USD",
                    "updates": [
                        {"side": "bid", "price_level": "71000.00", "new_quantity": "0.5", "event_time": "2024-01-01T00:00:00Z"}
                    ]
                },
                {
                    "type": "update",
                    "product_id": "OP-USD",
                    "updates": [
                        {"side": "ask", "price_level": "0.121", "new_quantity": "100.0", "event_time": "2024-01-01T00:00:00Z"}
                    ]
                }
            ]
        });

        let results = coinbase_book_to_proxy_events_mapped(&coinbase, &HashMap::new());
        assert_eq!(results.len(), 2, "Both events should be translated");

        match parse_proxy_event(&results[0]).unwrap() {
            ProxyEvent::BookUpdate { symbol, bids, .. } => {
                assert_eq!(symbol, "BTC/USDC");
                assert_eq!(bids.len(), 1);
            }
            other => panic!("Expected BookUpdate for BTC, got {:?}", other),
        }

        match parse_proxy_event(&results[1]).unwrap() {
            ProxyEvent::BookUpdate { symbol, asks, .. } => {
                assert_eq!(symbol, "OP/USDC");
                assert_eq!(asks.len(), 1);
            }
            other => panic!("Expected BookUpdate for OP, got {:?}", other),
        }
    }

    #[test]
    fn test_bad_update_entry_skipped_not_fatal() {
        // A single bad entry in the updates array should be skipped,
        // not cause the entire message to be dropped.
        let coinbase = json!({
            "channel": "l2_data",
            "events": [{
                "type": "update",
                "product_id": "BTC-USD",
                "updates": [
                    {"side": "bid", "price_level": "71000.00", "new_quantity": "0.5", "event_time": "2024-01-01T00:00:00Z"},
                    {"side": "bid", "price_level": null, "new_quantity": "0.1", "event_time": "2024-01-01T00:00:00Z"},
                    {"side": "ask", "price_level": "71001.00", "new_quantity": "0.3", "event_time": "2024-01-01T00:00:00Z"}
                ]
            }]
        });

        let result = coinbase_book_to_proxy_event(&coinbase).unwrap();
        match parse_proxy_event(&result).unwrap() {
            ProxyEvent::BookUpdate { symbol, bids, asks } => {
                assert_eq!(symbol, "BTC/USDC");
                assert_eq!(bids.len(), 1, "Good bid should be kept");
                assert_eq!(asks.len(), 1, "Good ask should be kept");
            }
            other => panic!("Expected BookUpdate, got {:?}", other),
        }
    }

    #[test]
    fn test_scientific_notation_parsed() {
        // Coinbase might send very small values in scientific notation.
        let coinbase = json!({
            "channel": "l2_data",
            "events": [{
                "type": "update",
                "product_id": "PEPE-USD",
                "updates": [
                    {"side": "bid", "price_level": "0.0000034", "new_quantity": "1000000", "event_time": "2024-01-01T00:00:00Z"},
                    {"side": "ask", "price_level": "0.0000035", "new_quantity": "9.7e-7", "event_time": "2024-01-01T00:00:00Z"}
                ]
            }]
        });

        let result = coinbase_book_to_proxy_event(&coinbase).unwrap();
        match parse_proxy_event(&result).unwrap() {
            ProxyEvent::BookUpdate { symbol, bids, asks } => {
                assert_eq!(symbol, "PEPE/USDC");
                assert_eq!(bids.len(), 1);
                assert_eq!(asks.len(), 1);
            }
            other => panic!("Expected BookUpdate, got {:?}", other),
        }
    }

    #[test]
    fn test_empty_string_fields_skipped() {
        // Empty strings for price/qty should be skipped, not crash.
        let coinbase = json!({
            "channel": "l2_data",
            "events": [{
                "type": "update",
                "product_id": "BTC-USD",
                "updates": [
                    {"side": "bid", "price_level": "", "new_quantity": "0.5", "event_time": "2024-01-01T00:00:00Z"},
                    {"side": "ask", "price_level": "71001.00", "new_quantity": "", "event_time": "2024-01-01T00:00:00Z"},
                    {"side": "ask", "price_level": "71002.00", "new_quantity": "0.2", "event_time": "2024-01-01T00:00:00Z"}
                ]
            }]
        });

        let result = coinbase_book_to_proxy_event(&coinbase).unwrap();
        match parse_proxy_event(&result).unwrap() {
            ProxyEvent::BookUpdate { bids, asks, .. } => {
                assert_eq!(bids.len(), 0, "Bad bid should be skipped");
                assert_eq!(asks.len(), 1, "Only good ask should be kept");
            }
            other => panic!("Expected BookUpdate, got {:?}", other),
        }
    }
}
