/// Translation between Kraken WS v2 format (bot protocol) and Coinbase formats.
///
/// The bot speaks Kraken WS v2 protocol. This module translates:
/// - Coinbase level2 book data → Kraken book snapshot/update format
/// - Coinbase user channel events → Kraken execution format
/// - Kraken subscribe messages → Coinbase subscribe format

use crate::pairs;
use serde_json::{json, Value};

/// Translate a Coinbase level2 (l2_data) message to Kraken book format.
///
/// Coinbase format:
/// ```json
/// {"channel":"l2_data","events":[{"type":"snapshot","product_id":"BTC-USD",
///   "updates":[{"side":"bid","price_level":"50000","new_quantity":"1.5"}, ...]}]}
/// ```
///
/// Kraken format:
/// ```json
/// {"channel":"book","type":"snapshot","data":[{"symbol":"BTC/USD",
///   "bids":[{"price":50000.0,"qty":1.5}],"asks":[...]}]}
/// ```
pub fn coinbase_book_to_kraken(coinbase_msg: &Value) -> Option<String> {
    let events = coinbase_msg.get("events")?.as_array()?;

    for event in events {
        let event_type = event.get("type")?.as_str()?;
        let product_id = event.get("product_id")?.as_str()?;
        let symbol = pairs::to_internal(product_id);
        let updates = event.get("updates")?.as_array()?;

        let mut bids = Vec::new();
        let mut asks = Vec::new();

        for update in updates {
            let side = update.get("side")?.as_str()?;
            let price: f64 = update.get("price_level")?.as_str()?.parse().ok()?;
            let qty: f64 = update.get("new_quantity")?.as_str()?.parse().ok()?;
            let level = json!({"price": price, "qty": qty});

            match side {
                "bid" => bids.push(level),
                "ask" | "offer" => asks.push(level),
                _ => {}
            }
        }

        let kraken_type = match event_type {
            "snapshot" => "snapshot",
            "update" => "update",
            _ => continue,
        };

        let kraken_msg = json!({
            "channel": "book",
            "type": kraken_type,
            "data": [{
                "symbol": symbol,
                "bids": bids,
                "asks": asks,
                "checksum": 0
            }]
        });

        return Some(kraken_msg.to_string());
    }

    None
}

/// Translate a Coinbase user channel message to Kraken execution format.
///
/// Coinbase user channel sends order updates with status changes.
/// We translate these to Kraken-style execution reports.
pub fn coinbase_user_to_kraken(coinbase_msg: &Value) -> Vec<String> {
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
            // Translate snapshot of open orders
            let exec_reports: Vec<Value> = orders
                .iter()
                .filter_map(|order| translate_order_to_exec(order, true))
                .collect();

            if !exec_reports.is_empty() {
                let msg = json!({
                    "channel": "executions",
                    "type": "snapshot",
                    "data": exec_reports
                });
                results.push(msg.to_string());
            }
        } else {
            // Individual order updates
            for order in orders {
                if let Some(exec_report) = translate_order_to_exec(order, false) {
                    let msg = json!({
                        "channel": "executions",
                        "type": "update",
                        "data": [exec_report]
                    });
                    results.push(msg.to_string());
                }
            }
        }
    }

    results
}

/// Translate a single Coinbase order object to a Kraken-style execution report.
fn translate_order_to_exec(order: &Value, _is_snapshot: bool) -> Option<Value> {
    let status = order.get("status")?.as_str()?;
    let product_id = order.get("product_id")?.as_str()?;
    let symbol = pairs::to_internal(product_id);
    let order_id = order.get("order_id")?.as_str().unwrap_or("");
    let client_order_id = order.get("client_order_id")?.as_str().unwrap_or("");
    let side = order.get("order_side")?.as_str().unwrap_or("").to_lowercase();

    // Map Coinbase status to Kraken exec_type
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

    let mut report = json!({
        "exec_type": exec_type,
        "order_id": order_id,
        "cl_ord_id": client_order_id,
        "symbol": symbol,
        "side": side,
        "last_qty": filled_qty,
        "last_price": avg_price,
        "fees": [{"asset": "USD", "qty": total_fees}],
        "order_status": order_status,
        "liquidity_ind": "m",
        "timestamp": timestamp
    });

    if let Some(reason) = cancel_reason {
        report["reason"] = json!(reason);
    }

    Some(report)
}

/// Translate a Kraken-format subscribe message to Coinbase format.
/// Returns (channel, product_ids) for the Coinbase subscription.
///
/// Kraken subscribe:
/// ```json
/// {"method":"subscribe","params":{"channel":"book","symbol":["BTC/USD"],"depth":10}}
/// ```
///
/// Coinbase subscribe:
/// ```json
/// {"type":"subscribe","product_ids":["BTC-USD"],"channel":"level2"}
/// ```
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

    Some(json!({
        "type": "subscribe",
        "product_ids": product_ids,
        "channel": coinbase_channel
    }))
}

/// Build a Kraken-format subscribe confirmation response.
pub fn subscribe_confirmed(channel: &str) -> String {
    json!({
        "method": "subscribe",
        "result": {
            "channel": channel
        },
        "success": true
    })
    .to_string()
}

/// Build a Kraken-format order response (success).
pub fn order_response_success(method: &str, req_id: u64, order_id: &str, cl_ord_id: &str) -> String {
    json!({
        "method": method,
        "req_id": req_id,
        "success": true,
        "result": {
            "order_id": order_id,
            "cl_ord_id": cl_ord_id
        }
    })
    .to_string()
}

/// Build a Kraken-format order response (error).
pub fn order_response_error(method: &str, req_id: u64, error: &str) -> String {
    json!({
        "method": method,
        "req_id": req_id,
        "success": false,
        "error": error
    })
    .to_string()
}

/// Build a Kraken-format pong response.
pub fn pong_response(req_id: u64) -> String {
    json!({
        "method": "pong",
        "req_id": req_id
    })
    .to_string()
}

/// Build a Kraken-format heartbeat message.
pub fn heartbeat() -> String {
    json!({"channel": "heartbeat"}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["channel"], "book");
        assert_eq!(parsed["type"], "snapshot");
        assert_eq!(parsed["data"][0]["symbol"], "BTC/USD");
        assert_eq!(parsed["data"][0]["bids"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["data"][0]["asks"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["data"][0]["bids"][0]["price"], 50000.0);
        assert_eq!(parsed["data"][0]["bids"][0]["qty"], 1.5);
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
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["channel"], "book");
        assert_eq!(parsed["type"], "update");
        assert_eq!(parsed["data"][0]["symbol"], "ETH/USD");
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

        let results = coinbase_user_to_kraken(&coinbase);
        assert_eq!(results.len(), 1);
        let parsed: Value = serde_json::from_str(&results[0]).unwrap();

        assert_eq!(parsed["channel"], "executions");
        assert_eq!(parsed["type"], "update");
        assert_eq!(parsed["data"][0]["exec_type"], "trade");
        assert_eq!(parsed["data"][0]["symbol"], "BTC/USD");
        assert_eq!(parsed["data"][0]["side"], "buy");
        assert_eq!(parsed["data"][0]["order_status"], "filled");
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

        let results = coinbase_user_to_kraken(&coinbase);
        assert_eq!(results.len(), 1);
        let parsed: Value = serde_json::from_str(&results[0]).unwrap();
        assert_eq!(parsed["data"][0]["exec_type"], "canceled");
        assert_eq!(parsed["data"][0]["reason"], "USER_CANCEL");
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
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["method"], "add_order");
        assert_eq!(parsed["req_id"], 42);
        assert!(parsed["success"].as_bool().unwrap());
        assert_eq!(parsed["result"]["order_id"], "cb-123");
        assert_eq!(parsed["result"]["cl_ord_id"], "mm_buy_001");
    }

    #[test]
    fn test_order_response_error() {
        let result = order_response_error("add_order", 99, "Insufficient funds");
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["req_id"], 99);
        assert!(!parsed["success"].as_bool().unwrap());
        assert_eq!(parsed["error"], "Insufficient funds");
    }
}
