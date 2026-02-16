use anyhow::Result;
use async_trait::async_trait;

/// Dead man's switch: auto-cancel all orders if the bot stops heartbeating.
#[async_trait]
pub trait DeadManSwitch: Send + Sync {
    async fn refresh(&self, timeout_secs: u64) -> Result<()>;
    async fn disable(&self) -> Result<()>;
}
