use anyhow::{bail, Result};
use chrono::{TimeZone, Utc};
use clap::Parser;
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;

use kraken_core::pnl::tracker::PnlTracker;
use kraken_core::types::fill::Fill;
use kraken_core::types::order::OrderSide;

#[derive(Parser)]
#[command(
    name = "pnl-bootstrap",
    about = "Bootstrap PnL state by fetching trade history from Kraken via proxy"
)]
struct Args {
    /// Proxy base URL
    #[arg(long, env = "PROXY_URL")]
    proxy_url: String,

    /// Proxy bearer token
    #[arg(long, env = "PROXY_TOKEN")]
    proxy_token: String,

    /// Output state file path
    #[arg(long, default_value = "pnl_state.json")]
    output: String,

    /// Only include trades after this Unix timestamp
    #[arg(long)]
    start: Option<i64>,

    /// Only include trades before this Unix timestamp
    #[arg(long)]
    end: Option<i64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()))
        .init();

    let client = reqwest::Client::new();
    let proxy_url = args.proxy_url.trim_end_matches('/');

    // Step 1: Fetch AssetPairs to build rest_name → ws_name mapping
    tracing::info!("Fetching asset pairs for name mapping...");
    let pairs_resp = fetch_public(&client, proxy_url, &args.proxy_token, "/0/public/AssetPairs").await?;
    let rest_to_ws = build_pair_mapping(&pairs_resp)?;
    tracing::info!(pairs = rest_to_ws.len(), "Built pair name mapping");

    // Step 2: Paginate through TradesHistory
    tracing::info!("Fetching trade history...");
    let mut all_trades: Vec<(f64, Fill)> = Vec::new();
    let mut offset = 0u64;

    loop {
        let mut body = format!("ofs={}", offset);
        if let Some(start) = args.start {
            body.push_str(&format!("&start={}", start));
        }
        if let Some(end) = args.end {
            body.push_str(&format!("&end={}", end));
        }

        let resp = fetch_private(
            &client,
            proxy_url,
            &args.proxy_token,
            "/0/private/TradesHistory",
            &body,
        )
        .await?;

        let trades = resp["result"]["trades"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("Invalid TradesHistory response: {:?}", resp))?;

        let count = resp["result"]["count"].as_u64().unwrap_or(0);

        if trades.is_empty() {
            break;
        }

        for (txid, trade) in trades {
            let pair_rest = trade["pair"].as_str().unwrap_or_default();
            let symbol = match rest_to_ws.get(pair_rest) {
                Some(ws) => ws.clone(),
                None => {
                    tracing::warn!(pair = pair_rest, txid, "Unknown pair, skipping");
                    continue;
                }
            };

            let time = trade["time"].as_f64().unwrap_or(0.0);
            let timestamp = Utc
                .timestamp_opt(time as i64, ((time.fract()) * 1_000_000_000.0) as u32)
                .single()
                .unwrap_or_else(Utc::now);

            let side = match trade["type"].as_str().unwrap_or("") {
                "buy" => OrderSide::Buy,
                "sell" => OrderSide::Sell,
                other => {
                    tracing::warn!(side = other, txid, "Unknown side, skipping");
                    continue;
                }
            };

            let price = parse_decimal(trade, "price");
            let qty = parse_decimal(trade, "vol");
            let fee = parse_decimal(trade, "fee");

            let fill = Fill {
                order_id: trade["ordertxid"].as_str().unwrap_or("").to_string(),
                cl_ord_id: txid.clone(),
                symbol,
                side,
                price,
                qty,
                fee,
                is_maker: trade["ordertype"].as_str() == Some("limit"),
                is_fully_filled: true,
                timestamp,
            };

            all_trades.push((time, fill));
        }

        tracing::info!(
            fetched = all_trades.len(),
            total = count,
            offset,
            "Fetching trades..."
        );

        offset += trades.len() as u64;
        if offset >= count {
            break;
        }
    }

    // Step 3: Sort by timestamp (Kraken returns unordered within a page)
    all_trades.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    tracing::info!(
        total = all_trades.len(),
        "All trades fetched, applying to PnL tracker..."
    );

    // Step 4: Apply to PnlTracker
    let mut tracker = PnlTracker::new();
    let mut seeded = 0u64;
    for (_, fill) in &all_trades {
        // If selling a pair we never tracked buying, seed cost basis at the sell price.
        // We don't know the real cost for pre-bootstrap positions, so assume break-even
        // to avoid phantom PnL. Only fees are counted as a loss.
        if matches!(fill.side, OrderSide::Sell) {
            let held = tracker
                .positions
                .get(&fill.symbol)
                .map(|p| p.qty)
                .unwrap_or(Decimal::ZERO);
            if held < fill.qty {
                let shortfall = fill.qty - held;
                let seed = Fill {
                    order_id: String::new(),
                    cl_ord_id: String::new(),
                    symbol: fill.symbol.clone(),
                    side: OrderSide::Buy,
                    price: fill.price,
                    qty: shortfall,
                    fee: Decimal::ZERO,
                    is_maker: false,
                    is_fully_filled: true,
                    timestamp: fill.timestamp,
                };
                tracing::warn!(
                    symbol = fill.symbol,
                    shortfall = %shortfall,
                    price = %fill.price,
                    "Seeding cost basis for pre-bootstrap position (PnL will be 0 for this sell)"
                );
                tracker.apply_fill(&seed);
                seeded += 1;
            }
        }
        tracker.apply_fill(fill);
    }

    if seeded > 0 {
        tracing::info!(seeded, "Seeded cost basis for sells without tracked buys");
    }

    // Step 5: Print summary
    let prices = HashMap::new();
    let summary = tracker.summary(&prices);
    tracing::info!(
        realized_pnl = %summary.realized_pnl,
        total_fees = %summary.total_fees,
        trade_count = summary.trade_count,
        positions = summary.positions.len(),
        "PnL state computed"
    );

    for (sym, pos) in &summary.positions {
        tracing::info!(
            symbol = sym,
            qty = %pos.qty,
            avg_cost = %pos.avg_cost,
            "Open position"
        );
    }

    // Step 6: Save
    let output_path = PathBuf::from(&args.output);
    tracker.save(&output_path)?;
    tracing::info!(path = %output_path.display(), "PnL state saved");

    Ok(())
}

fn parse_decimal(trade: &Value, field: &str) -> Decimal {
    trade[field]
        .as_str()
        .and_then(|s| Decimal::from_str(s).ok())
        .unwrap_or_default()
}

fn build_pair_mapping(resp: &Value) -> Result<HashMap<String, String>> {
    let result = resp["result"]
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Invalid AssetPairs response"))?;

    let mut mapping = HashMap::new();
    for (rest_key, info) in result {
        if let Some(wsname) = info["wsname"].as_str() {
            mapping.insert(rest_key.clone(), wsname.to_string());
            // Also add altname mapping (Kraken sometimes uses altname in TradesHistory)
            if let Some(altname) = info["altname"].as_str() {
                if altname != rest_key {
                    mapping.insert(altname.to_string(), wsname.to_string());
                }
            }
        }
    }
    Ok(mapping)
}

async fn fetch_private(
    client: &reqwest::Client,
    proxy_url: &str,
    token: &str,
    path: &str,
    body: &str,
) -> Result<Value> {
    let resp: Value = client
        .post(format!("{}{}", proxy_url, path))
        .header("Authorization", format!("Bearer {}", token))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body.to_string())
        .send()
        .await?
        .json()
        .await?;

    if let Some(errors) = resp["error"].as_array() {
        if !errors.is_empty() {
            bail!("Kraken API error: {:?}", errors);
        }
    }
    Ok(resp)
}

async fn fetch_public(client: &reqwest::Client, proxy_url: &str, token: &str, path: &str) -> Result<Value> {
    let resp: Value = client
        .get(format!("{}{}", proxy_url, path))
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await?
        .json()
        .await?;

    if let Some(errors) = resp["error"].as_array() {
        if !errors.is_empty() {
            bail!("Kraken API error: {:?}", errors);
        }
    }
    Ok(resp)
}
