use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tokio::sync::mpsc;

use kraken_core::config::Config;
use kraken_core::engine::core::Engine;
use kraken_core::engine::quoter::QuoteState;
use kraken_core::exchange::live::LiveExchange;
use kraken_core::exchange::proxy_client::ProxyClient;
use kraken_core::state::bot_state::BotState;
use kraken_core::state::csv_logger::CsvTradeLogger;
use kraken_core::state_store::{
    EngineSnapshot, PairReportData, StateStoreClient, StateStoreCommand, StateStoreConfig,
};
use kraken_core::traits::*;
use kraken_core::types::*;
use kraken_core::types::event::StateStoreAction;

#[derive(Parser)]
#[command(name = "kraken-mm", about = "Kraken low-liquidity market making bot")]
struct Args {
    /// Paper trading mode (logs orders, doesn't send them)
    #[arg(long)]
    dry_run: bool,

    /// Target pairs (e.g., CAMP/USD SUP/USD)
    #[arg(long, num_args = 1..)]
    pairs: Vec<String>,

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
    config.exchange.proxy_url = std::env::var("PROXY_URL").unwrap_or_default();
    config.exchange.proxy_token = std::env::var("PROXY_TOKEN").unwrap_or_default();
    config.trading.dry_run = args.dry_run;

    if !args.pairs.is_empty() {
        config.trading.pairs = args.pairs;
    }

    if config.trading.pairs.is_empty() {
        anyhow::bail!(
            "No pairs specified. Use --pairs CAMP/USD SUP/USD ..."
        );
    }

    if config.exchange.proxy_url.is_empty() {
        anyhow::bail!("PROXY_URL is required (e.g., http://localhost:3053)");
    }

    let mode = if config.trading.dry_run { "DRY-RUN" } else { "LIVE" };
    tracing::info!(
        mode,
        pairs = ?config.trading.pairs,
        proxy_url = config.exchange.proxy_url,
        "Starting Kraken Market Maker"
    );

    let exchange: Arc<dyn ExchangeClient> = Arc::new(ProxyClient::new(
        config.exchange.proxy_url.clone(),
        config.exchange.proxy_token.clone(),
    ));

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

    let logger = CsvTradeLogger::new(&config.persistence.trade_log_file)?;

    // Create engine with fresh state
    let engine = Engine::new(config.clone(), pair_info, BotState::default());

    // Event channel: feeds -> engine
    let (event_tx, event_rx) = mpsc::channel::<EngineEvent>(1024);
    // Command channel: engine -> dispatcher
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(256);

    // State store connection
    let state_store_url = std::env::var("STATE_STORE_URL").unwrap_or_default();
    let state_store_token = std::env::var("STATE_STORE_TOKEN").unwrap_or_default();

    // State store channels:
    // - ss_cmd_rx: state store client -> bot (pair configs, defaults)
    // - ss_snapshot_tx: bot -> state store client (engine snapshots for heartbeat/reports)
    let (ss_cmd_tx, ss_cmd_rx) = mpsc::channel::<StateStoreCommand>(64);
    let (ss_snapshot_tx, ss_snapshot_rx) = mpsc::channel::<EngineSnapshot>(4);

    let ss_handle = if !state_store_url.is_empty() {
        tracing::info!(url = state_store_url, "Connecting to state store");
        let ss_config = StateStoreConfig {
            url: state_store_url,
            token: state_store_token,
            ..Default::default()
        };
        let client = StateStoreClient::new(ss_config, ss_cmd_tx, ss_snapshot_rx);
        Some(tokio::spawn(async move { client.run().await }))
    } else {
        tracing::info!("No STATE_STORE_URL set — running from config file only");
        None
    };

    let result = if config.trading.dry_run {
        run_dry(config, engine, event_tx, event_rx, cmd_tx, cmd_rx, logger, ss_cmd_rx, ss_snapshot_tx).await
    } else {
        run_live(config, engine, event_tx, event_rx, cmd_tx, cmd_rx, logger, exchange, ss_cmd_rx, ss_snapshot_tx).await
    };

    // Abort state store task on shutdown
    if let Some(handle) = ss_handle {
        handle.abort();
    }

    result
}

/// Build an EngineSnapshot from the current engine state, for state store heartbeats/reports.
fn build_engine_snapshot(engine: &Engine) -> EngineSnapshot {
    let pairs = engine.pairs();
    let state = &engine.state;

    let active_pairs = pairs.values()
        .filter(|mp| mp.state == PairState::Active || mp.state == PairState::WindDown)
        .count() as u32;

    // Build per-pair reports
    let pair_reports: Vec<PairReportData> = pairs.values().map(|mp| {
        let pos = state.position(&mp.symbol);
        // Use avg_cost as a rough price proxy if no live price available
        let price_estimate = pos.avg_cost;
        let exposure_usd = pos.qty * price_estimate;
        let quoter_state = match mp.quoter.state {
            QuoteState::Idle => "idle",
            QuoteState::Quoting => "quoting",
            QuoteState::BidFilled => "bid_filled",
            QuoteState::AskFilled => "ask_filled",
        };
        let has_open_orders = mp.quoter.bid_cl_ord_id.is_some() || mp.quoter.ask_cl_ord_id.is_some();
        PairReportData {
            symbol: mp.symbol.clone(),
            position_qty: pos.qty,
            position_avg_cost: pos.avg_cost,
            exposure_usd,
            quoter_state: quoter_state.to_string(),
            has_open_orders,
        }
    }).collect();

    let total_exposure_usd = pair_reports.iter()
        .map(|r| r.exposure_usd)
        .sum();

    EngineSnapshot {
        active_pairs,
        total_exposure_usd,
        pair_reports,
    }
}

/// Convert a StateStoreCommand into an EngineEvent for the engine to process.
fn state_store_cmd_to_event(cmd: StateStoreCommand) -> EngineEvent {
    match cmd {
        StateStoreCommand::Snapshot { pairs, defaults } => {
            EngineEvent::StateStoreCommand(StateStoreAction::Snapshot { pairs, defaults })
        }
        StateStoreCommand::PairUpdated(record) => {
            EngineEvent::StateStoreCommand(StateStoreAction::PairUpdated(record))
        }
        StateStoreCommand::PairRemoved { symbol } => {
            EngineEvent::StateStoreCommand(StateStoreAction::PairRemoved { symbol })
        }
        StateStoreCommand::DefaultsUpdated(defaults) => {
            EngineEvent::StateStoreCommand(StateStoreAction::DefaultsUpdated(defaults))
        }
    }
}

async fn run_dry(
    config: Config,
    engine: Engine,
    event_tx: mpsc::Sender<EngineEvent>,
    event_rx: mpsc::Receiver<EngineEvent>,
    cmd_tx: mpsc::Sender<EngineCommand>,
    mut cmd_rx: mpsc::Receiver<EngineCommand>,
    mut logger: CsvTradeLogger,
    mut ss_cmd_rx: mpsc::Receiver<StateStoreCommand>,
    ss_snapshot_tx: mpsc::Sender<EngineSnapshot>,
) -> Result<()> {
    use kraken_core::exchange::ws::*;
    use kraken_core::exchange::messages::*;
    use chrono::Utc;

    // Connect public WS through proxy
    let base = config.exchange.proxy_url.trim_end_matches('/');
    let ws_base = base
        .replacen("http://", "ws://", 1)
        .replacen("https://", "wss://", 1);
    let pub_ws_url = format!("{}/ws/public", ws_base);
    let mut pub_ws = WsConnection::connect(&pub_ws_url).await?;
    tracing::info!(url = pub_ws_url, "Public WS connected");
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

    // Forward state store commands to the engine via the event channel
    let tx3 = event_tx.clone();
    let ss_forward_handle = tokio::spawn(async move {
        while let Some(cmd) = ss_cmd_rx.recv().await {
            let event = state_store_cmd_to_event(cmd);
            if tx3.send(event).await.is_err() {
                break;
            }
        }
    });

    // Spawn engine with periodic snapshot pushes to state store
    let engine_handle = tokio::spawn(async move {
        let mut engine = engine;
        let mut events = event_rx;
        let mut last_snapshot_push = tokio::time::Instant::now();
        let snapshot_interval = std::time::Duration::from_secs(5);

        while let Some(event) = events.recv().await {
            for cmd in engine.handle_event(event) {
                let is_shutdown = matches!(cmd, EngineCommand::Shutdown { .. });
                if cmd_tx.send(cmd).await.is_err() {
                    return Ok::<(), anyhow::Error>(());
                }
                if is_shutdown {
                    return Ok(());
                }
            }

            // Periodically push engine snapshot to state store client
            if last_snapshot_push.elapsed() >= snapshot_interval {
                let snapshot = build_engine_snapshot(&engine);
                let _ = ss_snapshot_tx.try_send(snapshot);
                last_snapshot_push = tokio::time::Instant::now();
            }
        }
        Ok(())
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
                EngineCommand::PersistState(_) => {}
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
    ss_forward_handle.abort();
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
    engine: Engine,
    event_tx: mpsc::Sender<EngineEvent>,
    event_rx: mpsc::Receiver<EngineEvent>,
    cmd_tx: mpsc::Sender<EngineCommand>,
    mut cmd_rx: mpsc::Receiver<EngineCommand>,
    mut logger: CsvTradeLogger,
    exchange: Arc<dyn ExchangeClient>,
    mut ss_cmd_rx: mpsc::Receiver<StateStoreCommand>,
    ss_snapshot_tx: mpsc::Sender<EngineSnapshot>,
) -> Result<()> {
    let live = LiveExchange::connect(config.clone(), event_tx.clone(), exchange).await?;
    let live = std::sync::Arc::new(live);

    // Clear any ghost orders from previous sessions (one-time cleanup)
    tracing::info!("Cancelling any leftover orders from previous sessions...");
    live.cancel_all().await?;

    // Spawn feeds (private WS read loop already spawned by connect)
    let book_handle = live.spawn_book_feed(event_tx.clone()).await?;
    let ticker_handle = live.spawn_ticker(event_tx.clone(), 10);

    // Forward state store commands to the engine via the event channel
    let tx_ss = event_tx.clone();
    let ss_forward_handle = tokio::spawn(async move {
        while let Some(cmd) = ss_cmd_rx.recv().await {
            let event = state_store_cmd_to_event(cmd);
            if tx_ss.send(event).await.is_err() {
                break;
            }
        }
    });

    // Spawn engine with periodic snapshot pushes to state store
    let engine_handle = tokio::spawn(async move {
        let mut engine = engine;
        let mut events = event_rx;
        let mut last_snapshot_push = tokio::time::Instant::now();
        let snapshot_interval = std::time::Duration::from_secs(5);

        while let Some(event) = events.recv().await {
            for cmd in engine.handle_event(event) {
                let is_shutdown = matches!(cmd, EngineCommand::Shutdown { .. });
                if cmd_tx.send(cmd).await.is_err() {
                    return Ok::<(), anyhow::Error>(());
                }
                if is_shutdown {
                    return Ok(());
                }
            }

            // Periodically push engine snapshot to state store client
            if last_snapshot_push.elapsed() >= snapshot_interval {
                let snapshot = build_engine_snapshot(&engine);
                let _ = ss_snapshot_tx.try_send(snapshot);
                last_snapshot_push = tokio::time::Instant::now();
            }
        }
        Ok(())
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
                    EngineCommand::PersistState(_) => {}
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
    ss_forward_handle.abort();
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
