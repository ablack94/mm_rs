use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::symbol::Ticker;

/// Static info about a trading pair, fetched from the exchange.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairInfo {
    #[serde(alias = "symbol")]
    pub pair: Ticker,
    /// Exchange-specific identifier for the pair (e.g. Kraken REST key, Coinbase product_id).
    pub rest_key: String,
    pub min_order_qty: Decimal,
    pub min_cost: Decimal,
    pub price_decimals: u32,
    pub qty_decimals: u32,
    pub maker_fee_pct: Decimal,
    #[serde(alias = "base_asset")]
    pub exchange_base_asset: String,
}
