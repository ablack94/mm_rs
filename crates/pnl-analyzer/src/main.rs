mod config;
mod edge;
mod state_store;

use anyhow::Result;
use axum::{
    Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::get,
};
use chrono::Utc;
use clap::Parser;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

use kraken_core::exchange::messages::*;
use kraken_core::exchange::proxy_client::ProxyClient;
use kraken_core::exchange::ws::WsConnection;
use kraken_core::pnl::tracker::{PnlTrade, PnlTracker};
use kraken_core::traits::ExchangeClient;
use kraken_core::types::fill::Fill;
use kraken_core::types::order::OrderSide;

use crate::config::AnalyzerConfig;
use crate::edge::EdgeTracker;
use crate::state_store::{PatchPairBody, PatchPairConfig, StateStoreClient};

#[derive(Parser)]
#[command(
    name = "pnl-analyzer",
    about = "PnL analyzer: monitors fills, computes per-pair edge, drives pair state changes"
)]
struct Args {
    /// Port for the REST API
    #[arg(long, env = "PNL_ANALYZER_PORT", default_value = "3031")]
    port: u16,

    /// State file path for the PnL tracker
    #[arg(long, default_value = "pnl_state.json")]
    pnl_state_file: String,

    /// State file path for the analyzer (edge metrics)
    #[arg(long, default_value = "pnl_analyzer_state.json")]
    analyzer_state_file: String,

    /// Proxy base URL
    #[arg(long, env = "PROXY_URL")]
    proxy_url: String,

    /// Proxy bearer token
    #[arg(long, env = "PROXY_TOKEN")]
    proxy_token: String,
}

struct AppState {
    tracker: RwLock<PnlTracker>,
    edge_tracker: RwLock<EdgeTracker>,
    prices: RwLock<HashMap<String, Decimal>>,
    pnl_state_file: PathBuf,
    analyzer_state_file: PathBuf,
    api_token: String,
    analyzer_config: AnalyzerConfig,
    start_time: Instant,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),
        )
        .init();

    let analyzer_config = AnalyzerConfig::from_env();
    let api_token = std::env::var("PNL_API_TOKEN").unwrap_or_default();
    let pnl_state_file = PathBuf::from(&args.pnl_state_file);
    let analyzer_state_file = PathBuf::from(&args.analyzer_state_file);

    // Load existing PnL tracker state
    let tracker = PnlTracker::load(&pnl_state_file)?.unwrap_or_else(|| {
        tracing::info!("No saved PnL state, starting fresh");
        PnlTracker::new()
    });

    tracing::info!(
        realized_pnl = %tracker.realized_pnl,
        total_fees = %tracker.total_fees,
        trades = tracker.trade_count,
        positions = tracker.positions.len(),
        "Loaded PnL tracker state"
    );

    // Load existing edge tracker state
    let edge_tracker = load_edge_tracker(&analyzer_state_file);

    tracing::info!(
        known_pairs = edge_tracker.known_pairs().len(),
        "Loaded edge tracker state"
    );

    tracing::info!(
        eval_interval_secs = analyzer_config.eval_interval_secs,
        eval_window_hours = analyzer_config.eval_window_hours,
        high_edge = %analyzer_config.high_edge_threshold,
        low_edge = %analyzer_config.low_edge_threshold,
        negative_edge = %analyzer_config.negative_edge_threshold,
        state_store_url = analyzer_config.state_store_url,
        "Analyzer config loaded"
    );

    let shared = Arc::new(AppState {
        tracker: RwLock::new(tracker),
        edge_tracker: RwLock::new(edge_tracker),
        prices: RwLock::new(HashMap::new()),
        pnl_state_file,
        analyzer_state_file,
        api_token,
        analyzer_config,
        start_time: Instant::now(),
    });

    // WS connection info for execution feeds
    let ws_url = {
        let base = args.proxy_url.trim_end_matches('/');
        let ws_base = base
            .replacen("http://", "ws://", 1)
            .replacen("https://", "wss://", 1);
        format!("{}/ws/private", ws_base)
    };
    let proxy_url = args.proxy_url.clone();
    let proxy_token = args.proxy_token.clone();

    // Spawn the REST API
    let api_shared = shared.clone();
    let api_port = args.port;
    let api_handle = tokio::spawn(async move {
        let router = build_router(api_shared);
        let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", api_port))
            .await
            .expect("Failed to bind");
        tracing::info!(port = api_port, "PnL analyzer API listening");
        if let Err(e) = axum::serve(listener, router).await {
            tracing::error!(error = %e, "PnL analyzer API server error");
        }
    });

    // Spawn the fill processing loop with reconnection
    let feed_shared = shared.clone();
    let feed_handle = tokio::spawn(async move {
        run_feed_with_reconnect(feed_shared, &ws_url, &proxy_url, &proxy_token).await;
    });

    // Spawn the evaluation loop
    let eval_shared = shared.clone();
    let eval_handle = tokio::spawn(async move {
        run_evaluation_loop(eval_shared).await;
    });

    // Wait for shutdown
    tokio::signal::ctrl_c().await?;
    tracing::info!("Shutting down PnL analyzer...");

    // Save final state
    {
        let tracker = shared.tracker.read().await;
        if let Err(e) = tracker.save(&shared.pnl_state_file) {
            tracing::error!(error = %e, "Failed to save final PnL state");
        }
    }
    {
        let edge_tracker = shared.edge_tracker.read().await;
        save_edge_tracker(&edge_tracker, &shared.analyzer_state_file);
    }

    api_handle.abort();
    feed_handle.abort();
    eval_handle.abort();
    tracing::info!("PnL analyzer shutdown complete");
    Ok(())
}

// ---------------------------------------------------------------------------
// Fill feed processing
// ---------------------------------------------------------------------------

async fn run_feed_with_reconnect(
    shared: Arc<AppState>,
    ws_url: &str,
    proxy_url: &str,
    proxy_token: &str,
) {
    let mut backoff_secs = 1u64;
    let max_backoff = 30u64;

    loop {
        tracing::info!(url = ws_url, "Connecting to proxy WS for execution feed...");

        let result: Result<()> = async {
            let mut ws = WsConnection::connect_with_token(ws_url, proxy_token).await?;

            let proxy_client = ProxyClient::new(proxy_url.to_string(), proxy_token.to_string());
            let token = proxy_client.get_ws_token().await?;
            ws.send_json(&SubscribeExecMsg {
                method: "subscribe",
                params: SubscribeExecParams {
                    channel: "executions",
                    snap_orders: false,
                    snap_trades: true,
                    ratecounter: false,
                    token,
                },
            })
            .await?;

            tracing::info!("Subscribed to execution feed");
            backoff_secs = 1; // reset on successful connect

            process_fill_feed(&mut ws, &shared).await;
            Ok(())
        }
        .await;

        if let Err(e) = result {
            tracing::warn!(error = %e, backoff_secs, "WS connection failed, will retry");
        } else {
            tracing::warn!(backoff_secs, "WS execution feed disconnected, will reconnect");
        }

        tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(max_backoff);
    }
}

async fn process_fill_feed(ws: &mut WsConnection, shared: &Arc<AppState>) {
    while let Some(raw) = ws.recv().await {
        let msg = parse_ws_message(&raw);
        match msg {
            WsMessage::Execution(report) if report.exec_type == "trade" => {
                let side = if report.side == "buy" {
                    OrderSide::Buy
                } else {
                    OrderSide::Sell
                };

                let fill = Fill {
                    order_id: report.order_id,
                    cl_ord_id: report.cl_ord_id,
                    symbol: report.symbol.clone(),
                    side,
                    price: report.last_price,
                    qty: report.last_qty,
                    fee: report.fee,
                    is_maker: report.is_maker,
                    is_fully_filled: report.order_status == "filled",
                    timestamp: report.timestamp,
                };

                tracing::info!(
                    symbol = fill.symbol,
                    side = %fill.side,
                    price = %fill.price,
                    qty = %fill.qty,
                    fee = %fill.fee,
                    "Fill received"
                );

                // Apply to PnL tracker
                let pnl;
                {
                    let mut tracker = shared.tracker.write().await;
                    // Compute PnL before applying (for edge tracking)
                    let pos = tracker.positions.get(&fill.symbol);
                    pnl = match fill.side {
                        OrderSide::Sell => {
                            let avg_cost = pos.map(|p| p.avg_cost).unwrap_or_default();
                            fill.qty * (fill.price - avg_cost) - fill.fee
                        }
                        OrderSide::Buy => Decimal::ZERO - fill.fee,
                    };

                    tracker.apply_fill(&fill);

                    // Persist PnL state
                    if let Err(e) = tracker.save(&shared.pnl_state_file) {
                        tracing::error!(error = %e, "Failed to persist PnL state");
                    }
                }

                // Update last price
                shared
                    .prices
                    .write()
                    .await
                    .insert(fill.symbol.clone(), fill.price);

                // Record in edge tracker
                let trade = PnlTrade {
                    timestamp: fill.timestamp,
                    symbol: fill.symbol,
                    side: fill.side.to_string(),
                    price: fill.price,
                    qty: fill.qty,
                    fee: fill.fee,
                    pnl,
                };
                {
                    let mut et = shared.edge_tracker.write().await;
                    et.record_trade(&trade);
                }

                // Persist edge state periodically (every trade is fine for now)
                {
                    let et = shared.edge_tracker.read().await;
                    save_edge_tracker(&et, &shared.analyzer_state_file);
                }
            }
            WsMessage::SubscribeConfirmed { channel } => {
                tracing::info!(channel, "Subscription confirmed");
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Evaluation loop
// ---------------------------------------------------------------------------

async fn run_evaluation_loop(shared: Arc<AppState>) {
    let interval_secs = shared.analyzer_config.eval_interval_secs;
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));

    // Skip the first immediate tick to let the system settle
    interval.tick().await;

    tracing::info!(
        interval_secs,
        "Evaluation loop started"
    );

    let store_client = StateStoreClient::new(
        shared.analyzer_config.state_store_url.clone(),
        shared.analyzer_config.state_store_token.clone(),
    );

    loop {
        interval.tick().await;

        if let Err(e) = run_evaluation_cycle(&shared, &store_client).await {
            tracing::error!(error = %e, "Evaluation cycle failed");
        }
    }
}

async fn run_evaluation_cycle(
    shared: &Arc<AppState>,
    store: &StateStoreClient,
) -> Result<()> {
    let cfg = &shared.analyzer_config;
    let window = cfg.eval_window_hours;

    // Read current pair states from the state store
    let store_pairs = match store.get_pairs().await {
        Ok(pairs) => pairs,
        Err(e) => {
            tracing::warn!(error = %e, "Could not reach state store, skipping evaluation");
            return Ok(());
        }
    };

    let store_pair_map: HashMap<String, &crate::state_store::StorePairRecord> = store_pairs
        .iter()
        .map(|p| (p.symbol.clone(), p))
        .collect();

    // Compute edge metrics for all pairs
    let edges = {
        let et = shared.edge_tracker.read().await;
        et.all_pair_edges(window)
    };

    let now = Utc::now();

    for edge in &edges {
        let symbol = &edge.symbol;

        // Only act on pairs that exist in the state store
        let store_pair = match store_pair_map.get(symbol) {
            Some(p) => p,
            None => continue, // Not managed by state store; skip
        };

        // Skip disabled or liquidating pairs -- we don't promote/demote them
        if store_pair.state == "disabled" || store_pair.state == "liquidating" {
            continue;
        }

        // Check for zero trades -> disable
        let last_trade_at = {
            let et = shared.edge_tracker.read().await;
            et.last_trade_time(symbol)
        };

        if let Some(last_trade) = last_trade_at {
            let hours_since = (now - last_trade).num_hours();
            if hours_since >= cfg.zero_trades_disable_hours as i64
                && store_pair.state == "active"
            {
                tracing::warn!(
                    symbol,
                    hours_since,
                    "No trades for extended period, disabling pair"
                );
                if let Err(e) = store.patch_pair(
                    symbol,
                    &PatchPairBody {
                        state: Some("disabled".to_string()),
                        config: None,
                        disabled_reason: Some(format!(
                            "No trades for {} hours",
                            hours_since
                        )),
                    },
                ).await {
                    tracing::error!(symbol, error = %e, "Failed to disable pair via state store");
                }
                continue;
            }
        }

        // Skip pairs with no volume in the evaluation window
        if edge.trade_count == 0 {
            continue;
        }

        // High edge -> promote (increase inventory, tighten spread)
        if edge.edge >= cfg.high_edge_threshold && store_pair.state == "active" {
            tracing::info!(
                symbol,
                edge = %edge.edge,
                volume = %edge.total_volume,
                trades = edge.trade_count,
                "High edge detected, promoting pair"
            );
            if let Err(e) = store.patch_pair(
                symbol,
                &PatchPairBody {
                    state: None,
                    config: Some(PatchPairConfig {
                        order_size_usd: None,
                        max_inventory_usd: Some(cfg.promoted_max_inventory_usd),
                        min_spread_bps: Some(cfg.promoted_min_spread_bps),
                    }),
                    disabled_reason: None,
                },
            ).await {
                tracing::error!(symbol, error = %e, "Failed to promote pair via state store");
            }
        }
        // Sustained negative edge -> wind down
        else if edge.edge <= cfg.negative_edge_threshold && store_pair.state == "active" {
            tracing::warn!(
                symbol,
                edge = %edge.edge,
                volume = %edge.total_volume,
                trades = edge.trade_count,
                "Sustained negative edge, winding down pair"
            );
            if let Err(e) = store.patch_pair(
                symbol,
                &PatchPairBody {
                    state: Some("wind_down".to_string()),
                    config: None,
                    disabled_reason: Some(format!(
                        "Negative edge: {} over {}h window",
                        edge.edge, window
                    )),
                },
            ).await {
                tracing::error!(symbol, error = %e, "Failed to wind down pair via state store");
            }
        }
        // Low edge -> demote (reduce inventory, widen spread)
        else if edge.edge <= cfg.low_edge_threshold && store_pair.state == "active" {
            tracing::info!(
                symbol,
                edge = %edge.edge,
                volume = %edge.total_volume,
                trades = edge.trade_count,
                "Low edge detected, demoting pair"
            );
            if let Err(e) = store.patch_pair(
                symbol,
                &PatchPairBody {
                    state: None,
                    config: Some(PatchPairConfig {
                        order_size_usd: None,
                        max_inventory_usd: Some(cfg.demoted_max_inventory_usd),
                        min_spread_bps: Some(cfg.demoted_min_spread_bps),
                    }),
                    disabled_reason: None,
                },
            ).await {
                tracing::error!(symbol, error = %e, "Failed to demote pair via state store");
            }
        }
    }

    // Also check state store pairs that have NO edge data at all (never traded)
    for store_pair in &store_pairs {
        if store_pair.state != "active" {
            continue;
        }
        let has_edge = edges.iter().any(|e| e.symbol == store_pair.symbol);
        if has_edge {
            continue;
        }

        // Check if we know about this pair at all
        let last_trade = {
            let et = shared.edge_tracker.read().await;
            et.last_trade_time(&store_pair.symbol)
        };

        // If pair has been active but never traded, and it's been long enough
        if last_trade.is_none() {
            // We don't know when this pair was added, so we check created_at
            if let Some(ref created_str) = store_pair.created_at {
                if let Ok(created) = created_str.parse::<chrono::DateTime<Utc>>() {
                    let hours_since = (now - created).num_hours();
                    if hours_since >= cfg.zero_trades_disable_hours as i64 {
                        tracing::warn!(
                            symbol = store_pair.symbol,
                            hours_since,
                            "Active pair with zero trades since creation, disabling"
                        );
                        if let Err(e) = store.patch_pair(
                            &store_pair.symbol,
                            &PatchPairBody {
                                state: Some("disabled".to_string()),
                                config: None,
                                disabled_reason: Some(format!(
                                    "Zero trades for {} hours since creation",
                                    hours_since
                                )),
                            },
                        ).await {
                            tracing::error!(
                                symbol = store_pair.symbol,
                                error = %e,
                                "Failed to disable zero-trade pair"
                            );
                        }
                    }
                }
            }
        }
    }

    tracing::debug!(
        pairs_evaluated = edges.len(),
        store_pairs = store_pairs.len(),
        "Evaluation cycle complete"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// REST API
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct TradesQuery {
    limit: Option<usize>,
}

fn build_router(shared: Arc<AppState>) -> Router {
    Router::new()
        .route("/pnl", get(get_pnl))
        .route("/pairs", get(get_pairs))
        .route("/pairs/{symbol}", get(get_pair_detail))
        .route("/trades", get(get_trades))
        .with_state(shared)
}

fn check_auth(headers: &HeaderMap, token: &str) -> Result<(), (StatusCode, Json<Value>)> {
    if token.is_empty() {
        return Ok(());
    }
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if auth == format!("Bearer {}", token) {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        ))
    }
}

async fn get_pnl(
    headers: HeaderMap,
    State(shared): State<Arc<AppState>>,
) -> (StatusCode, Json<Value>) {
    if let Err(e) = check_auth(&headers, &shared.api_token) {
        return e;
    }

    let uptime = shared.start_time.elapsed().as_secs();
    let window = shared.analyzer_config.eval_window_hours;

    let et = shared.edge_tracker.read().await;
    let summary = et.summary(window, uptime);

    // Also include PnL tracker summary
    let tracker = shared.tracker.read().await;
    let prices = shared.prices.read().await;
    let pnl_summary = tracker.summary(&prices);

    (
        StatusCode::OK,
        Json(json!({
            "pnl": {
                "realized_pnl": pnl_summary.realized_pnl,
                "unrealized_pnl": pnl_summary.unrealized_pnl,
                "total_fees": pnl_summary.total_fees,
                "net_pnl": pnl_summary.net_pnl,
                "trade_count": pnl_summary.trade_count,
            },
            "edge": {
                "window_hours": window,
                "total_realized_pnl": summary.total_realized_pnl,
                "total_fees": summary.total_fees,
                "total_volume": summary.total_volume,
                "total_trade_count": summary.total_trade_count,
                "overall_edge": summary.overall_edge,
                "pair_count": summary.pair_count,
            },
            "uptime_secs": uptime,
        })),
    )
}

async fn get_trades(
    headers: HeaderMap,
    Query(params): Query<TradesQuery>,
    State(shared): State<Arc<AppState>>,
) -> (StatusCode, Json<Value>) {
    if let Err(e) = check_auth(&headers, &shared.api_token) {
        return e;
    }

    let limit = params.limit.unwrap_or(50);
    let tracker = shared.tracker.read().await;

    let trades: Vec<Value> = tracker
        .recent_trades
        .iter()
        .rev()
        .take(limit)
        .map(|t| {
            let value = t.price * t.qty;
            json!({
                "timestamp": t.timestamp,
                "symbol": t.symbol,
                "side": t.side,
                "price": t.price,
                "qty": t.qty,
                "value": value,
                "fee": t.fee,
                "pnl": t.pnl,
            })
        })
        .collect();

    (StatusCode::OK, Json(json!(trades)))
}

async fn get_pairs(
    headers: HeaderMap,
    State(shared): State<Arc<AppState>>,
) -> (StatusCode, Json<Value>) {
    if let Err(e) = check_auth(&headers, &shared.api_token) {
        return e;
    }

    let et = shared.edge_tracker.read().await;
    let windows = [1, 6, 24];

    let pairs: Vec<Value> = et
        .known_pairs()
        .iter()
        .map(|symbol| {
            let edges: HashMap<String, Value> = windows
                .iter()
                .map(|&w| {
                    let edge = et.pair_edge(symbol, w);
                    (
                        format!("{}h", w),
                        json!({
                            "realized_pnl": edge.realized_pnl,
                            "total_volume": edge.total_volume,
                            "total_fees": edge.total_fees,
                            "trade_count": edge.trade_count,
                            "edge": edge.edge,
                        }),
                    )
                })
                .collect();

            json!({
                "symbol": symbol,
                "last_trade_at": et.last_trade_time(symbol),
                "windows": edges,
            })
        })
        .collect();

    (StatusCode::OK, Json(json!({ "pairs": pairs })))
}

async fn get_pair_detail(
    headers: HeaderMap,
    Path(symbol_raw): Path<String>,
    State(shared): State<Arc<AppState>>,
) -> (StatusCode, Json<Value>) {
    if let Err(e) = check_auth(&headers, &shared.api_token) {
        return e;
    }

    // Support both OMG-USD and OMG/USD in the URL
    let symbol = symbol_raw.replace('-', "/");

    let et = shared.edge_tracker.read().await;
    let tracker = shared.tracker.read().await;
    let prices = shared.prices.read().await;

    let last_trade = et.last_trade_time(&symbol);
    if last_trade.is_none() && !tracker.positions.contains_key(&symbol) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "pair not found"})),
        );
    }

    let windows: Vec<Value> = [1, 6, 24]
        .iter()
        .map(|&w| {
            let edge = et.pair_edge(&symbol, w);
            json!({
                "window_hours": w,
                "realized_pnl": edge.realized_pnl,
                "total_volume": edge.total_volume,
                "total_fees": edge.total_fees,
                "trade_count": edge.trade_count,
                "edge": edge.edge,
            })
        })
        .collect();

    let position = tracker.positions.get(&symbol).map(|pos| {
        let current_price = prices.get(&symbol).copied().unwrap_or_default();
        json!({
            "qty": pos.qty,
            "avg_cost": pos.avg_cost,
            "current_price": current_price,
            "current_value": pos.value_at(current_price),
            "unrealized_pnl": pos.qty * (current_price - pos.avg_cost),
        })
    });

    // Evaluation status
    let cfg = &shared.analyzer_config;
    let eval_edge = et.pair_edge(&symbol, cfg.eval_window_hours);
    let classification = if eval_edge.trade_count == 0 {
        "no_data"
    } else if eval_edge.edge >= cfg.high_edge_threshold {
        "high_edge"
    } else if eval_edge.edge <= cfg.negative_edge_threshold {
        "negative_edge"
    } else if eval_edge.edge <= cfg.low_edge_threshold {
        "low_edge"
    } else {
        "neutral"
    };

    (
        StatusCode::OK,
        Json(json!({
            "symbol": symbol,
            "last_trade_at": last_trade,
            "position": position,
            "windows": windows,
            "evaluation": {
                "window_hours": cfg.eval_window_hours,
                "edge": eval_edge.edge,
                "classification": classification,
                "high_threshold": cfg.high_edge_threshold,
                "low_threshold": cfg.low_edge_threshold,
                "negative_threshold": cfg.negative_edge_threshold,
            }
        })),
    )
}

// ---------------------------------------------------------------------------
// Persistence helpers
// ---------------------------------------------------------------------------

fn load_edge_tracker(path: &PathBuf) -> EdgeTracker {
    if !path.exists() {
        tracing::info!("No saved analyzer state, starting fresh");
        return EdgeTracker::new();
    }
    match std::fs::read_to_string(path) {
        Ok(data) => match serde_json::from_str(&data) {
            Ok(tracker) => {
                tracing::info!("Loaded analyzer state from {}", path.display());
                tracker
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Failed to parse analyzer state, starting fresh"
                );
                EdgeTracker::new()
            }
        },
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Failed to read analyzer state, starting fresh"
            );
            EdgeTracker::new()
        }
    }
}

fn save_edge_tracker(tracker: &EdgeTracker, path: &PathBuf) {
    let tmp = path.with_extension("json.tmp");
    match serde_json::to_string_pretty(tracker) {
        Ok(data) => {
            if let Err(e) = std::fs::write(&tmp, &data) {
                tracing::error!(error = %e, "Failed to write analyzer state tmp file");
                return;
            }
            if let Err(e) = std::fs::rename(&tmp, path) {
                tracing::error!(error = %e, "Failed to rename analyzer state file");
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to serialize analyzer state");
        }
    }
}
