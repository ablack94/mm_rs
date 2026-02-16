use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use crate::types::StoreData;

// ---------------------------------------------------------------------------
// Storage backend trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait Store: Send + Sync {
    async fn load(&self) -> Result<StoreData>;
    async fn save(&self, data: &StoreData) -> Result<()>;
}

// ---------------------------------------------------------------------------
// JSON file backend (atomic write via temp file + rename)
// ---------------------------------------------------------------------------

pub struct JsonFileStore {
    path: PathBuf,
}

impl JsonFileStore {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }
}

#[async_trait]
impl Store for JsonFileStore {
    async fn load(&self) -> Result<StoreData> {
        if !self.path.exists() {
            info!(path = %self.path.display(), "state file does not exist, using defaults");
            return Ok(StoreData::default());
        }

        let contents = tokio::fs::read_to_string(&self.path).await?;
        if contents.trim().is_empty() {
            warn!(path = %self.path.display(), "state file is empty, using defaults");
            return Ok(StoreData::default());
        }

        let data: StoreData = serde_json::from_str(&contents)?;
        info!(
            path = %self.path.display(),
            pairs = data.pairs.len(),
            "loaded state from file"
        );
        Ok(data)
    }

    async fn save(&self, data: &StoreData) -> Result<()> {
        let json = serde_json::to_string_pretty(data)?;

        // Atomic write: write to temp file in the same directory, then rename.
        // This prevents partial writes if the process crashes mid-write.
        let dir = self.path.parent().unwrap_or(Path::new("."));
        let tmp_path = dir.join(format!(
            ".state_store_{}.tmp",
            std::process::id()
        ));

        tokio::fs::write(&tmp_path, &json).await?;
        tokio::fs::rename(&tmp_path, &self.path).await?;

        tracing::debug!(path = %self.path.display(), "persisted state to file");
        Ok(())
    }
}
