use anyhow::Result;
use async_trait::async_trait;
use rust_decimal::Decimal;
use crate::types::OrderRequest;

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
