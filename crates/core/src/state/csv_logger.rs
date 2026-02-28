use anyhow::Result;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use crate::traits::TradeLogger;
use crate::types::TradeRecord;

pub struct CsvTradeLogger {
    path: PathBuf,
    header_written: bool,
}

impl CsvTradeLogger {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let header_written = path.exists();
        Ok(Self { path, header_written })
    }
}

impl TradeLogger for CsvTradeLogger {
    fn log_trade(&mut self, record: &TradeRecord) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;

        if !self.header_written {
            writeln!(
                file,
                "timestamp,symbol,side,price,qty,value_usd,fee,pnl,cumulative_pnl"
            )?;
            self.header_written = true;
        }

        writeln!(
            file,
            "{},{},{},{},{},{},{},{},{}",
            record.timestamp.format("%Y-%m-%d %H:%M:%S"),
            record.pair,
            record.side,
            record.price,
            record.qty,
            record.value_usd,
            record.fee,
            record.pnl,
            record.cumulative_pnl,
        )?;
        Ok(())
    }
}
