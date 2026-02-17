use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Serialize, Serializer};
use crate::types::*;

/// Serialize Decimal as a JSON float (Kraken rejects string-encoded numbers).
fn decimal_as_f64<S: Serializer>(val: &Decimal, s: S) -> Result<S::Ok, S::Error> {
    use rust_decimal::prelude::ToPrimitive;
    s.serialize_f64(val.to_f64().unwrap_or(0.0))
}

/// Serialize Option<Decimal> as a JSON float when present.
fn option_decimal_as_f64<S: Serializer>(val: &Option<Decimal>, s: S) -> Result<S::Ok, S::Error> {
    use rust_decimal::prelude::ToPrimitive;
    match val {
        Some(d) => s.serialize_f64(d.to_f64().unwrap_or(0.0)),
        None => s.serialize_none(),
    }
}

// --- Outgoing messages (to Kraken) ---

#[derive(Debug, Serialize)]
pub struct SubscribeBookMsg {
    pub method: &'static str,
    pub params: SubscribeBookParams,
}

#[derive(Debug, Serialize)]
pub struct SubscribeBookParams {
    pub channel: &'static str,
    pub symbol: Vec<String>,
    pub depth: u32,
}

#[derive(Debug, Serialize)]
pub struct SubscribeExecMsg {
    pub method: &'static str,
    pub params: SubscribeExecParams,
}

#[derive(Debug, Serialize)]
pub struct SubscribeExecParams {
    pub channel: &'static str,
    pub snap_orders: bool,
    pub snap_trades: bool,
    pub ratecounter: bool,
    pub token: String,
}

#[derive(Debug, Serialize)]
pub struct AddOrderMsg {
    pub method: &'static str,
    pub params: AddOrderParams,
    pub req_id: u64,
}

#[derive(Debug, Serialize)]
pub struct AddOrderParams {
    pub order_type: &'static str,
    pub side: String,
    pub symbol: String,
    #[serde(serialize_with = "decimal_as_f64")]
    pub limit_price: Decimal,
    #[serde(serialize_with = "decimal_as_f64")]
    pub order_qty: Decimal,
    pub post_only: bool,
    pub time_in_force: &'static str,
    pub cl_ord_id: String,
    pub token: String,
}

#[derive(Debug, Serialize)]
pub struct AmendOrderMsg {
    pub method: &'static str,
    pub params: AmendOrderParams,
    pub req_id: u64,
}

#[derive(Debug, Serialize)]
pub struct AmendOrderParams {
    pub cl_ord_id: String,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "option_decimal_as_f64")]
    pub limit_price: Option<Decimal>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "option_decimal_as_f64")]
    pub order_qty: Option<Decimal>,
    pub post_only: bool,
    pub token: String,
}

#[derive(Debug, Serialize)]
pub struct CancelOrderMsg {
    pub method: &'static str,
    pub params: CancelOrderParams,
    pub req_id: u64,
}

#[derive(Debug, Serialize)]
pub struct CancelOrderParams {
    pub cl_ord_id: Vec<String>,
    pub token: String,
}

#[derive(Debug, Serialize)]
pub struct CancelAllMsg {
    pub method: &'static str,
    pub params: CancelAllParams,
    pub req_id: u64,
}

#[derive(Debug, Serialize)]
pub struct CancelAllParams {
    pub token: String,
}

#[derive(Debug, Serialize)]
pub struct DmsMsg {
    pub method: &'static str,
    pub params: DmsParams,
    pub req_id: u64,
}

#[derive(Debug, Serialize)]
pub struct DmsParams {
    pub timeout: u64,
    pub token: String,
}

#[derive(Debug, Serialize)]
pub struct PingMsg {
    pub method: &'static str,
    pub req_id: u64,
}

// --- Incoming message parsing ---

/// Parsed WS message from Kraken.
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
}

/// Parse a raw WS JSON message into our typed enum.
pub fn parse_ws_message(raw: &str) -> WsMessage {
    let v: serde_json::Value = match serde_json::from_str(raw) {
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

fn parse_book(v: &serde_json::Value) -> WsMessage {
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

fn parse_levels(v: &serde_json::Value) -> Vec<LevelUpdate> {
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

fn parse_executions(v: &serde_json::Value) -> WsMessage {
    let msg_type = v["type"].as_str().unwrap_or("");

    if msg_type == "snapshot" {
        // Parse all trades in the snapshot
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

fn parse_single_exec(data: &serde_json::Value) -> Option<ExecReport> {
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
    })
}

fn parse_order_response(v: &serde_json::Value, method: &str) -> WsMessage {
    WsMessage::OrderResponse {
        req_id: v["req_id"].as_u64().unwrap_or(0),
        success: v["success"].as_bool().unwrap_or(false),
        method: method.to_string(),
        order_id: v["result"]["order_id"].as_str().map(String::from),
        cl_ord_id: v["result"]["cl_ord_id"].as_str().map(String::from),
        error: v["error"].as_str().map(String::from),
    }
}

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
                assert_eq!(bids[1].price, dec!(0.0038));
                assert_eq!(bids[1].qty, dec!(25000.0));
                assert_eq!(asks[0].price, dec!(0.0041));
                assert_eq!(asks[0].qty, dec!(50000.0));
                assert_eq!(asks[1].price, dec!(0.0042));
                assert_eq!(asks[1].qty, dec!(30000.0));
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
                assert_eq!(bid_updates[0].price, dec!(97500.0));
                assert_eq!(bid_updates[0].qty, dec!(1.5));
                assert_eq!(ask_updates[0].price, dec!(97600.0));
                assert_eq!(ask_updates[0].qty, dec!(0.8));
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
                assert_eq!(report.order_id, "ORD-ABC-123");
                assert_eq!(report.cl_ord_id, "mm_buy_camp_001");
                assert_eq!(report.symbol, "CAMP/USD");
                assert_eq!(report.side, "buy");
                assert_eq!(report.last_qty, dec!(10000.0));
                assert_eq!(report.last_price, dec!(0.0039));
                assert_eq!(report.fee, dec!(0.09));
                assert_eq!(report.order_status, "filled");
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
                assert_eq!(trades[0].order_id, "ORD-OLD-999");
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
            WsMessage::OrderResponse { req_id, success, method, order_id, cl_ord_id, error } => {
                assert_eq!(req_id, 99);
                assert!(!success);
                assert_eq!(method, "add_order");
                assert!(order_id.is_none());
                assert!(cl_ord_id.is_none());
                assert_eq!(error, Some("EOrder:Insufficient funds".to_string()));
            }
            other => panic!("Expected OrderResponse, got {:?}", other),
        }
    }

    #[test]
    fn test_subscribe_confirmation() {
        let raw = r#"{
            "method": "subscribe",
            "result": {
                "channel": "book",
                "depth": 10,
                "symbol": "CAMP/USD"
            },
            "success": true
        }"#;
        let msg = parse_ws_message(raw);
        match msg {
            WsMessage::SubscribeConfirmed { channel } => {
                assert_eq!(channel, "book");
            }
            other => panic!("Expected SubscribeConfirmed, got {:?}", other),
        }
    }

    #[test]
    fn test_heartbeat_parsing() {
        let raw = r#"{"channel":"heartbeat"}"#;
        let msg = parse_ws_message(raw);
        assert!(matches!(msg, WsMessage::Heartbeat));
    }

    #[test]
    fn test_pong_parsing() {
        let raw = r#"{"method":"pong","req_id":1,"time_in":"2026-02-15T12:00:00.000000Z","time_out":"2026-02-15T12:00:00.000001Z"}"#;
        let msg = parse_ws_message(raw);
        assert!(matches!(msg, WsMessage::Pong));
    }

    #[test]
    fn test_unknown_message() {
        let raw = r#"{"channel":"status","data":[{"api_version":"v2","system":"online"}],"type":"update"}"#;
        let msg = parse_ws_message(raw);
        assert!(matches!(msg, WsMessage::Unknown(_)));
    }

    #[test]
    fn test_invalid_json() {
        let raw = "this is not json at all";
        let msg = parse_ws_message(raw);
        match msg {
            WsMessage::Unknown(s) => assert_eq!(s, "this is not json at all"),
            other => panic!("Expected Unknown, got {:?}", other),
        }
    }
}
