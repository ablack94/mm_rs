use anyhow::Result;
use chrono::Utc;
use clap::Parser;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use tokio::time::{Duration, Instant};

use kraken_core::exchange::ws::*;


#[derive(Parser)]
#[command(name = "recorder", about = "Record Kraken WS data for replay testing")]
struct Args {
    /// Pairs to record (e.g., CAMP/USD SUP/USD)
    #[arg(long, num_args = 1..)]
    pairs: Vec<String>,

    /// Output JSONL file
    #[arg(long, default_value = "recorded_data.jsonl")]
    output: PathBuf,

    /// Recording duration in seconds
    #[arg(long, default_value_t = 300)]
    duration: u64,

    /// Book depth
    #[arg(long, default_value_t = 10)]
    depth: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .init();

    let args = Args::parse();

    if args.pairs.is_empty() {
        anyhow::bail!("Specify at least one pair with --pairs");
    }

    tracing::info!(
        pairs = ?args.pairs,
        duration = args.duration,
        output = %args.output.display(),
        "Starting recorder"
    );

    if let Some(parent) = args.output.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&args.output)?;

    let mut ws = WsConnection::connect("wss://ws.kraken.com/v2").await?;
    tracing::info!("Connected to public WS");

    subscribe_book(&mut ws, &args.pairs, args.depth).await?;
    tracing::info!("Subscribed to book for {} pairs", args.pairs.len());

    let deadline = Instant::now() + Duration::from_secs(args.duration);
    let mut count = 0u64;

    while Instant::now() < deadline {
        let recv = tokio::time::timeout(Duration::from_secs(30), ws.recv()).await;
        match recv {
            Ok(Some(raw)) => {
                let record = serde_json::json!({
                    "timestamp": Utc::now().to_rfc3339(),
                    "raw": raw,
                });
                writeln!(file, "{}", record)?;
                count += 1;
                if count % 100 == 0 {
                    tracing::info!(messages = count, "Recording...");
                }
            }
            Ok(None) => {
                tracing::warn!("WS connection closed");
                break;
            }
            Err(_) => {
                tracing::warn!("WS recv timeout, continuing...");
            }
        }
    }

    ws.close().await;
    tracing::info!(messages = count, "Recording complete: {}", args.output.display());
    Ok(())
}
