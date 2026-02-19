//! Proxy wire protocol — the canonical message format between bot and proxy.
//!
//! The bot speaks this protocol over WebSocket to any exchange proxy.
//! Each proxy translates its exchange's native format to/from this protocol.
//! The format originated from Kraken WS v2 but is now the system's internal standard.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::book::LevelUpdate;

// ---------------------------------------------------------------------------
// Exchange capabilities (proxy → bot, queried at startup via REST)
// ---------------------------------------------------------------------------

/// Capabilities advertised by a proxy, queried at startup.
/// The bot uses this to skip unsupported features (e.g., DMS on Coinbase).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeCapabilities {
    /// Whether the exchange supports dead man's switch (cancel-all-after).
    pub dead_man_switch: bool,
}

impl Default for ExchangeCapabilities {
    fn default() -> Self {
        Self {
            dead_man_switch: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Parsed incoming messages (proxy/exchange → bot)
// ---------------------------------------------------------------------------

/// Parsed WS message from the proxy.
#[derive(Debug)]
pub enum WsMessage {
    BookSnapshot {
        symbol: String,
        bids: Vec<LevelUpdate>,
        asks: Vec<LevelUpdate>,
    },
    BookUpdate {
        symbol: String,
        bid_updates: Vec<LevelUpdate>,
        ask_updates: Vec<LevelUpdate>,
    },
    Execution(ExecReport),
    ExecutionSnapshot(Vec<ExecReport>),
    SubscribeConfirmed {
        channel: String,
    },
    OrderResponse {
        req_id: u64,
        success: bool,
        method: String,
        order_id: Option<String>,
        cl_ord_id: Option<String>,
        error: Option<String>,
    },
    Heartbeat,
    Pong,
    Unknown(String),
}

/// An execution report from the exchange (trade fill, order status change, etc).
#[derive(Debug)]
pub struct ExecReport {
    pub exec_type: String,
    pub order_id: String,
    pub cl_ord_id: String,
    pub symbol: String,
    pub side: String,
    pub last_qty: Decimal,
    pub last_price: Decimal,
    pub fee: Decimal,
    pub order_status: String,
    pub is_maker: bool,
    pub timestamp: DateTime<Utc>,
    /// Reason for cancellation (populated on canceled/expired exec_type).
    pub cancel_reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Parse a raw WS JSON message into our typed enum.
pub fn parse_ws_message(raw: &str) -> WsMessage {
    let v: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return WsMessage::Unknown(raw.to_string()),
    };

    if let Some(channel) = v.get("channel").and_then(|c| c.as_str()) {
        match channel {
            "book" => parse_book(&v),
            "executions" => parse_executions(&v),
            "heartbeat" => WsMessage::Heartbeat,
            _ => WsMessage::Unknown(raw.to_string()),
        }
    } else if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
        match method {
            "pong" => WsMessage::Pong,
            "subscribe" => {
                let ch = v["result"]["channel"].as_str().unwrap_or("").to_string();
                WsMessage::SubscribeConfirmed { channel: ch }
            }
            _ => parse_order_response(&v, method),
        }
    } else {
        WsMessage::Unknown(raw.to_string())
    }
}

fn parse_book(v: &Value) -> WsMessage {
    let msg_type = v["type"].as_str().unwrap_or("");
    let data = match v["data"].as_array().and_then(|a| a.first()) {
        Some(d) => d,
        None => return WsMessage::Unknown(v.to_string()),
    };
    let symbol = data["symbol"].as_str().unwrap_or("").to_string();
    let bids = parse_levels(&data["bids"]);
    let asks = parse_levels(&data["asks"]);

    match msg_type {
        "snapshot" => WsMessage::BookSnapshot { symbol, bids, asks },
        "update" => WsMessage::BookUpdate {
            symbol,
            bid_updates: bids,
            ask_updates: asks,
        },
        _ => WsMessage::Unknown(v.to_string()),
    }
}

fn parse_levels(v: &Value) -> Vec<LevelUpdate> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|entry| {
                    let price = Decimal::try_from(entry["price"].as_f64()?).ok()?;
                    let qty = Decimal::try_from(entry["qty"].as_f64()?).ok()?;
                    Some(LevelUpdate { price, qty })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_executions(v: &Value) -> WsMessage {
    let msg_type = v["type"].as_str().unwrap_or("");

    if msg_type == "snapshot" {
        let trades: Vec<ExecReport> = v["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter(|d| d["exec_type"].as_str() == Some("trade"))
                    .filter_map(|d| parse_single_exec(d))
                    .collect()
            })
            .unwrap_or_default();
        return WsMessage::ExecutionSnapshot(trades);
    }

    let data = match v["data"].as_array().and_then(|a| a.first()) {
        Some(d) => d,
        None => return WsMessage::Unknown(v.to_string()),
    };

    match parse_single_exec(data) {
        Some(report) => WsMessage::Execution(report),
        None => WsMessage::Unknown(v.to_string()),
    }
}

fn parse_single_exec(data: &Value) -> Option<ExecReport> {
    let fee = data["fees"]
        .as_array()
        .and_then(|fees| fees.iter().map(|f| f["qty"].as_f64().unwrap_or(0.0)).reduce(|a, b| a + b))
        .and_then(|f| Decimal::try_from(f).ok())
        .unwrap_or_default();

    let ts = data["timestamp"]
        .as_str()
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        .unwrap_or_else(Utc::now);

    Some(ExecReport {
        exec_type: data["exec_type"].as_str().unwrap_or("").to_string(),
        order_id: data["order_id"].as_str().unwrap_or("").to_string(),
        cl_ord_id: data["cl_ord_id"].as_str().unwrap_or("").to_string(),
        symbol: data["symbol"].as_str().unwrap_or("").to_string(),
        side: data["side"].as_str().unwrap_or("").to_string(),
        last_qty: Decimal::try_from(data["last_qty"].as_f64().unwrap_or(0.0)).unwrap_or_default(),
        last_price: Decimal::try_from(data["last_price"].as_f64().unwrap_or(0.0)).unwrap_or_default(),
        fee,
        order_status: data["order_status"].as_str().unwrap_or("").to_string(),
        is_maker: data["liquidity_ind"].as_str() == Some("m"),
        timestamp: ts,
        cancel_reason: data["reason"].as_str().map(String::from),
    })
}

fn parse_order_response(v: &Value, method: &str) -> WsMessage {
    WsMessage::OrderResponse {
        req_id: v["req_id"].as_u64().unwrap_or(0),
        success: v["success"].as_bool().unwrap_or(false),
        method: method.to_string(),
        order_id: v["result"]["order_id"].as_str().map(String::from),
        cl_ord_id: v["result"]["cl_ord_id"].as_str().map(String::from),
        error: v["error"].as_str().map(String::from),
    }
}

// ---------------------------------------------------------------------------
// Builder functions (for proxies constructing protocol-compliant messages)
// ---------------------------------------------------------------------------

/// Build a book snapshot or update message.
pub fn build_book_message(
    msg_type: &str,
    symbol: &str,
    bids: Vec<Value>,
    asks: Vec<Value>,
) -> String {
    json!({
        "channel": "book",
        "type": msg_type,
        "data": [{
            "symbol": symbol,
            "bids": bids,
            "asks": asks,
            "checksum": 0
        }]
    })
    .to_string()
}

/// Build a single price level for book messages.
pub fn build_book_level(price: f64, qty: f64) -> Value {
    json!({"price": price, "qty": qty})
}

/// Build an execution update message (single report).
pub fn build_execution_update(exec_report: Value) -> String {
    json!({
        "channel": "executions",
        "type": "update",
        "data": [exec_report]
    })
    .to_string()
}

/// Build an execution snapshot message (multiple reports).
pub fn build_execution_snapshot(exec_reports: Vec<Value>) -> String {
    json!({
        "channel": "executions",
        "type": "snapshot",
        "data": exec_reports
    })
    .to_string()
}

/// Build an execution report JSON value.
pub fn build_exec_report(
    exec_type: &str,
    order_id: &str,
    cl_ord_id: &str,
    symbol: &str,
    side: &str,
    last_qty: f64,
    last_price: f64,
    total_fees: f64,
    order_status: &str,
    liquidity_ind: &str,
    timestamp: &str,
    cancel_reason: Option<&str>,
) -> Value {
    let mut report = json!({
        "exec_type": exec_type,
        "order_id": order_id,
        "cl_ord_id": cl_ord_id,
        "symbol": symbol,
        "side": side,
        "last_qty": last_qty,
        "last_price": last_price,
        "fees": [{"asset": "USD", "qty": total_fees}],
        "order_status": order_status,
        "liquidity_ind": liquidity_ind,
        "timestamp": timestamp
    });

    if let Some(reason) = cancel_reason {
        report["reason"] = json!(reason);
    }

    report
}

/// Build a subscribe confirmation response.
pub fn build_subscribe_confirmed(channel: &str) -> String {
    json!({
        "method": "subscribe",
        "result": {
            "channel": channel
        },
        "success": true
    })
    .to_string()
}

/// Build an order response (success).
pub fn build_order_response_success(method: &str, req_id: u64, order_id: &str, cl_ord_id: &str) -> String {
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

/// Build an order response (error).
pub fn build_order_response_error(method: &str, req_id: u64, error: &str) -> String {
    json!({
        "method": method,
        "req_id": req_id,
        "success": false,
        "error": error
    })
    .to_string()
}

/// Build a pong response.
pub fn build_pong(req_id: u64) -> String {
    json!({
        "method": "pong",
        "req_id": req_id
    })
    .to_string()
}

/// Build a heartbeat message.
pub fn build_heartbeat() -> String {
    json!({"channel": "heartbeat"}).to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_book_snapshot_parsing() {
        let raw = r#"{
            "channel": "book",
            "type": "snapshot",
            "data": [{
                "symbol": "CAMP/USD",
                "bids": [
                    {"price": 0.0039, "qty": 50000.0},
                    {"price": 0.0038, "qty": 25000.0}
                ],
                "asks": [
                    {"price": 0.0041, "qty": 50000.0},
                    {"price": 0.0042, "qty": 30000.0}
                ],
                "checksum": 0
            }]
        }"#;
        let msg = parse_ws_message(raw);
        match msg {
            WsMessage::BookSnapshot { symbol, bids, asks } => {
                assert_eq!(symbol, "CAMP/USD");
                assert_eq!(bids.len(), 2);
                assert_eq!(asks.len(), 2);
                assert_eq!(bids[0].price, dec!(0.0039));
                assert_eq!(bids[0].qty, dec!(50000.0));
            }
            other => panic!("Expected BookSnapshot, got {:?}", other),
        }
    }

    #[test]
    fn test_book_update_parsing() {
        let raw = r#"{
            "channel": "book",
            "type": "update",
            "data": [{
                "symbol": "BTC/USD",
                "bids": [{"price": 97500.0, "qty": 1.5}],
                "asks": [{"price": 97600.0, "qty": 0.8}],
                "checksum": 12345
            }]
        }"#;
        let msg = parse_ws_message(raw);
        match msg {
            WsMessage::BookUpdate { symbol, bid_updates, ask_updates } => {
                assert_eq!(symbol, "BTC/USD");
                assert_eq!(bid_updates.len(), 1);
                assert_eq!(ask_updates.len(), 1);
            }
            other => panic!("Expected BookUpdate, got {:?}", other),
        }
    }

    #[test]
    fn test_execution_report_trade() {
        let raw = r#"{
            "channel": "executions",
            "type": "update",
            "data": [{
                "exec_type": "trade",
                "order_id": "ORD-ABC-123",
                "cl_ord_id": "mm_buy_camp_001",
                "symbol": "CAMP/USD",
                "side": "buy",
                "last_qty": 10000.0,
                "last_price": 0.0039,
                "fees": [{"asset": "USD", "qty": 0.09}],
                "order_status": "filled",
                "liquidity_ind": "m",
                "timestamp": "2026-02-15T12:00:00.000Z"
            }]
        }"#;
        let msg = parse_ws_message(raw);
        match msg {
            WsMessage::Execution(report) => {
                assert_eq!(report.exec_type, "trade");
                assert_eq!(report.cl_ord_id, "mm_buy_camp_001");
                assert_eq!(report.last_qty, dec!(10000.0));
                assert_eq!(report.fee, dec!(0.09));
                assert!(report.is_maker);
            }
            other => panic!("Expected Execution, got {:?}", other),
        }
    }

    #[test]
    fn test_execution_snapshot_parsed() {
        let raw = r#"{
            "channel": "executions",
            "type": "snapshot",
            "data": [{
                "exec_type": "trade",
                "order_id": "ORD-OLD-999",
                "cl_ord_id": "old_order",
                "symbol": "ETH/USD",
                "side": "sell",
                "last_qty": 0.1,
                "last_price": 3000.0,
                "fees": [],
                "order_status": "filled",
                "liquidity_ind": "t",
                "timestamp": "2026-01-01T00:00:00.000Z"
            }]
        }"#;
        let msg = parse_ws_message(raw);
        match msg {
            WsMessage::ExecutionSnapshot(trades) => {
                assert_eq!(trades.len(), 1);
                assert_eq!(trades[0].symbol, "ETH/USD");
            }
            other => panic!("Expected ExecutionSnapshot, got {:?}", other),
        }
    }

    #[test]
    fn test_order_response_success() {
        let raw = r#"{
            "method": "add_order",
            "req_id": 42,
            "success": true,
            "result": {
                "order_id": "ORD-NEW-001",
                "cl_ord_id": "mm_buy_camp_002"
            }
        }"#;
        let msg = parse_ws_message(raw);
        match msg {
            WsMessage::OrderResponse { req_id, success, method, order_id, cl_ord_id, error } => {
                assert_eq!(req_id, 42);
                assert!(success);
                assert_eq!(method, "add_order");
                assert_eq!(order_id, Some("ORD-NEW-001".to_string()));
                assert_eq!(cl_ord_id, Some("mm_buy_camp_002".to_string()));
                assert!(error.is_none());
            }
            other => panic!("Expected OrderResponse, got {:?}", other),
        }
    }

    #[test]
    fn test_order_response_error() {
        let raw = r#"{
            "method": "add_order",
            "req_id": 99,
            "success": false,
            "error": "EOrder:Insufficient funds",
            "result": {}
        }"#;
        let msg = parse_ws_message(raw);
        match msg {
            WsMessage::OrderResponse { success, error, .. } => {
                assert!(!success);
                assert_eq!(error, Some("EOrder:Insufficient funds".to_string()));
            }
            other => panic!("Expected OrderResponse, got {:?}", other),
        }
    }

    #[test]
    fn test_subscribe_confirmation() {
        let raw = r#"{
            "method": "subscribe",
            "result": {"channel": "book", "depth": 10, "symbol": "CAMP/USD"},
            "success": true
        }"#;
        let msg = parse_ws_message(raw);
        match msg {
            WsMessage::SubscribeConfirmed { channel } => assert_eq!(channel, "book"),
            other => panic!("Expected SubscribeConfirmed, got {:?}", other),
        }
    }

    #[test]
    fn test_heartbeat_parsing() {
        let raw = r#"{"channel":"heartbeat"}"#;
        assert!(matches!(parse_ws_message(raw), WsMessage::Heartbeat));
    }

    #[test]
    fn test_pong_parsing() {
        let raw = r#"{"method":"pong","req_id":1}"#;
        assert!(matches!(parse_ws_message(raw), WsMessage::Pong));
    }

    #[test]
    fn test_unknown_message() {
        let raw = r#"{"channel":"status","data":[]}"#;
        assert!(matches!(parse_ws_message(raw), WsMessage::Unknown(_)));
    }

    #[test]
    fn test_invalid_json() {
        let msg = parse_ws_message("not json");
        assert!(matches!(msg, WsMessage::Unknown(_)));
    }

    #[test]
    fn test_exchange_capabilities_serde() {
        let caps = ExchangeCapabilities { dead_man_switch: false };
        let json = serde_json::to_string(&caps).unwrap();
        let parsed: ExchangeCapabilities = serde_json::from_str(&json).unwrap();
        assert!(!parsed.dead_man_switch);

        // Default has DMS enabled
        let default_caps = ExchangeCapabilities::default();
        assert!(default_caps.dead_man_switch);
    }

    // Builder function tests — ensure builders produce parseable messages.

    #[test]
    fn test_build_and_parse_book_snapshot() {
        let msg = build_book_message(
            "snapshot",
            "BTC/USD",
            vec![build_book_level(50000.0, 1.5)],
            vec![build_book_level(50001.0, 0.8)],
        );
        match parse_ws_message(&msg) {
            WsMessage::BookSnapshot { symbol, bids, asks } => {
                assert_eq!(symbol, "BTC/USD");
                assert_eq!(bids.len(), 1);
                assert_eq!(asks.len(), 1);
            }
            other => panic!("Expected BookSnapshot, got {:?}", other),
        }
    }

    #[test]
    fn test_build_and_parse_execution() {
        let report = build_exec_report(
            "trade", "ORD-1", "cl-1", "ETH/USD", "buy",
            1.0, 3000.0, 0.69, "filled", "m",
            "2026-02-15T12:00:00.000Z", None,
        );
        let msg = build_execution_update(report);
        match parse_ws_message(&msg) {
            WsMessage::Execution(r) => {
                assert_eq!(r.exec_type, "trade");
                assert_eq!(r.cl_ord_id, "cl-1");
                assert!(r.is_maker);
            }
            other => panic!("Expected Execution, got {:?}", other),
        }
    }

    #[test]
    fn test_build_and_parse_exec_with_cancel_reason() {
        let report = build_exec_report(
            "canceled", "ORD-2", "cl-2", "BTC/USD", "sell",
            0.0, 0.0, 0.0, "canceled", "m",
            "2026-02-15T12:00:00.000Z", Some("Market price protection"),
        );
        let msg = build_execution_update(report);
        match parse_ws_message(&msg) {
            WsMessage::Execution(r) => {
                assert_eq!(r.cancel_reason, Some("Market price protection".to_string()));
            }
            other => panic!("Expected Execution, got {:?}", other),
        }
    }

    #[test]
    fn test_build_and_parse_order_response() {
        let msg = build_order_response_success("add_order", 42, "ORD-1", "cl-1");
        match parse_ws_message(&msg) {
            WsMessage::OrderResponse { req_id, success, method, order_id, cl_ord_id, .. } => {
                assert_eq!(req_id, 42);
                assert!(success);
                assert_eq!(method, "add_order");
                assert_eq!(order_id, Some("ORD-1".to_string()));
                assert_eq!(cl_ord_id, Some("cl-1".to_string()));
            }
            other => panic!("Expected OrderResponse, got {:?}", other),
        }
    }

    #[test]
    fn test_build_and_parse_subscribe_confirmed() {
        let msg = build_subscribe_confirmed("book");
        match parse_ws_message(&msg) {
            WsMessage::SubscribeConfirmed { channel } => assert_eq!(channel, "book"),
            other => panic!("Expected SubscribeConfirmed, got {:?}", other),
        }
    }

    #[test]
    fn test_build_and_parse_heartbeat() {
        let msg = build_heartbeat();
        assert!(matches!(parse_ws_message(&msg), WsMessage::Heartbeat));
    }

    #[test]
    fn test_build_and_parse_pong() {
        let msg = build_pong(7);
        match parse_ws_message(&msg) {
            WsMessage::Pong => {}
            other => panic!("Expected Pong, got {:?}", other),
        }
    }
}
