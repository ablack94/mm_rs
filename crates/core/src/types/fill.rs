use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use super::order::OrderSide;

/// An execution fill from the exchange.
#[derive(Debug, Clone)]
pub struct Fill {
    pub order_id: String,
    pub cl_ord_id: String,
    pub symbol: String,
    pub side: OrderSide,
    pub price: Decimal,
    pub qty: Decimal,
    pub fee: Decimal,
    pub is_maker: bool,
    pub is_fully_filled: bool,
    pub timestamp: DateTime<Utc>,
}

/// A completed trade record for CSV logging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeRecord {
    pub timestamp: DateTime<Utc>,
    pub symbol: String,
    pub side: String,
    pub price: Decimal,
    pub qty: Decimal,
    pub value_usd: Decimal,
    pub fee: Decimal,
    pub pnl: Decimal,
    pub cumulative_pnl: Decimal,
}
