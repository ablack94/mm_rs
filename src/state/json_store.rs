use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use crate::state::bot_state::BotState;
use crate::traits::StateStore;

pub struct JsonStateStore {
    path: PathBuf,
}

impl JsonStateStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl StateStore for JsonStateStore {
    fn save(&self, state: &BotState) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let data = serde_json::to_string_pretty(state)?;
        fs::write(&tmp, data)?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    fn load(&self) -> Result<Option<BotState>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let data = fs::read_to_string(&self.path)?;
        let state: BotState = serde_json::from_str(&data)?;
        Ok(Some(state))
    }
}
