use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

use kraken_core::api::shared::SharedState;
use kraken_core::api::server::build_router;
use kraken_core::config::Config;
use kraken_core::engine::core::Engine;
use kraken_core::exchange::live::LiveExchange;
use kraken_core::exchange::proxy_client::ProxyClient;
use kraken_core::exchange::rest::KrakenRest;
use kraken_core::state::bot_state::BotState;
use kraken_core::state::csv_logger::CsvTradeLogger;
use kraken_core::state::json_store::JsonStateStore;
use kraken_core::traits::*;
use kraken_core::types::*;

#[derive(Parser)]
#[command(name = "kraken-mm", about = "Kraken low-liquidity market making bot")]
struct Args {
    /// Paper trading mode (logs orders, doesn't send them)
    #[arg(long)]
    dry_run: bool,

    /// Target pairs (e.g., CAMP/USD SUP/USD)
    #[arg(long, num_args = 1..)]
    pairs: Vec<String>,

    /// Load pairs from scanner output file instead of --pairs
    #[arg(long, default_value = "scanned_pairs.json")]
    scan_file: String,

    /// Log level
    #[arg(long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(&args.log_level)
        .init();

    let mut config = Config::default();
    config.exchange.api_key = std::env::var("KRAKEN_API_KEY").unwrap_or_default();
    config.exchange.api_secret = std::env::var("KRAKEN_API_SECRET").unwrap_or_default();
    config.exchange.proxy_mode = std::env::var("PROXY_MODE").unwrap_or_default() == "true";
    config.exchange.proxy_url = std::env::var("PROXY_URL").unwrap_or_default();
    config.exchange.proxy_token = std::env::var("PROXY_TOKEN").unwrap_or_default();
    config.trading.dry_run = args.dry_run;

    if !args.pairs.is_empty() {
        config.trading.pairs = args.pairs;
    }

    // If no pairs given on CLI, try loading from scan file
    if config.trading.pairs.is_empty() {
        match load_scan_file(&args.scan_file) {
            Ok(symbols) if !symbols.is_empty() => {
                tracing::info!(
                    file = args.scan_file,
                    count = symbols.len(),
                    "Loaded pairs from scan file"
                );
                config.trading.pairs = symbols;
            }
            Ok(_) => {
                anyhow::bail!(
                    "No pairs specified and scan file is empty. \
                     Use --pairs CAMP/USD SUP/USD ... or run the scanner first."
                );
            }
            Err(_) => {
                anyhow::bail!(
                    "No pairs specified and no scan file found at '{}'. \
                     Use --pairs CAMP/USD SUP/USD ... or run the scanner first.",
                    args.scan_file
                );
            }
        }
    }

    let mode = if config.trading.dry_run { "DRY-RUN" } else { "LIVE" };
    tracing::info!(mode, pairs = ?config.trading.pairs, "Starting Kraken Market Maker");

    if !config.trading.dry_run && !config.exchange.proxy_mode
        && (config.exchange.api_key.is_empty() || config.exchange.api_secret.is_empty())
    {
        anyhow::bail!("Set KRAKEN_API_KEY and KRAKEN_API_SECRET environment variables");
    }

    if config.exchange.proxy_mode && config.exchange.proxy_url.is_empty() {
        anyhow::bail!("PROXY_MODE=true but PROXY_URL is not set");
    }

    // Build exchange client (direct or proxy)
    let exchange: Arc<dyn ExchangeClient> = if config.exchange.proxy_mode {
        tracing::info!(
            proxy_url = config.exchange.proxy_url,
            "Using proxy mode"
        );
        Arc::new(ProxyClient::new(
            config.exchange.proxy_url.clone(),
            config.exchange.proxy_token.clone(),
        ))
    } else {
        Arc::new(KrakenRest::new(config.exchange.clone()))
    };

    // Fetch pair info
    tracing::info!("Fetching pair info...");
    let pair_info = exchange.get_pair_info(&config.trading.pairs).await?;

    for (symbol, info) in &pair_info {
        tracing::info!(
            symbol,
            min_order = %info.min_order_qty,
            price_dec = info.price_decimals,
            qty_dec = info.qty_decimals,
            cost_min = %info.min_cost,
            "Pair info"
        );
    }

    // Batch ticker filter: single request for all pairs, identify downtrends
    tracing::info!("Fetching 24h ticker data for all pairs...");
    let mut downtrend_pairs = std::collections::HashSet::new();
    match exchange.get_tickers(&pair_info).await {
        Ok(tickers) => {
            for (symbol, td) in &tickers {
                if td.change_pct < config.trading.downtrend_threshold_pct {
                    tracing::warn!(
                        symbol,
                        change_pct = %td.change_pct.round_dp(2),
                        threshold = %config.trading.downtrend_threshold_pct,
                        "DOWNTREND — sell-only mode (no new buys)"
                    );
                    downtrend_pairs.insert(symbol.clone());
                } else {
                    tracing::info!(
                        symbol,
                        change_pct = %td.change_pct.round_dp(2),
                        close = %td.close,
                        volume_24h = %td.volume_24h.round_dp(2),
                        "PASS — within threshold"
                    );
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to fetch tickers — allowing buys for all pairs");
        }
    }

    if !downtrend_pairs.is_empty() {
        tracing::info!(
            count = downtrend_pairs.len(),
            pairs = ?downtrend_pairs,
            "Pairs in sell-only mode due to downtrend"
        );
    }

    // Load or create state
    let store = JsonStateStore::new(&config.persistence.state_file);
    let mut state = store.load()?.unwrap_or_else(|| {
        tracing::info!("Fresh start (no saved state)");
        BotState::default()
    });

    // Clear stale open_orders from previous run (we cancel_all on shutdown)
    if !state.open_orders.is_empty() {
        tracing::info!(
            count = state.open_orders.len(),
            "Clearing stale open orders from previous session"
        );
        state.open_orders.clear();
    }

    // Reconcile positions with actual Kraken balances (live mode only)
    if !config.trading.dry_run && !state.positions.is_empty() {
        tracing::info!("Reconciling state positions with Kraken balances...");
        match exchange.get_balances().await {
            Ok(balances) => {
                let symbols: Vec<String> = state.positions.keys().cloned().collect();
                for symbol in symbols {
                    let pos = match state.positions.get(&symbol) {
                        Some(p) => p.clone(),
                        None => continue,
                    };
                    let base_asset = pair_info
                        .get(&symbol)
                        .map(|pi| pi.base_asset.as_str())
                        .unwrap_or("");
                    if base_asset.is_empty() {
                        tracing::warn!(
                            symbol,
                            qty = %pos.qty,
                            "Cannot reconcile — no pair_info (pair may have been removed from config)"
                        );
                        continue;
                    }
                    let kraken_qty = balances.get(base_asset).copied().unwrap_or_default();
                    let min_order_qty = pair_info.get(&symbol).map_or(rust_decimal::Decimal::ONE, |pi| pi.min_order_qty);
                    if kraken_qty < min_order_qty {
                        tracing::warn!(
                            symbol,
                            base_asset,
                            state_qty = %pos.qty,
                            kraken_qty = %kraken_qty,
                            min_order_qty = %min_order_qty,
                            "Removing stale position — balance below min order qty (dust)"
                        );
                        state.positions.remove(&symbol);
                    } else if kraken_qty != pos.qty {
                        tracing::warn!(
                            symbol,
                            base_asset,
                            state_qty = %pos.qty,
                            kraken_qty = %kraken_qty,
                            avg_cost = %pos.avg_cost,
                            "Adjusting position qty to match Kraken balance"
                        );
                        if let Some(p) = state.positions.get_mut(&symbol) {
                            p.qty = kraken_qty;
                        }
                    } else {
                        tracing::info!(
                            symbol,
                            base_asset,
                            qty = %pos.qty,
                            "Balance match"
                        );
                    }
                }
                // Save reconciled state
                store.save(&state)?;
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to fetch balances — skipping reconciliation");
            }
        }
    }

    // Log loaded positions
    for (symbol, pos) in &state.positions {
        tracing::info!(
            symbol,
            qty = %pos.qty,
            avg_cost = %pos.avg_cost,
            "Loaded existing position"
        );
    }
    if !state.positions.is_empty() {
        tracing::info!(
            realized_pnl = %state.realized_pnl,
            total_fees = %state.total_fees,
            trades = state.trade_count,
            "Resuming from saved state"
        );
    }

    let logger = CsvTradeLogger::new(&config.persistence.trade_log_file)?;

    // Create engine
    let mut engine = Engine::new(config.clone(), pair_info, state.clone());
    if !downtrend_pairs.is_empty() {
        engine.set_sell_only(downtrend_pairs);
    }

    // Event channel: feeds → engine
    let (event_tx, event_rx) = mpsc::channel::<EngineEvent>(1024);
    // Command channel: engine → dispatcher
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(256);

    // REST API setup
    let api_token = std::env::var("BOT_API_TOKEN").unwrap_or_default();
    let api_port: u16 = std::env::var("BOT_API_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3030);

    let shared_state = Arc::new(SharedState {
        bot_state: Arc::new(RwLock::new(state)),
        config: Arc::new(config.clone()),
        event_tx: event_tx.clone(),
        trade_log_path: config.persistence.trade_log_file.clone(),
        exchange: exchange.clone(),
    });

    // Spawn API server if token is set
    let api_handle = if !api_token.is_empty() {
        let router = build_router(shared_state.clone(), api_token);
        let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", api_port)).await?;
        tracing::info!(port = api_port, "REST API server starting");
        Some(tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router).await {
                tracing::error!(error = %e, "API server error");
            }
        }))
    } else {
        tracing::info!("REST API disabled (set BOT_API_TOKEN to enable)");
        None
    };

    let result = if config.trading.dry_run {
        run_dry(config, engine, event_tx, event_rx, cmd_tx, cmd_rx, store, logger, shared_state.clone()).await
    } else {
        run_live(config, engine, event_tx, event_rx, cmd_tx, cmd_rx, store, logger, shared_state.clone(), exchange).await
    };

    // Clean up API server
    if let Some(h) = api_handle {
        h.abort();
    }

    result
}

async fn run_dry(
    config: Config,
    mut engine: Engine,
    event_tx: mpsc::Sender<EngineEvent>,
    event_rx: mpsc::Receiver<EngineEvent>,
    cmd_tx: mpsc::Sender<EngineCommand>,
    mut cmd_rx: mpsc::Receiver<EngineCommand>,
    store: JsonStateStore,
    mut logger: CsvTradeLogger,
    shared: Arc<SharedState>,
) -> Result<()> {
    use kraken_core::exchange::ws::*;
    use kraken_core::exchange::messages::*;
    use chrono::Utc;

    // Connect public WS for book data
    let mut pub_ws = WsConnection::connect(&config.exchange.ws_public_url).await?;
    tracing::info!("Public WS connected");
    subscribe_book(&mut pub_ws, &config.trading.pairs, config.exchange.book_depth).await?;

    // Spawn book feed reader
    let tx = event_tx.clone();
    let book_handle = tokio::spawn(async move {
        while let Some(raw) = pub_ws.recv().await {
            let msg = parse_ws_message(&raw);
            let event = match msg {
                WsMessage::BookSnapshot { symbol, bids, asks } => {
                    Some(EngineEvent::BookSnapshot { symbol, bids, asks, timestamp: Utc::now() })
                }
                WsMessage::BookUpdate { symbol, bid_updates, ask_updates } => {
                    Some(EngineEvent::BookUpdate { symbol, bid_updates, ask_updates, timestamp: Utc::now() })
                }
                WsMessage::SubscribeConfirmed { channel } => {
                    tracing::info!(channel, "Subscription confirmed");
                    None
                }
                _ => None,
            };
            if let Some(e) = event {
                if tx.send(e).await.is_err() { break; }
            }
        }
    });

    // Spawn tick timer
    let tx2 = event_tx.clone();
    let ticker_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            interval.tick().await;
            if tx2.send(EngineEvent::Tick { timestamp: Utc::now() }).await.is_err() {
                break;
            }
        }
    });

    // Spawn engine
    let engine_handle = tokio::spawn(async move {
        engine.run(event_rx, cmd_tx).await
    });

    // Command dispatcher (dry-run: just log)
    let dispatch_handle = tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            match &cmd {
                EngineCommand::PlaceOrder(req) => {
                    tracing::info!(
                        side = %req.side, symbol = req.symbol,
                        price = %req.price, qty = %req.qty,
                        "[DRY-RUN] Would place order"
                    );
                }
                EngineCommand::AmendOrder { cl_ord_id, new_price, .. } => {
                    tracing::info!(cl_ord_id, price = ?new_price, "[DRY-RUN] Would amend");
                }
                EngineCommand::CancelOrders(ids) => {
                    tracing::info!(?ids, "[DRY-RUN] Would cancel");
                }
                EngineCommand::CancelAll => {
                    tracing::info!("[DRY-RUN] Would cancel all");
                }
                EngineCommand::RefreshDms => {
                    tracing::debug!("[DRY-RUN] Would refresh DMS");
                }
                EngineCommand::PersistState(state) => {
                    let _ = store.save(state);
                    *shared.bot_state.write().await = state.clone();
                }
                EngineCommand::LogTrade(record) => {
                    let _ = logger.log_trade(record);
                }
                EngineCommand::Shutdown { reason } => {
                    tracing::warn!(reason, "Shutdown requested");
                    break;
                }
            }
        }
    });

    // Handle Ctrl+C
    tokio::signal::ctrl_c().await?;
    tracing::info!("Shutting down...");

    // Force exit on second Ctrl+C
    tokio::spawn(async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::warn!("Force exit (second Ctrl+C)");
        std::process::exit(1);
    });

    // Abort feed tasks so their event_tx clones are dropped
    book_handle.abort();
    ticker_handle.abort();
    drop(event_tx);

    // Give engine/dispatcher 3 seconds to finish, then force exit
    let shutdown = async {
        let _ = engine_handle.await;
        let _ = dispatch_handle.await;
    };
    if tokio::time::timeout(std::time::Duration::from_secs(3), shutdown).await.is_err() {
        tracing::warn!("Shutdown timed out, forcing exit");
        std::process::exit(1);
    }

    tracing::info!("Shutdown complete");
    Ok(())
}

async fn run_live(
    config: Config,
    mut engine: Engine,
    event_tx: mpsc::Sender<EngineEvent>,
    event_rx: mpsc::Receiver<EngineEvent>,
    cmd_tx: mpsc::Sender<EngineCommand>,
    mut cmd_rx: mpsc::Receiver<EngineCommand>,
    store: JsonStateStore,
    mut logger: CsvTradeLogger,
    shared: Arc<SharedState>,
    exchange: Arc<dyn ExchangeClient>,
) -> Result<()> {
    let live = LiveExchange::connect(config.clone(), event_tx.clone(), exchange).await?;
    let live = std::sync::Arc::new(live);

    // Clear any ghost orders from previous sessions (one-time cleanup)
    tracing::info!("Cancelling any leftover orders from previous sessions...");
    live.cancel_all().await?;

    // Spawn feeds (private WS read loop already spawned by connect)
    let book_handle = live.spawn_book_feed(event_tx.clone()).await?;
    let ticker_handle = live.spawn_ticker(event_tx.clone(), 10);

    // Spawn engine
    let engine_handle = tokio::spawn(async move {
        engine.run(event_rx, cmd_tx).await
    });

    // Command dispatcher (live)
    let live_ref = live.clone();
    let dms_timeout = config.risk.dms_timeout_secs;
    let dispatch_handle = tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            let result: Result<()> = async {
                match cmd {
                    EngineCommand::PlaceOrder(req) => {
                        tracing::info!(
                            symbol = req.symbol,
                            side = %req.side,
                            price = %req.price,
                            qty = %req.qty,
                            cl_ord_id = req.cl_ord_id,
                            "Sending order to exchange"
                        );
                        live_ref.place_order(&req).await?;
                    }
                    EngineCommand::AmendOrder { cl_ord_id, new_price, new_qty } => {
                        tracing::info!(cl_ord_id, price = ?new_price, qty = ?new_qty, "Amending order");
                        live_ref.amend_order(&cl_ord_id, new_price, new_qty).await?;
                    }
                    EngineCommand::CancelOrders(ids) => {
                        tracing::info!(?ids, "Cancelling orders");
                        live_ref.cancel_orders(&ids).await?;
                    }
                    EngineCommand::CancelAll => {
                        tracing::info!("Cancelling all orders");
                        live_ref.cancel_all().await?;
                    }
                    EngineCommand::RefreshDms => {
                        live_ref.refresh(dms_timeout).await?;
                    }
                    EngineCommand::PersistState(state) => {
                        store.save(&state)?;
                        *shared.bot_state.write().await = state;
                    }
                    EngineCommand::LogTrade(record) => {
                        logger.log_trade(&record)?;
                    }
                    EngineCommand::Shutdown { reason } => {
                        tracing::warn!(reason, "Shutdown requested");
                        let _ = live_ref.cancel_all().await;
                        let _ = live_ref.disable().await;
                        return Ok(());
                    }
                }
                Ok(())
            }.await;
            if let Err(e) = result {
                tracing::error!(error = %e, "Command dispatch error");
            }
        }
    });

    tokio::signal::ctrl_c().await?;
    tracing::info!("Shutting down — cancelling all orders...");
    let _ = live.cancel_all().await;
    let _ = live.disable().await;

    // Force exit on second Ctrl+C
    tokio::spawn(async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::warn!("Force exit (second Ctrl+C)");
        std::process::exit(1);
    });

    // Abort all feed tasks so their event_tx clones are dropped
    book_handle.abort();
    ticker_handle.abort();
    live.abort_tasks().await;
    drop(event_tx);

    // Give engine/dispatcher 3 seconds to finish, then force exit
    let shutdown = async {
        let _ = engine_handle.await;
        let _ = dispatch_handle.await;
    };
    if tokio::time::timeout(std::time::Duration::from_secs(3), shutdown).await.is_err() {
        tracing::warn!("Shutdown timed out, forcing exit");
        std::process::exit(1);
    }

    tracing::info!("Shutdown complete");
    Ok(())
}

/// Load pair symbols from a scanner output JSON file.
fn load_scan_file(path: &str) -> Result<Vec<String>> {
    let data = std::fs::read_to_string(path)?;
    let v: serde_json::Value = serde_json::from_str(&data)?;
    let symbols = v["symbols"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Ok(symbols)
}
