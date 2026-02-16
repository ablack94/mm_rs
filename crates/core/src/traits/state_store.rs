use anyhow::Result;
use crate::state::bot_state::BotState;

/// Persistence for bot state (positions, orders, P&L).
pub trait StateStore: Send + Sync {
    fn save(&self, state: &BotState) -> Result<()>;
    fn load(&self) -> Result<Option<BotState>>;
}
