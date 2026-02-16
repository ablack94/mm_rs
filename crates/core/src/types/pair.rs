use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Static info about a trading pair, fetched from the exchange.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairInfo {
    pub symbol: String,
    pub rest_key: String,
    pub min_order_qty: Decimal,
    pub min_cost: Decimal,
    pub price_decimals: u32,
    pub qty_decimals: u32,
    pub maker_fee_pct: Decimal,
    pub base_asset: String,
}
