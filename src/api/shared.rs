use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

use crate::config::Config;
use crate::state::bot_state::BotState;
use crate::types::EngineEvent;

pub struct SharedState {
    pub bot_state: Arc<RwLock<BotState>>,
    pub config: Arc<Config>,
    pub event_tx: mpsc::Sender<EngineEvent>,
    pub trade_log_path: String,
}
