use anyhow::Result;
use crate::types::TradeRecord;

/// Appends trade records to a log (CSV, database, etc).
pub trait TradeLogger: Send + Sync {
    fn log_trade(&mut self, record: &TradeRecord) -> Result<()>;
}
