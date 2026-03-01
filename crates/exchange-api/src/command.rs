//! Proxy commands: bot → proxy.
//!
//! Flat JSON tagged by `"cmd"`. No `token` field (proxy manages auth internally).

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Priority for order commands. `Urgent` bypasses rate limiting (used for liquidations).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderPriority {
    Normal,
    Urgent,
}

impl Default for OrderPriority {
    fn default() -> Self {
        Self::Normal
    }
}

/// Commands sent from bot to proxy over WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ProxyCommand {
    PlaceOrder {
        req_id: u64,
        symbol: String,
        side: String,
        order_type: String,
        price: Decimal,
        qty: Decimal,
        cl_ord_id: String,
        #[serde(default)]
        post_only: bool,
        #[serde(default)]
        priority: OrderPriority,
        /// Market orders skip price validation.
        #[serde(default)]
        market: bool,
    },
    AmendOrder {
        req_id: u64,
        cl_ord_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        price: Option<Decimal>,
        #[serde(skip_serializing_if = "Option::is_none")]
        qty: Option<Decimal>,
    },
    CancelOrders {
        req_id: u64,
        cl_ord_ids: Vec<String>,
    },
    CancelAll {
        req_id: u64,
    },
    SetDms {
        req_id: u64,
        timeout_secs: u64,
    },
    Subscribe {
        channel: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        symbols: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        depth: Option<u32>,
    },
    Ping {
        req_id: u64,
    },
}

impl ProxyCommand {
    /// Returns the method name for rate limiting purposes.
    pub fn method(&self) -> &str {
        match self {
            Self::PlaceOrder { .. } => "place_order",
            Self::AmendOrder { .. } => "amend_order",
            Self::CancelOrders { .. } => "cancel_orders",
            Self::CancelAll { .. } => "cancel_all",
            Self::SetDms { .. } => "set_dms",
            Self::Subscribe { .. } => "subscribe",
            Self::Ping { .. } => "ping",
        }
    }

    /// Whether this command is rate-limit exempt.
    pub fn is_rate_limit_exempt(&self) -> bool {
        matches!(
            self,
            Self::CancelAll { .. }
                | Self::SetDms { .. }
                | Self::Subscribe { .. }
                | Self::Ping { .. }
        )
    }

    /// Returns the priority if this is an order command.
    pub fn priority(&self) -> OrderPriority {
        match self {
            Self::PlaceOrder { priority, .. } => *priority,
            _ => OrderPriority::Normal,
        }
    }

    /// Returns the req_id if present.
    pub fn req_id(&self) -> Option<u64> {
        match self {
            Self::PlaceOrder { req_id, .. }
            | Self::AmendOrder { req_id, .. }
            | Self::CancelOrders { req_id, .. }
            | Self::CancelAll { req_id, .. }
            | Self::SetDms { req_id, .. }
            | Self::Ping { req_id, .. } => Some(*req_id),
            Self::Subscribe { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_place_order_roundtrip() {
        let cmd = ProxyCommand::PlaceOrder {
            req_id: 1,
            symbol: "BTC/USD".into(),
            side: "buy".into(),
            order_type: "limit".into(),
            price: dec!(50000.0),
            qty: dec!(0.01),
            cl_ord_id: "bid-1".into(),
            post_only: true,
            priority: OrderPriority::Normal,
            market: false,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"cmd\":\"place_order\""));
        assert!(json.contains("\"symbol\":\"BTC/USD\""));
        let parsed: ProxyCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.method(), "place_order");
    }

    #[test]
    fn test_cancel_orders_roundtrip() {
        let cmd = ProxyCommand::CancelOrders {
            req_id: 3,
            cl_ord_ids: vec!["bid-1".into(), "ask-2".into()],
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"cmd\":\"cancel_orders\""));
        let parsed: ProxyCommand = serde_json::from_str(&json).unwrap();
        match parsed {
            ProxyCommand::CancelOrders { cl_ord_ids, .. } => {
                assert_eq!(cl_ord_ids.len(), 2);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_subscribe_roundtrip() {
        let cmd = ProxyCommand::Subscribe {
            channel: "book".into(),
            symbols: vec!["BTC/USD".into(), "ETH/USD".into()],
            depth: Some(10),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"cmd\":\"subscribe\""));
        let parsed: ProxyCommand = serde_json::from_str(&json).unwrap();
        match parsed {
            ProxyCommand::Subscribe { channel, symbols, .. } => {
                assert_eq!(channel, "book");
                assert_eq!(symbols.len(), 2);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_amend_order_optional_fields() {
        let cmd = ProxyCommand::AmendOrder {
            req_id: 2,
            cl_ord_id: "bid-1".into(),
            price: Some(dec!(50100.0)),
            qty: None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"price\":"));
        assert!(!json.contains("\"qty\":"));
    }

    #[test]
    fn test_priority_default() {
        let json = r#"{"cmd":"place_order","req_id":1,"symbol":"BTC/USD","side":"buy","order_type":"limit","price":50000.0,"qty":0.01,"cl_ord_id":"bid-1"}"#;
        let parsed: ProxyCommand = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.priority(), OrderPriority::Normal);
    }
}
