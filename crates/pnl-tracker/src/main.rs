use anyhow::Result;
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::get,
};
use clap::Parser;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use kraken_core::exchange::messages::*;
use kraken_core::exchange::proxy_client::ProxyClient;
use kraken_core::exchange::ws::WsConnection;
use kraken_core::pnl::tracker::PnlTracker;
use kraken_core::traits::ExchangeClient;
use kraken_core::types::fill::Fill;
use kraken_core::types::order::OrderSide;

#[derive(Parser)]
#[command(name = "pnl-tracker", about = "Standalone P&L tracker that monitors fills via proxy")]
struct Args {
    /// Port for the REST API
    #[arg(long, default_value = "3031")]
    port: u16,

    /// State file path
    #[arg(long, default_value = "pnl_state.json")]
    state_file: String,

    /// Proxy base URL
    #[arg(long, env = "PROXY_URL")]
    proxy_url: String,

    /// Proxy bearer token
    #[arg(long, env = "PROXY_TOKEN")]
    proxy_token: String,
}

struct AppState {
    tracker: RwLock<PnlTracker>,
    prices: RwLock<HashMap<String, Decimal>>,
    state_file: PathBuf,
    api_token: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),
        )
        .init();

    let api_token = std::env::var("PNL_API_TOKEN").unwrap_or_default();
    let state_file = PathBuf::from(&args.state_file);

    // Load existing state
    let tracker = PnlTracker::load(&state_file)?
        .unwrap_or_else(|| {
            tracing::info!("No saved P&L state, starting fresh");
            PnlTracker::new()
        });

    tracing::info!(
        realized_pnl = %tracker.realized_pnl,
        total_fees = %tracker.total_fees,
        trades = tracker.trade_count,
        positions = tracker.positions.len(),
        "Loaded P&L state"
    );

    let shared = Arc::new(AppState {
        tracker: RwLock::new(tracker),
        prices: RwLock::new(HashMap::new()),
        state_file,
        api_token,
    });

    // Connect to proxy WS for execution feeds
    let proxy_client = ProxyClient::new(
        args.proxy_url.clone(),
        args.proxy_token.clone(),
    );

    let ws_url = {
        let base = args.proxy_url.trim_end_matches('/');
        let ws_base = base.replacen("http://", "ws://", 1).replacen("https://", "wss://", 1);
        format!("{}/ws/private", ws_base)
    };

    tracing::info!(url = ws_url, "Connecting to proxy WS for execution feed...");
    let mut ws = WsConnection::connect_with_token(&ws_url, &args.proxy_token).await?;

    // Subscribe to executions with snap_trades for recent history
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

    // Spawn the REST API
    let api_shared = shared.clone();
    let api_handle = tokio::spawn(async move {
        let router = build_pnl_router(api_shared);
        let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", args.port))
            .await
            .expect("Failed to bind");
        tracing::info!(port = args.port, "P&L tracker API listening");
        if let Err(e) = axum::serve(listener, router).await {
            tracing::error!(error = %e, "P&L API server error");
        }
    });

    // Process execution feed
    let feed_shared = shared.clone();
    let feed_handle = tokio::spawn(async move {
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

                    let mut tracker = feed_shared.tracker.write().await;
                    tracker.apply_fill(&fill);

                    // Update last price
                    feed_shared
                        .prices
                        .write()
                        .await
                        .insert(fill.symbol, fill.price);

                    // Persist
                    if let Err(e) = tracker.save(&feed_shared.state_file) {
                        tracing::error!(error = %e, "Failed to persist P&L state");
                    }
                }
                WsMessage::SubscribeConfirmed { channel } => {
                    tracing::info!(channel, "Subscription confirmed");
                }
                _ => {}
            }
        }
        tracing::warn!("WS execution feed ended");
    });

    // Wait for shutdown
    tokio::signal::ctrl_c().await?;
    tracing::info!("Shutting down P&L tracker...");

    // Save final state
    let tracker = shared.tracker.read().await;
    if let Err(e) = tracker.save(&shared.state_file) {
        tracing::error!(error = %e, "Failed to save final state");
    }

    api_handle.abort();
    feed_handle.abort();
    tracing::info!("P&L tracker shutdown complete");
    Ok(())
}

fn build_pnl_router(shared: Arc<AppState>) -> Router {
    Router::new()
        .route("/pnl", get(get_pnl))
        .route("/positions", get(get_positions))
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
        Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))))
    }
}

async fn get_pnl(
    headers: HeaderMap,
    State(shared): State<Arc<AppState>>,
) -> (StatusCode, Json<Value>) {
    if let Err(e) = check_auth(&headers, &shared.api_token) {
        return e;
    }
    let tracker = shared.tracker.read().await;
    let prices = shared.prices.read().await;
    let summary = tracker.summary(&prices);
    (StatusCode::OK, Json(serde_json::to_value(&summary).unwrap_or_default()))
}

async fn get_positions(
    headers: HeaderMap,
    State(shared): State<Arc<AppState>>,
) -> (StatusCode, Json<Value>) {
    if let Err(e) = check_auth(&headers, &shared.api_token) {
        return e;
    }
    let tracker = shared.tracker.read().await;
    let prices = shared.prices.read().await;

    let positions: Vec<Value> = tracker
        .positions
        .iter()
        .map(|(symbol, pos)| {
            let current_price = prices.get(symbol).copied().unwrap_or_default();
            json!({
                "symbol": symbol,
                "qty": pos.qty,
                "avg_cost": pos.avg_cost,
                "current_price": current_price,
                "current_value": pos.value_at(current_price),
                "unrealized_pnl": pos.qty * (current_price - pos.avg_cost),
            })
        })
        .collect();

    (StatusCode::OK, Json(json!(positions)))
}

async fn get_trades(
    headers: HeaderMap,
    State(shared): State<Arc<AppState>>,
) -> (StatusCode, Json<Value>) {
    if let Err(e) = check_auth(&headers, &shared.api_token) {
        return e;
    }
    let tracker = shared.tracker.read().await;
    (
        StatusCode::OK,
        Json(serde_json::to_value(&tracker.recent_trades).unwrap_or_default()),
    )
}
