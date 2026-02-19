use anyhow::Result;
use async_trait::async_trait;
use rust_decimal::Decimal;
use std::collections::HashMap;

use crate::pair::PairInfo;
use crate::ticker::TickerData;

#[async_trait]
pub trait ExchangeClient: Send + Sync {
    /// Get a WebSocket authentication token.
    async fn get_ws_token(&self) -> Result<String>;

    /// Fetch pair info for the given target symbols.
    async fn get_pair_info(&self, symbols: &[String]) -> Result<HashMap<String, PairInfo>>;

    /// Fetch 24h ticker data for all pairs.
    async fn get_tickers(&self, pair_info: &HashMap<String, PairInfo>) -> Result<HashMap<String, TickerData>>;

    /// Fetch account balances (asset code -> balance).
    async fn get_balances(&self) -> Result<HashMap<String, Decimal>>;
}
