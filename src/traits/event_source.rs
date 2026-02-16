use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;
use crate::types::EngineEvent;

/// Produces EngineEvents into a channel.
/// Live implementation connects to Kraken WS.
/// Replay implementation reads from a recorded file.
#[async_trait]
pub trait EventSource: Send {
    async fn run(&mut self, tx: mpsc::Sender<EngineEvent>) -> Result<()>;
}
