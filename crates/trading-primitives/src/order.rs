use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::symbol::Ticker;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderSide {
    Buy,
    Sell,
}

impl std::fmt::Display for OrderSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderSide::Buy => write!(f, "buy"),
            OrderSide::Sell => write!(f, "sell"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRequest {
    pub cl_ord_id: String,
    #[serde(alias = "symbol")]
    pub pair: Ticker,
    pub side: OrderSide,
    pub price: Decimal,
    pub qty: Decimal,
    #[serde(default = "default_true")]
    pub post_only: bool,
    /// If true, place as market order (immediate fill, ignore price).
    #[serde(default)]
    pub market: bool,
    /// If true, this order bypasses the proxy's rate limiter (for liquidations).
    #[serde(default)]
    pub urgent: bool,
}

fn default_true() -> bool {
    true
}
