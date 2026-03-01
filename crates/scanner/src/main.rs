use anyhow::{bail, Result};
use chrono::Utc;
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "scanner", about = "Scan exchange for viable low-liquidity MM pairs via proxy")]
struct Args {
    /// Proxy URL (e.g., http://localhost:3030)
    #[arg(long, env = "PROXY_URL")]
    proxy_url: String,

    /// Proxy auth token
    #[arg(long, env = "PROXY_TOKEN", default_value = "")]
    proxy_token: String,

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

    /// Fetch order book depth for each pair (slower, one API call per pair)
    #[arg(long)]
    depth: bool,

    /// Order size in USD for effective spread calculation (used with --depth)
    #[arg(long, default_value_t = 500.0)]
    order_size: f64,

    /// Number of order book levels to fetch (used with --depth)
    #[arg(long, default_value_t = 50)]
    depth_levels: u32,

    /// State store URL to push new pairs (e.g., http://localhost:3040)
    #[arg(long)]
    state_store_url: Option<String>,

    /// State store auth token
    #[arg(long, env = "STATE_STORE_TOKEN", default_value = "")]
    state_store_token: String,
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
    /// Depth metrics (only populated when --depth is used)
    #[serde(skip_serializing_if = "Option::is_none")]
    depth: Option<DepthMetrics>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepthMetrics {
    /// Total USD value on the bid side (all fetched levels)
    bid_depth_usd: f64,
    /// Total USD value on the ask side (all fetched levels)
    ask_depth_usd: f64,
    /// Effective spread % if you fill $order_size on each side (VWAP-based)
    effective_spread_pct: f64,
    /// Order size used for effective spread calculation
    order_size_usd: f64,
    /// Whether $order_size can actually be filled on both sides
    fillable: bool,
    /// Number of bid levels
    bid_levels: usize,
    /// Number of ask levels
    ask_levels: usize,
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
    let base = args.proxy_url.trim_end_matches('/');
    let token = &args.proxy_token;

    // 1. Fetch all asset pairs
    tracing::info!("Fetching all asset pairs...");
    let mut req = client.get(format!("{base}/0/public/AssetPairs"));
    if !token.is_empty() {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let pairs_resp: serde_json::Value = req.send().await?.json().await?;

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
        let is_usd = wsname.ends_with("/USD") || quote == "USD" || quote == "ZUSD";
        let is_usdc = wsname.ends_with("/USDC") || quote == "USDC";
        if !is_usd && !is_usdc {
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
        let mut req = client
            .get(format!("{base}/0/public/Ticker"))
            .query(&[("pair", &pair_csv)]);
        if !token.is_empty() {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let ticker_resp: serde_json::Value = req.send().await?.json().await?;

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
            depth: None,
        });
    }

    // Sort by spread descending
    results.sort_by(|a, b| b.spread_pct.partial_cmp(&a.spread_pct).unwrap());

    if args.max_results > 0 && results.len() > args.max_results {
        results.truncate(args.max_results);
    }

    // 4. Fetch depth data if requested
    if args.depth {
        tracing::info!(pairs = results.len(), "Fetching order book depth...");
        for pair in &mut results {
            let mut req = client.get(format!(
                "{base}/0/public/Depth?pair={}&count={}",
                pair.rest_key, args.depth_levels
            ));
            if !token.is_empty() {
                req = req.header("Authorization", format!("Bearer {token}"));
            }

            match req.send().await {
                Ok(resp) => match resp.json::<serde_json::Value>().await {
                    Ok(data) => {
                        if let Some(errors) = data["error"].as_array() {
                            if !errors.is_empty() {
                                tracing::warn!(pair = %pair.rest_key, ?errors, "Depth API error");
                                continue;
                            }
                        }
                        if let Some(book) = data["result"].as_object().and_then(|r| r.values().next()) {
                            pair.depth = Some(compute_depth_metrics(book, args.order_size));
                        }
                    }
                    Err(e) => tracing::warn!(pair = %pair.rest_key, error = %e, "Failed to parse depth"),
                },
                Err(e) => tracing::warn!(pair = %pair.rest_key, error = %e, "Failed to fetch depth"),
            }

            // Rate limit: small delay between requests
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        tracing::info!("Depth analysis complete");
    }

    // 5. Output
    if args.json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        print_report(&results);
    }

    // 6. Save to file
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

    // Push new pairs to state store if configured
    if let Some(ref ss_url) = args.state_store_url {
        push_to_state_store(&client, ss_url, &args.state_store_token, &scan_result.symbols).await?;
    }

    Ok(())
}

fn check_error(resp: &serde_json::Value) -> Result<()> {
    if let Some(errors) = resp["error"].as_array() {
        if !errors.is_empty() {
            bail!("API error: {:?}", errors);
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

/// Push newly discovered pairs to the state store.
/// Only creates pairs that don't already exist (never overrides existing pairs).
async fn push_to_state_store(
    client: &reqwest::Client,
    ss_url: &str,
    token: &str,
    symbols: &[String],
) -> Result<()> {
    let base = ss_url.trim_end_matches('/');

    // Fetch existing pairs from state store
    let mut req = client.get(format!("{base}/pairs"));
    if !token.is_empty() {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let resp: serde_json::Value = req.send().await?.json().await?;
    let existing: std::collections::HashSet<String> = resp["pairs"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|p| p["symbol"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let new_pairs: Vec<&String> = symbols.iter().filter(|s| !existing.contains(*s)).collect();

    if new_pairs.is_empty() {
        tracing::info!("All scanned pairs already in state store — nothing to add");
        return Ok(());
    }

    tracing::info!(count = new_pairs.len(), "Pushing new pairs to state store");

    let mut added = 0u32;
    for symbol in &new_pairs {
        // URL-encode the symbol (e.g., OMG/USD → OMG%2FUSD)
        let encoded = symbol.replace('/', "%2F");
        let body = serde_json::json!({
            "state": "active",
            "config": {}
        });

        let mut req = client.put(format!("{base}/pairs/{encoded}")).json(&body);
        if !token.is_empty() {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                added += 1;
                tracing::info!(symbol, status = %resp.status(), "Added pair to state store");
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                tracing::warn!(symbol, %status, body, "Failed to add pair to state store");
            }
            Err(e) => {
                tracing::error!(symbol, error = %e, "Request failed for pair");
            }
        }
    }

    tracing::info!(added, total_new = new_pairs.len(), "State store push complete");
    Ok(())
}

/// Compute VWAP (volume-weighted average price) for filling $target_usd on one side of the book.
/// `levels` is an array of [price, size, timestamp] in best-to-worst order.
/// Returns (vwap, filled_usd, fillable).
fn compute_vwap(levels: &[serde_json::Value], target_usd: f64) -> (f64, f64, bool) {
    let mut filled_usd = 0.0;
    let mut filled_qty = 0.0;

    for level in levels {
        let arr = match level.as_array() {
            Some(a) => a,
            None => continue,
        };
        let price = parse_level_f64(arr.first().unwrap_or(&serde_json::Value::Null));
        let size = parse_level_f64(arr.get(1).unwrap_or(&serde_json::Value::Null));

        if price <= 0.0 || size <= 0.0 {
            continue;
        }

        let level_usd = price * size;
        let remaining = target_usd - filled_usd;

        if level_usd >= remaining {
            // Partial fill on this level
            let partial_qty = remaining / price;
            filled_qty += partial_qty;
            filled_usd += remaining;
            break;
        } else {
            filled_qty += size;
            filled_usd += level_usd;
        }
    }

    let vwap = if filled_qty > 0.0 {
        filled_usd / filled_qty
    } else {
        0.0
    };
    (vwap, filled_usd, filled_usd >= target_usd * 0.999)
}

/// Parse a value that might be a JSON string or number into f64
fn parse_level_f64(v: &serde_json::Value) -> f64 {
    v.as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| v.as_f64())
        .unwrap_or(0.0)
}

fn compute_depth_metrics(book: &serde_json::Value, order_size_usd: f64) -> DepthMetrics {
    let bids = book["bids"].as_array();
    let asks = book["asks"].as_array();

    let bid_levels = bids.map(|b| b.len()).unwrap_or(0);
    let ask_levels = asks.map(|a| a.len()).unwrap_or(0);

    // Total depth in USD on each side
    let bid_depth_usd: f64 = bids
        .map(|levels| {
            levels.iter().map(|l| {
                let arr = match l.as_array() { Some(a) => a, None => return 0.0 };
                let price = parse_level_f64(arr.first().unwrap_or(&serde_json::Value::Null));
                let size = parse_level_f64(arr.get(1).unwrap_or(&serde_json::Value::Null));
                price * size
            }).sum()
        })
        .unwrap_or(0.0);

    let ask_depth_usd: f64 = asks
        .map(|levels| {
            levels.iter().map(|l| {
                let arr = match l.as_array() { Some(a) => a, None => return 0.0 };
                let price = parse_level_f64(arr.first().unwrap_or(&serde_json::Value::Null));
                let size = parse_level_f64(arr.get(1).unwrap_or(&serde_json::Value::Null));
                price * size
            }).sum()
        })
        .unwrap_or(0.0);

    // Compute effective spread at order_size_usd
    let empty = vec![];
    let (bid_vwap, _, bid_fillable) = compute_vwap(bids.unwrap_or(&empty), order_size_usd);
    let (ask_vwap, _, ask_fillable) = compute_vwap(asks.unwrap_or(&empty), order_size_usd);

    let fillable = bid_fillable && ask_fillable;
    let effective_spread_pct = if bid_vwap > 0.0 && ask_vwap > 0.0 {
        let mid = (bid_vwap + ask_vwap) / 2.0;
        (ask_vwap - bid_vwap) / mid * 100.0
    } else {
        0.0
    };

    DepthMetrics {
        bid_depth_usd: round2(bid_depth_usd),
        ask_depth_usd: round2(ask_depth_usd),
        effective_spread_pct: round2(effective_spread_pct),
        order_size_usd,
        fillable,
        bid_levels,
        ask_levels,
    }
}

fn print_report(results: &[ScannedPair]) {
    println!(
        "\n{}",
        "=".repeat(100)
    );
    println!(
        "LOW-LIQUIDITY PAIR SCAN — {}",
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

    // Depth analysis table
    let has_depth = results.iter().any(|p| p.depth.is_some());
    if has_depth {
        let order_size = results.iter()
            .find_map(|p| p.depth.as_ref().map(|d| d.order_size_usd))
            .unwrap_or(500.0);

        println!(
            "\n{}",
            "=".repeat(100)
        );
        println!("ORDER BOOK DEPTH ANALYSIS (order size: ${:.0})", order_size);
        println!("{}", "=".repeat(100));
        println!(
            "{:<14} {:>8} {:>10} {:>10} {:>10} {:>8} {:>8} {:>8}",
            "Pair", "Spread%", "EffSprd%", "BidDepth$", "AskDepth$", "Fill?", "BidLvl", "AskLvl"
        );
        println!(
            "{} {} {} {} {} {} {} {}",
            "-".repeat(14),
            "-".repeat(8),
            "-".repeat(10),
            "-".repeat(10),
            "-".repeat(10),
            "-".repeat(8),
            "-".repeat(8),
            "-".repeat(8),
        );

        for p in results {
            if let Some(d) = &p.depth {
                println!(
                    "{:<14} {:>7.2}% {:>9.2}% {:>10.0} {:>10.0} {:>8} {:>8} {:>8}",
                    p.symbol,
                    p.spread_pct,
                    d.effective_spread_pct,
                    d.bid_depth_usd,
                    d.ask_depth_usd,
                    if d.fillable { "YES" } else { "NO" },
                    d.bid_levels,
                    d.ask_levels,
                );
            }
        }
    }
}
