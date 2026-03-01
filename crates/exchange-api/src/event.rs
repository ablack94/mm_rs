//! Proxy events: proxy → bot.
//!
//! Flat JSON tagged by `"event"`. Separate event types instead of one overloaded ExecReport.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// A price level in book snapshot/update messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookLevel {
    pub price: Decimal,
    pub qty: Decimal,
}

/// Events sent from proxy to bot over WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ProxyEvent {
    BookSnapshot {
        symbol: String,
        bids: Vec<BookLevel>,
        asks: Vec<BookLevel>,
    },
    BookUpdate {
        symbol: String,
        bids: Vec<BookLevel>,
        asks: Vec<BookLevel>,
    },
    Fill {
        order_id: String,
        cl_ord_id: String,
        symbol: String,
        side: String,
        qty: Decimal,
        price: Decimal,
        fee: Decimal,
        is_maker: bool,
        timestamp: String,
        /// Whether this fill fully filled the order.
        #[serde(default)]
        is_fully_filled: bool,
    },
    OrderAccepted {
        #[serde(default)]
        req_id: u64,
        cl_ord_id: String,
        order_id: String,
    },
    OrderCancelled {
        cl_ord_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Symbol for the cancelled order (needed by engine for pair routing).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        symbol: Option<String>,
    },
    OrderRejected {
        #[serde(default)]
        req_id: u64,
        cl_ord_id: String,
        error: String,
        /// Symbol for the rejected order (needed by engine for pair routing).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        symbol: Option<String>,
    },
    CommandAck {
        req_id: u64,
        cmd: String,
    },
    Subscribed {
        channel: String,
    },
    Heartbeat,
    Pong {
        req_id: u64,
    },
}

impl ProxyEvent {
    /// Serialize this event to a JSON string.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("ProxyEvent serialization should never fail")
    }
}

// ---------------------------------------------------------------------------
// Builder helpers for proxies constructing events
// ---------------------------------------------------------------------------

impl ProxyEvent {
    pub fn book_snapshot(symbol: &str, bids: Vec<BookLevel>, asks: Vec<BookLevel>) -> Self {
        Self::BookSnapshot {
            symbol: symbol.to_string(),
            bids,
            asks,
        }
    }

    pub fn book_update(symbol: &str, bids: Vec<BookLevel>, asks: Vec<BookLevel>) -> Self {
        Self::BookUpdate {
            symbol: symbol.to_string(),
            bids,
            asks,
        }
    }

    pub fn fill(
        order_id: &str,
        cl_ord_id: &str,
        symbol: &str,
        side: &str,
        qty: Decimal,
        price: Decimal,
        fee: Decimal,
        is_maker: bool,
        timestamp: &str,
        is_fully_filled: bool,
    ) -> Self {
        Self::Fill {
            order_id: order_id.to_string(),
            cl_ord_id: cl_ord_id.to_string(),
            symbol: symbol.to_string(),
            side: side.to_string(),
            qty,
            price,
            fee,
            is_maker,
            timestamp: timestamp.to_string(),
            is_fully_filled,
        }
    }

    pub fn order_accepted(req_id: u64, cl_ord_id: &str, order_id: &str) -> Self {
        Self::OrderAccepted {
            req_id,
            cl_ord_id: cl_ord_id.to_string(),
            order_id: order_id.to_string(),
        }
    }

    pub fn order_cancelled(cl_ord_id: &str, reason: Option<&str>, symbol: Option<&str>) -> Self {
        Self::OrderCancelled {
            cl_ord_id: cl_ord_id.to_string(),
            reason: reason.map(String::from),
            symbol: symbol.map(String::from),
        }
    }

    pub fn order_rejected(req_id: u64, cl_ord_id: &str, error: &str, symbol: Option<&str>) -> Self {
        Self::OrderRejected {
            req_id,
            cl_ord_id: cl_ord_id.to_string(),
            error: error.to_string(),
            symbol: symbol.map(String::from),
        }
    }

    pub fn command_ack(req_id: u64, cmd: &str) -> Self {
        Self::CommandAck {
            req_id,
            cmd: cmd.to_string(),
        }
    }

    pub fn subscribed(channel: &str) -> Self {
        Self::Subscribed {
            channel: channel.to_string(),
        }
    }

    pub fn heartbeat() -> Self {
        Self::Heartbeat
    }

    pub fn pong(req_id: u64) -> Self {
        Self::Pong { req_id }
    }
}

/// Parse a raw JSON string into a ProxyEvent.
pub fn parse_proxy_event(raw: &str) -> Result<ProxyEvent, serde_json::Error> {
    serde_json::from_str(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_book_snapshot_roundtrip() {
        let event = ProxyEvent::book_snapshot(
            "BTC/USD",
            vec![BookLevel { price: dec!(50000.0), qty: dec!(1.5) }],
            vec![BookLevel { price: dec!(50001.0), qty: dec!(0.8) }],
        );
        let json = event.to_json();
        assert!(json.contains("\"event\":\"book_snapshot\""));
        let parsed: ProxyEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            ProxyEvent::BookSnapshot { symbol, bids, asks } => {
                assert_eq!(symbol, "BTC/USD");
                assert_eq!(bids.len(), 1);
                assert_eq!(asks.len(), 1);
                assert_eq!(bids[0].price, dec!(50000.0));
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_fill_roundtrip() {
        let event = ProxyEvent::fill(
            "X1", "bid-1", "BTC/USD", "buy",
            dec!(0.01), dec!(50000.0), dec!(0.115),
            true, "2026-02-15T12:00:00Z", false,
        );
        let json = event.to_json();
        assert!(json.contains("\"event\":\"fill\""));
        let parsed: ProxyEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            ProxyEvent::Fill { cl_ord_id, qty, price, is_maker, .. } => {
                assert_eq!(cl_ord_id, "bid-1");
                assert_eq!(qty, dec!(0.01));
                assert_eq!(price, dec!(50000.0));
                assert!(is_maker);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_order_accepted_roundtrip() {
        let event = ProxyEvent::order_accepted(1, "bid-1", "X1");
        let json = event.to_json();
        assert!(json.contains("\"event\":\"order_accepted\""));
        let parsed: ProxyEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            ProxyEvent::OrderAccepted { cl_ord_id, order_id, .. } => {
                assert_eq!(cl_ord_id, "bid-1");
                assert_eq!(order_id, "X1");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_order_cancelled_roundtrip() {
        let event = ProxyEvent::order_cancelled("bid-1", Some("user_requested"), None);
        let json = event.to_json();
        assert!(json.contains("\"event\":\"order_cancelled\""));
        let parsed: ProxyEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            ProxyEvent::OrderCancelled { cl_ord_id, reason, .. } => {
                assert_eq!(cl_ord_id, "bid-1");
                assert_eq!(reason, Some("user_requested".to_string()));
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_heartbeat_roundtrip() {
        let event = ProxyEvent::heartbeat();
        let json = event.to_json();
        assert!(json.contains("\"event\":\"heartbeat\""));
        let parsed: ProxyEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, ProxyEvent::Heartbeat));
    }

    #[test]
    fn test_pong_roundtrip() {
        let event = ProxyEvent::pong(6);
        let json = event.to_json();
        assert!(json.contains("\"event\":\"pong\""));
        let parsed: ProxyEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            ProxyEvent::Pong { req_id } => assert_eq!(req_id, 6),
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_subscribed_roundtrip() {
        let event = ProxyEvent::subscribed("book");
        let json = event.to_json();
        let parsed: ProxyEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            ProxyEvent::Subscribed { channel } => assert_eq!(channel, "book"),
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_command_ack_roundtrip() {
        let event = ProxyEvent::command_ack(4, "cancel_all");
        let json = event.to_json();
        let parsed: ProxyEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            ProxyEvent::CommandAck { req_id, cmd } => {
                assert_eq!(req_id, 4);
                assert_eq!(cmd, "cancel_all");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_order_rejected_optional_symbol() {
        // Without symbol
        let event = ProxyEvent::order_rejected(1, "bid-1", "Insufficient funds", None);
        let json = event.to_json();
        assert!(!json.contains("\"symbol\""));

        // With symbol
        let event = ProxyEvent::order_rejected(1, "bid-1", "Insufficient funds", Some("BTC/USD"));
        let json = event.to_json();
        assert!(json.contains("\"symbol\":\"BTC/USD\""));
    }
}
