use rust_decimal::Decimal;
use serde::{Serialize, Serializer};

// Re-export protocol types and parser from trading-primitives.
pub use trading_primitives::protocol::{
    WsMessage, ExecReport, ExchangeCapabilities, parse_ws_message,
    // Builder functions (used by proxies, re-exported for convenience).
    build_book_message, build_book_level,
    build_execution_update, build_execution_snapshot, build_exec_report,
    build_subscribe_confirmed, build_order_response_success, build_order_response_error,
    build_pong, build_heartbeat,
};

// ---------------------------------------------------------------------------
// Outgoing message structs (bot → proxy, only used in live.rs)
// ---------------------------------------------------------------------------

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _priority: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _priority: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _priority: Option<String>,
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

// Tests are now in trading_primitives::protocol::tests.
// The tests below validate that re-exports work correctly.
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
