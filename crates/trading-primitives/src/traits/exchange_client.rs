use anyhow::Result;
use async_trait::async_trait;
use rust_decimal::Decimal;
use std::collections::HashMap;

use crate::pair::PairInfo;
use crate::symbol::Ticker;
use crate::ticker::TickerData;

#[async_trait]
pub trait ExchangeClient: Send + Sync {
    /// Get a WebSocket authentication token.
    async fn get_ws_token(&self) -> Result<String>;

    /// Fetch pair info for the given target pairs.
    async fn get_pair_info(&self, pairs: &[Ticker]) -> Result<HashMap<Ticker, PairInfo>>;

    /// Fetch 24h ticker data for all pairs.
    async fn get_tickers(&self, pair_info: &HashMap<Ticker, PairInfo>) -> Result<HashMap<Ticker, TickerData>>;

    /// Fetch account balances (asset code -> balance).
    async fn get_balances(&self) -> Result<HashMap<String, Decimal>>;
}
