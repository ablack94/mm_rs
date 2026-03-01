//! Exchange abstraction traits.
//!
//! These traits define the interface between the bot and exchange adapters.

use anyhow::Result;
use async_trait::async_trait;
use rust_decimal::Decimal;
use std::collections::HashMap;

use trading_primitives::order::OrderRequest;
use trading_primitives::pair::PairInfo;
use trading_primitives::symbol::Ticker;
use trading_primitives::ticker::TickerData;

/// REST client for exchange queries (balances, pair info, tickers, WS token).
#[async_trait]
pub trait ExchangeClient: Send + Sync {
    /// Get a WebSocket authentication token.
    async fn get_ws_token(&self) -> Result<String>;

    /// Fetch pair info for the given target pairs.
    async fn get_pair_info(&self, pairs: &[Ticker]) -> Result<HashMap<Ticker, PairInfo>>;

    /// Fetch 24h ticker data for all pairs.
    async fn get_tickers(
        &self,
        pair_info: &HashMap<Ticker, PairInfo>,
    ) -> Result<HashMap<Ticker, TickerData>>;

    /// Fetch account balances (asset code -> balance).
    async fn get_balances(&self) -> Result<HashMap<String, Decimal>>;
}

/// Sends orders to the exchange. Thin interface — one concern only.
#[async_trait]
pub trait OrderManager: Send + Sync {
    async fn place_order(&self, request: &OrderRequest) -> Result<()>;
    async fn amend_order(
        &self,
        cl_ord_id: &str,
        new_price: Option<Decimal>,
        new_qty: Option<Decimal>,
    ) -> Result<()>;
    async fn cancel_orders(&self, cl_ord_ids: &[String]) -> Result<()>;
    async fn cancel_all(&self) -> Result<()>;
}

/// Dead man's switch: auto-cancel all orders if the bot stops heartbeating.
#[async_trait]
pub trait DeadManSwitch: Send + Sync {
    async fn refresh(&self, timeout_secs: u64) -> Result<()>;
    async fn disable(&self) -> Result<()>;
}
