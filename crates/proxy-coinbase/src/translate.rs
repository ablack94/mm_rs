/// Translation between proxy protocol (bot) and Coinbase formats.
///
/// The bot speaks the proxy wire protocol. This module translates:
/// - Coinbase level2 book data → protocol book snapshot/update format
/// - Coinbase user channel events → protocol execution format
/// - Protocol subscribe messages → Coinbase subscribe format

use crate::pairs;
use serde_json::Value;
use std::collections::HashMap;
use trading_primitives::protocol;

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

/// Translate a Coinbase level2 (l2_data) message to proxy protocol book format.
#[cfg(test)]
pub fn coinbase_book_to_kraken(coinbase_msg: &Value) -> Option<String> {
    coinbase_book_to_kraken_mapped(coinbase_msg, &std::collections::HashMap::new())
}

/// Translate a Coinbase level2 (l2_data) message to proxy protocol book format,
/// using a product_id → internal symbol mapping.
///
/// Coinbase merges USD/USDC order books and always returns the base-USD product_id
/// in level2 data. The mapping allows relabeling (e.g., "DOGE-USD" → "DOGE/USDC").
pub fn coinbase_book_to_kraken_mapped(
    coinbase_msg: &Value,
    product_id_map: &std::collections::HashMap<String, String>,
) -> Option<String> {
    let events = coinbase_msg.get("events")?.as_array()?;

    for event in events {
        let event_type = event.get("type")?.as_str()?;
        let product_id = event.get("product_id")?.as_str()?;
        // Use mapped symbol if available, otherwise fall back to simple conversion
        let symbol = product_id_map
            .get(product_id)
            .cloned()
            .unwrap_or_else(|| pairs::to_internal(product_id));
        let updates = event.get("updates")?.as_array()?;

        let mut bids = Vec::new();
        let mut asks = Vec::new();

        for update in updates {
            let side = update.get("side")?.as_str()?;
            let price: f64 = update.get("price_level")?.as_str()?.parse().ok()?;
            let qty: f64 = update.get("new_quantity")?.as_str()?.parse().ok()?;
            let level = protocol::build_book_level(price, qty);

            match side {
                "bid" => bids.push(level),
                "ask" | "offer" => asks.push(level),
                _ => {}
            }
        }

        let msg_type = match event_type {
            "snapshot" => "snapshot",
            "update" => "update",
            _ => continue,
        };

        return Some(protocol::build_book_message(msg_type, &symbol, bids, asks));
    }

    None
}

/// Translate a Coinbase user channel message to proxy protocol execution format.
///
/// The `fill_tracker` converts Coinbase's cumulative fill data to incremental
/// quantities expected by the bot. For snapshot events, it seeds the tracker
/// without emitting fills; for updates, it computes deltas and emits trade events.
pub fn coinbase_user_to_kraken(
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
            let exec_reports: Vec<Value> = orders
                .iter()
                .filter_map(|order| {
                    seed_tracker_from_order(order, fill_tracker);
                    translate_order_to_exec(order)
                })
                .collect();

            if !exec_reports.is_empty() {
                results.push(protocol::build_execution_snapshot(exec_reports));
            }
        } else {
            for order in orders {
                if let Some(exec_report) =
                    translate_order_to_exec_tracked(order, fill_tracker)
                {
                    results.push(protocol::build_execution_update(exec_report));
                }
            }
        }
    }

    results
}

/// Translate a single Coinbase order object to a protocol execution report.
fn translate_order_to_exec(order: &Value) -> Option<Value> {
    let status = order.get("status")?.as_str()?;
    let product_id = order.get("product_id")?.as_str()?;
    let symbol = pairs::to_internal(product_id);
    let order_id = order.get("order_id")?.as_str().unwrap_or("");
    let client_order_id = order.get("client_order_id")?.as_str().unwrap_or("");
    let side = order.get("order_side")?.as_str().unwrap_or("").to_lowercase();

    // Map Coinbase status to protocol exec_type
    let (exec_type, order_status) = match status {
        "FILLED" => ("trade", "filled"),
        "OPEN" => ("new", "open"),
        "PENDING" => ("pending_new", "pending"),
        "CANCELLED" => ("canceled", "canceled"),
        "EXPIRED" => ("expired", "expired"),
        "FAILED" => ("canceled", "canceled"),
        _ => return None,
    };

    let avg_price: f64 = order
        .get("avg_price")
        .and_then(|p| p.as_str())
        .and_then(|p| p.parse().ok())
        .unwrap_or(0.0);

    let filled_qty: f64 = order
        .get("cumulative_quantity")
        .and_then(|q| q.as_str())
        .and_then(|q| q.parse().ok())
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

    Some(protocol::build_exec_report(
        exec_type,
        order_id,
        client_order_id,
        &symbol,
        &side,
        filled_qty,
        avg_price,
        total_fees,
        order_status,
        "m", // assume maker (Coinbase doesn't always report this)
        timestamp,
        cancel_reason,
    ))
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

/// Translate a Coinbase order to a protocol exec report, using the fill tracker
/// to convert cumulative quantities to incremental.
///
/// Key behaviors:
/// - OPEN + cumulative_qty > 0 with new fill delta → exec_type "trade" (partial fill)
/// - OPEN + no new fill → exec_type "new" (order ack or duplicate update)
/// - FILLED → exec_type "trade" with incremental delta from last partial
/// - Terminal states (CANCELLED/EXPIRED/FAILED) → clean up tracker
fn translate_order_to_exec_tracked(
    order: &Value,
    tracker: &mut FillTracker,
) -> Option<Value> {
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
                let order_status = if status == "FILLED" { "filled" } else { "open" };
                if status == "FILLED" {
                    tracker.remove(client_order_id);
                }
                Some(protocol::build_exec_report(
                    "trade",
                    order_id,
                    client_order_id,
                    &symbol,
                    &side,
                    inc_qty,
                    inc_price,
                    inc_fees,
                    order_status,
                    "m",
                    timestamp,
                    cancel_reason,
                ))
            } else if status == "OPEN" {
                // No fill delta — this is an order ack or duplicate update
                Some(protocol::build_exec_report(
                    "new",
                    order_id,
                    client_order_id,
                    &symbol,
                    &side,
                    0.0,
                    0.0,
                    0.0,
                    "open",
                    "m",
                    timestamp,
                    cancel_reason,
                ))
            } else {
                // FILLED with no delta (e.g., duplicate) — still report it
                tracker.remove(client_order_id);
                Some(protocol::build_exec_report(
                    "trade",
                    order_id,
                    client_order_id,
                    &symbol,
                    &side,
                    cum_qty,
                    avg_price,
                    total_fees,
                    "filled",
                    "m",
                    timestamp,
                    cancel_reason,
                ))
            }
        }
        "PENDING" => Some(protocol::build_exec_report(
            "pending_new",
            order_id,
            client_order_id,
            &symbol,
            &side,
            0.0,
            0.0,
            0.0,
            "pending",
            "m",
            timestamp,
            cancel_reason,
        )),
        "CANCELLED" | "EXPIRED" | "FAILED" => {
            tracker.remove(client_order_id);
            let (exec_type, order_status) = match status {
                "CANCELLED" => ("canceled", "canceled"),
                "EXPIRED" => ("expired", "expired"),
                "FAILED" => ("canceled", "canceled"),
                _ => unreachable!(),
            };
            Some(protocol::build_exec_report(
                exec_type,
                order_id,
                client_order_id,
                &symbol,
                &side,
                0.0,
                0.0,
                0.0,
                order_status,
                "m",
                timestamp,
                cancel_reason,
            ))
        }
        _ => None,
    }
}

/// Translate a protocol subscribe message to Coinbase format.
/// Returns the Coinbase subscribe JSON (without auth fields — caller adds those).
pub fn kraken_subscribe_to_coinbase(kraken_msg: &Value) -> Option<Value> {
    let params = kraken_msg.get("params")?;
    let channel = params.get("channel")?.as_str()?;
    let symbols = params.get("symbol")?.as_array()?;

    let product_ids: Vec<String> = symbols
        .iter()
        .filter_map(|s| s.as_str())
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

/// Build a protocol subscribe confirmation response.
pub fn subscribe_confirmed(channel: &str) -> String {
    protocol::build_subscribe_confirmed(channel)
}

/// Build a protocol order response (success).
pub fn order_response_success(method: &str, req_id: u64, order_id: &str, cl_ord_id: &str) -> String {
    protocol::build_order_response_success(method, req_id, order_id, cl_ord_id)
}

/// Build a protocol order response (error).
pub fn order_response_error(method: &str, req_id: u64, error: &str) -> String {
    protocol::build_order_response_error(method, req_id, error)
}

/// Build a protocol pong response.
pub fn pong_response(req_id: u64) -> String {
    protocol::build_pong(req_id)
}

/// Build a synthetic "new" execution report for when an order is accepted via REST.
///
/// Coinbase's user WS may deliver an OPEN event later, but the bot needs an immediate
/// ack to track the order. Duplicate acks are harmless.
pub fn synthetic_exec_new(order_id: &str, cl_ord_id: &str, symbol: &str, side: &str) -> String {
    let report = protocol::build_exec_report(
        "new",
        order_id,
        cl_ord_id,
        symbol,
        side,
        0.0,  // no fill yet
        0.0,  // no price yet
        0.0,  // no fees yet
        "open",
        "m",
        "1970-01-01T00:00:00Z",
        None,
    );
    protocol::build_execution_update(report)
}

/// Build a synthetic "canceled" execution report for when a cancel succeeds via REST.
pub fn synthetic_exec_canceled(cl_ord_id: &str, symbol: &str, side: &str) -> String {
    let report = protocol::build_exec_report(
        "canceled",
        cl_ord_id,  // use cl_ord_id as order_id too
        cl_ord_id,
        symbol,
        side,
        0.0,
        0.0,
        0.0,
        "canceled",
        "m",
        "1970-01-01T00:00:00Z",
        Some("USER_CANCEL"),
    );
    protocol::build_execution_update(report)
}

/// Build a protocol heartbeat message.
pub fn heartbeat() -> String {
    protocol::build_heartbeat()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use trading_primitives::protocol::{parse_ws_message, WsMessage};

    #[test]
    fn test_coinbase_book_snapshot_to_kraken() {
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

        let result = coinbase_book_to_kraken(&coinbase).unwrap();
        // Verify it parses correctly through the protocol parser
        match parse_ws_message(&result) {
            WsMessage::BookSnapshot { pair, bids, asks } => {
                assert_eq!(pair.to_string(), "BTC/USDC");
                assert_eq!(bids.len(), 2);
                assert_eq!(asks.len(), 1);
            }
            other => panic!("Expected BookSnapshot, got {:?}", other),
        }
    }

    #[test]
    fn test_coinbase_book_update_to_kraken() {
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

        let result = coinbase_book_to_kraken(&coinbase).unwrap();
        match parse_ws_message(&result) {
            WsMessage::BookUpdate { pair, .. } => {
                assert_eq!(pair.to_string(), "ETH/USDC");
            }
            other => panic!("Expected BookUpdate, got {:?}", other),
        }
    }

    #[test]
    fn test_coinbase_user_fill_to_kraken() {
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
        let results = coinbase_user_to_kraken(&coinbase, &mut tracker);
        assert_eq!(results.len(), 1);
        // Verify it parses correctly through the protocol parser
        match parse_ws_message(&results[0]) {
            WsMessage::Execution(report) => {
                assert_eq!(report.exec_type, "trade");
                assert_eq!(report.pair.to_string(), "BTC/USDC");
                assert_eq!(report.side, "buy");
            }
            other => panic!("Expected Execution, got {:?}", other),
        }
    }

    #[test]
    fn test_coinbase_user_cancel_to_kraken() {
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
        let results = coinbase_user_to_kraken(&coinbase, &mut tracker);
        assert_eq!(results.len(), 1);
        match parse_ws_message(&results[0]) {
            WsMessage::Execution(report) => {
                assert_eq!(report.exec_type, "canceled");
                assert_eq!(report.cancel_reason, Some("USER_CANCEL".to_string()));
            }
            other => panic!("Expected Execution, got {:?}", other),
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

        let results = coinbase_user_to_kraken(&msg1, &mut tracker);
        assert_eq!(results.len(), 1);
        match parse_ws_message(&results[0]) {
            WsMessage::Execution(report) => {
                assert_eq!(report.exec_type, "trade");
                // Incremental: 0.3 (first fill, no previous)
                assert_eq!(report.last_qty.to_string(), "0.3");
            }
            other => panic!("Expected Execution trade, got {:?}", other),
        }

        // Second partial fill: cumulative 0.7 at weighted avg price 95
        // Incremental: 0.4 units, cost delta = 0.7*95 - 0.3*100 = 66.5 - 30 = 36.5, price = 91.25
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

        let results = coinbase_user_to_kraken(&msg2, &mut tracker);
        assert_eq!(results.len(), 1);
        match parse_ws_message(&results[0]) {
            WsMessage::Execution(report) => {
                assert_eq!(report.exec_type, "trade");
                // Incremental qty: 0.7 - 0.3 = 0.4
                let qty_f64: f64 = report.last_qty.to_string().parse().unwrap();
                assert!((qty_f64 - 0.4).abs() < 0.001,
                    "Expected incremental qty ~0.4, got {}", qty_f64);
            }
            other => panic!("Expected Execution trade, got {:?}", other),
        }

        // Final fill: cumulative 1.0 at weighted avg price 90
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

        let results = coinbase_user_to_kraken(&msg3, &mut tracker);
        assert_eq!(results.len(), 1);
        match parse_ws_message(&results[0]) {
            WsMessage::Execution(report) => {
                assert_eq!(report.exec_type, "trade");
                // Incremental qty: 1.0 - 0.7 = 0.3
                let qty_f64: f64 = report.last_qty.to_string().parse().unwrap();
                assert!((qty_f64 - 0.3).abs() < 0.001,
                    "Expected incremental qty ~0.3, got {}", qty_f64);
            }
            other => panic!("Expected Execution trade, got {:?}", other),
        }

        // Tracker should have cleaned up after FILLED
        assert_eq!(tracker.len(), 0);
    }

    #[test]
    fn test_open_no_fill_emits_ack() {
        let mut tracker = FillTracker::new();

        // OPEN with zero cumulative_quantity is just an ack
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

        let results = coinbase_user_to_kraken(&msg, &mut tracker);
        assert_eq!(results.len(), 1);
        match parse_ws_message(&results[0]) {
            WsMessage::Execution(report) => {
                assert_eq!(report.exec_type, "new");
            }
            other => panic!("Expected Execution new, got {:?}", other),
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

        let results = coinbase_user_to_kraken(&msg, &mut tracker);
        // Snapshot should produce an execution snapshot message (ack, not trade)
        assert_eq!(results.len(), 1);

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

        let results = coinbase_user_to_kraken(&update, &mut tracker);
        assert_eq!(results.len(), 1);
        match parse_ws_message(&results[0]) {
            WsMessage::Execution(report) => {
                // No new fill, should be an ack
                assert_eq!(report.exec_type, "new");
            }
            other => panic!("Expected Execution new (no fill delta), got {:?}", other),
        }

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

        let results = coinbase_user_to_kraken(&update2, &mut tracker);
        assert_eq!(results.len(), 1);
        match parse_ws_message(&results[0]) {
            WsMessage::Execution(report) => {
                assert_eq!(report.exec_type, "trade");
                // Incremental: 0.8 - 0.5 = 0.3
                let qty_f64: f64 = report.last_qty.to_string().parse().unwrap();
                assert!((qty_f64 - 0.3).abs() < 0.001,
                    "Expected incremental qty ~0.3, got {}", qty_f64);
            }
            other => panic!("Expected Execution trade, got {:?}", other),
        }
    }

    #[test]
    fn test_kraken_subscribe_to_coinbase() {
        let kraken = json!({
            "method": "subscribe",
            "params": {
                "channel": "book",
                "symbol": ["BTC/USD", "ETH/USD"],
                "depth": 10
            }
        });

        let result = kraken_subscribe_to_coinbase(&kraken).unwrap();
        assert_eq!(result["type"], "subscribe");
        assert_eq!(result["channel"], "level2");
        let product_ids = result["product_ids"].as_array().unwrap();
        assert_eq!(product_ids[0], "BTC-USD");
        assert_eq!(product_ids[1], "ETH-USD");
    }

    #[test]
    fn test_order_response_success() {
        let result = order_response_success("add_order", 42, "cb-123", "mm_buy_001");
        match parse_ws_message(&result) {
            WsMessage::OrderResponse { req_id, success, method, order_id, cl_ord_id, .. } => {
                assert_eq!(req_id, 42);
                assert!(success);
                assert_eq!(method, "add_order");
                assert_eq!(order_id, Some("cb-123".to_string()));
                assert_eq!(cl_ord_id, Some("mm_buy_001".to_string()));
            }
            other => panic!("Expected OrderResponse, got {:?}", other),
        }
    }

    #[test]
    fn test_synthetic_exec_new() {
        let result = synthetic_exec_new("cb-order-789", "mm_buy_doge_001", "DOGE/USDC", "buy");
        match parse_ws_message(&result) {
            WsMessage::Execution(report) => {
                assert_eq!(report.exec_type, "new");
                assert_eq!(report.order_id, "cb-order-789");
                assert_eq!(report.cl_ord_id, "mm_buy_doge_001");
                assert_eq!(report.pair.to_string(), "DOGE/USDC");
                assert_eq!(report.side, "buy");
            }
            other => panic!("Expected Execution, got {:?}", other),
        }
    }

    #[test]
    fn test_synthetic_exec_canceled() {
        let result = synthetic_exec_canceled("mm_sell_xrp_002", "XRP/USDC", "sell");
        match parse_ws_message(&result) {
            WsMessage::Execution(report) => {
                assert_eq!(report.exec_type, "canceled");
                assert_eq!(report.cl_ord_id, "mm_sell_xrp_002");
                assert_eq!(report.pair.to_string(), "XRP/USDC");
                assert_eq!(report.cancel_reason, Some("USER_CANCEL".to_string()));
            }
            other => panic!("Expected Execution, got {:?}", other),
        }
    }

    #[test]
    fn test_order_response_error() {
        let result = order_response_error("add_order", 99, "Insufficient funds");
        match parse_ws_message(&result) {
            WsMessage::OrderResponse { success, error, .. } => {
                assert!(!success);
                assert_eq!(error, Some("Insufficient funds".to_string()));
            }
            other => panic!("Expected OrderResponse, got {:?}", other),
        }
    }
}
