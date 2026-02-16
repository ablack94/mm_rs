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

    // Skip execution snapshots — they replay old fills that would double-count
    // positions already loaded from state.json. We only want live updates.
    if msg_type == "snapshot" {
        return WsMessage::Unknown("exec_snapshot".to_string());
    }

    let data = match v["data"].as_array().and_then(|a| a.first()) {
        Some(d) => d,
        None => return WsMessage::Unknown(v.to_string()),
    };

    let fee = data["fees"]
        .as_array()
        .and_then(|fees| fees.iter().map(|f| f["qty"].as_f64().unwrap_or(0.0)).reduce(|a, b| a + b))
        .and_then(|f| Decimal::try_from(f).ok())
        .unwrap_or_default();

    let ts = data["timestamp"]
        .as_str()
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        .unwrap_or_else(Utc::now);

    WsMessage::Execution(ExecReport {
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
