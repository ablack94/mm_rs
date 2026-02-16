use anyhow::{bail, Result};
use chrono::Utc;
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "scanner", about = "Scan Kraken for viable low-liquidity MM pairs")]
struct Args {
    /// Minimum spread % to include
    #[arg(long, default_value_t = 2.0)]
    min_spread: f64,

    /// Maximum spread % to include
    #[arg(long, default_value_t = 30.0)]
    max_spread: f64,

    /// Minimum 24h USD volume
    #[arg(long, default_value_t = 0.0)]
    min_volume: f64,

    /// Limit output to top N pairs (0 = all)
    #[arg(long, default_value_t = 0)]
    max_results: usize,

    /// Output file path
    #[arg(long, default_value = "scanned_pairs.json")]
    output: PathBuf,

    /// Output JSON to stdout instead of a table
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScannedPair {
    symbol: String,
    rest_key: String,
    spread_pct: f64,
    bid: f64,
    ask: f64,
    mid: f64,
    volume_24h_usd: f64,
    trades_24h: u64,
    ordermin: String,
    costmin: String,
    pair_decimals: u32,
    lot_decimals: u32,
    maker_fee_pct: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ScanResult {
    pub scanned_at: String,
    pub total_pairs: usize,
    pub pairs: Vec<ScannedPair>,
    pub symbols: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .init();

    let args = Args::parse();

    let client = reqwest::Client::new();
    let base = "https://api.kraken.com";

    // 1. Fetch all asset pairs
    tracing::info!("Fetching all asset pairs...");
    let pairs_resp: serde_json::Value = client
        .get(format!("{base}/0/public/AssetPairs"))
        .send()
        .await?
        .json()
        .await?;

    check_error(&pairs_resp)?;
    let all_pairs = pairs_resp["result"]
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Invalid AssetPairs response"))?;

    // Filter to active USD-quoted pairs
    let mut usd_pairs: HashMap<String, &serde_json::Value> = HashMap::new();
    for (key, info) in all_pairs {
        let wsname = info["wsname"].as_str().unwrap_or("");
        let status = info["status"].as_str().unwrap_or("online");
        let quote = info["quote"].as_str().unwrap_or("");

        if status != "online" {
            continue;
        }
        if !wsname.ends_with("/USD") && quote != "USD" && quote != "ZUSD" {
            continue;
        }
        if wsname.is_empty() {
            continue;
        }

        usd_pairs.insert(key.clone(), info);
    }

    tracing::info!(count = usd_pairs.len(), "Active USD pairs found");

    // 2. Fetch tickers in batches
    tracing::info!("Fetching ticker data...");
    let keys: Vec<&String> = usd_pairs.keys().collect();
    let mut all_tickers: HashMap<String, serde_json::Value> = HashMap::new();

    for chunk in keys.chunks(50) {
        let pair_csv: String = chunk.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(",");
        let ticker_resp: serde_json::Value = client
            .get(format!("{base}/0/public/Ticker"))
            .query(&[("pair", &pair_csv)])
            .send()
            .await?
            .json()
            .await?;

        check_error(&ticker_resp)?;
        if let Some(obj) = ticker_resp["result"].as_object() {
            for (k, v) in obj {
                all_tickers.insert(k.clone(), v.clone());
            }
        }

        if chunk.len() == 50 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    tracing::info!(tickers = all_tickers.len(), "Ticker data fetched");

    // 3. Analyze spreads
    let mut results: Vec<ScannedPair> = Vec::new();

    for (key, info) in &usd_pairs {
        let wsname = info["wsname"].as_str().unwrap_or("").to_string();
        let ticker = match all_tickers.get(key) {
            Some(t) => t,
            None => continue,
        };

        let bid = parse_ticker_f64(ticker, "b");
        let ask = parse_ticker_f64(ticker, "a");
        let vol_base = parse_ticker_vol(ticker);
        let trades_24h = ticker["t"]
            .as_array()
            .and_then(|a| a.get(1))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        if bid <= 0.0 || ask <= 0.0 || bid >= ask {
            continue;
        }

        let mid = (bid + ask) / 2.0;
        let spread_pct = (ask - bid) / mid * 100.0;
        let volume_usd = vol_base * mid;

        if spread_pct < args.min_spread || spread_pct > args.max_spread {
            continue;
        }
        if volume_usd < args.min_volume {
            continue;
        }

        let maker_fee_pct = info["fees_maker"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|f| f.get(1))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.26);

        results.push(ScannedPair {
            symbol: wsname,
            rest_key: key.clone(),
            spread_pct: round2(spread_pct),
            bid,
            ask,
            mid,
            volume_24h_usd: round2(volume_usd),
            trades_24h,
            ordermin: info["ordermin"].as_str().unwrap_or("0").to_string(),
            costmin: info["costmin"].as_str().unwrap_or("0").to_string(),
            pair_decimals: info["pair_decimals"].as_u64().unwrap_or(8) as u32,
            lot_decimals: info["lot_decimals"].as_u64().unwrap_or(8) as u32,
            maker_fee_pct,
        });
    }

    // Sort by spread descending
    results.sort_by(|a, b| b.spread_pct.partial_cmp(&a.spread_pct).unwrap());

    if args.max_results > 0 && results.len() > args.max_results {
        results.truncate(args.max_results);
    }

    // 4. Output
    if args.json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        print_report(&results);
    }

    // 5. Save to file
    let scan_result = ScanResult {
        scanned_at: Utc::now().to_rfc3339(),
        total_pairs: results.len(),
        symbols: results.iter().map(|p| p.symbol.clone()).collect(),
        pairs: results,
    };

    if let Some(parent) = args.output.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = args.output.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(&scan_result)?)?;
    fs::rename(&tmp, &args.output)?;
    tracing::info!(
        file = %args.output.display(),
        pairs = scan_result.total_pairs,
        "Scan results saved"
    );

    Ok(())
}

fn check_error(resp: &serde_json::Value) -> Result<()> {
    if let Some(errors) = resp["error"].as_array() {
        if !errors.is_empty() {
            bail!("Kraken API error: {:?}", errors);
        }
    }
    Ok(())
}

fn parse_ticker_f64(ticker: &serde_json::Value, field: &str) -> f64 {
    ticker[field]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
}

fn parse_ticker_vol(ticker: &serde_json::Value) -> f64 {
    // v[1] is 24h volume
    ticker["v"]
        .as_array()
        .and_then(|a| a.get(1))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn print_report(results: &[ScannedPair]) {
    println!(
        "\n{}",
        "=".repeat(100)
    );
    println!(
        "KRAKEN LOW-LIQUIDITY PAIR SCAN — {}",
        Utc::now().format("%Y-%m-%d %H:%M UTC")
    );
    println!("{}", "=".repeat(100));
    println!(
        "{:<14} {:>8} {:>14} {:>14} {:>12} {:>7} {:>10} {:>7}",
        "Pair", "Spread%", "Bid", "Ask", "Vol24h$", "Trades", "MinOrd", "MkrFee"
    );
    println!(
        "{} {} {} {} {} {} {} {}",
        "-".repeat(14),
        "-".repeat(8),
        "-".repeat(14),
        "-".repeat(14),
        "-".repeat(12),
        "-".repeat(7),
        "-".repeat(10),
        "-".repeat(7)
    );

    for p in results {
        println!(
            "{:<14} {:>7.2}% {:>14.7} {:>14.7} {:>12.0} {:>7} {:>10} {:>6.2}%",
            p.symbol,
            p.spread_pct,
            p.bid,
            p.ask,
            p.volume_24h_usd,
            p.trades_24h,
            p.ordermin,
            p.maker_fee_pct,
        );
    }

    println!("\nTotal pairs found: {}", results.len());

    let mut buckets = [0u32; 4]; // 2-5%, 5-10%, 10-20%, 20%+
    for p in results {
        match p.spread_pct {
            s if s >= 20.0 => buckets[3] += 1,
            s if s >= 10.0 => buckets[2] += 1,
            s if s >= 5.0 => buckets[1] += 1,
            _ => buckets[0] += 1,
        }
    }
    println!(
        "Spread buckets: 2-5%: {}, 5-10%: {}, 10-20%: {}, 20%+: {}",
        buckets[0], buckets[1], buckets[2], buckets[3]
    );
}
