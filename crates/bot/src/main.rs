use anyhow::Result;
use clap::Parser;
use std::collections::HashSet;
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
use kraken_core::types::Ticker;

#[derive(Parser)]
#[command(name = "kraken-mm", about = "Low-liquidity market making bot (exchange-agnostic via proxy)")]
struct Args {
    /// Paper trading mode (logs orders, doesn't send them)
    #[arg(long)]
    dry_run: bool,

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

    if config.exchange.proxy_url.is_empty() {
        anyhow::bail!("PROXY_URL is required (e.g., http://localhost:3053)");
    }

    let state_store_url = std::env::var("STATE_STORE_URL").unwrap_or_default();
    let state_store_token = std::env::var("STATE_STORE_TOKEN").unwrap_or_default();

    if state_store_url.is_empty() {
        anyhow::bail!("STATE_STORE_URL is required (e.g., http://localhost:3040)");
    }

    let mode = if config.trading.dry_run { "DRY-RUN" } else { "LIVE" };
    tracing::info!(
        mode,
        proxy_url = config.exchange.proxy_url,
        state_store_url,
        "Starting Market Maker"
    );

    // State store channels:
    // - ss_cmd_rx: state store client -> bot (pair configs, defaults)
    // - ss_snapshot_tx: bot -> state store client (engine snapshots for heartbeat/reports)
    let (ss_cmd_tx, ss_cmd_rx) = mpsc::channel::<StateStoreCommand>(64);
    let (ss_snapshot_tx, ss_snapshot_rx) = mpsc::channel::<EngineSnapshot>(4);

    tracing::info!(url = state_store_url, "Connecting to state store");
    let ss_config = StateStoreConfig {
        url: state_store_url,
        token: state_store_token,
        ..Default::default()
    };
    let client = StateStoreClient::new(ss_config, ss_cmd_tx, ss_snapshot_rx);
    let ss_handle = tokio::spawn(async move { client.run().await });

    // Wait for initial snapshot from state store to get pair list
    let (initial_snapshot, ss_cmd_rx) = wait_for_snapshot(ss_cmd_rx).await?;

    let pairs: Vec<Ticker> = initial_snapshot.pairs.iter()
        .map(|p| p.pair.clone())
        .collect();

    if pairs.is_empty() {
        tracing::info!("No pairs in state store yet — will idle until pairs are added");
    } else {
        tracing::info!(?pairs, "Got {} pairs from state store", pairs.len());
    }

    let proxy_client = ProxyClient::new(
        config.exchange.proxy_url.clone(),
        config.exchange.proxy_token.clone(),
    );
    let proxy_client = Arc::new(proxy_client);

    let exchange: Arc<dyn ExchangeClient> = proxy_client.clone();

    // Fetch pair info for state store pairs (skip if empty — pairs will be fetched dynamically)
    let pair_info = if pairs.is_empty() {
        std::collections::HashMap::new()
    } else {
        tracing::info!("Fetching pair info...");
        let info = exchange.get_pair_info(&pairs).await?;
        for (pair, pi) in &info {
            tracing::info!(
                pair = %pair,
                min_order = %pi.min_order_qty,
                price_dec = pi.price_decimals,
                qty_dec = pi.qty_decimals,
                cost_min = %pi.min_cost,
                "Pair info"
            );
        }
        info
    };

    let logger = CsvTradeLogger::new(&config.persistence.trade_log_file)?;

    // Create engine and apply the initial state store snapshot
    let mut engine = Engine::new(config.clone(), pair_info, BotState::default());
    let snapshot_event = state_store_cmd_to_event(StateStoreCommand::Snapshot {
        pairs: initial_snapshot.pairs,
        defaults: initial_snapshot.defaults,
    });
    engine.handle_event(snapshot_event);

    // Restore positions: try proxy-tracked positions first (preserves avg_cost),
    // fall back to raw exchange balances (loses avg_cost).
    if !pairs.is_empty() {
        match proxy_client.get_proxy_positions().await {
            Ok(positions) if !positions.is_empty() => {
                tracing::info!(count = positions.len(), "Restoring positions from proxy");
                engine.restore_positions_from_proxy(&positions);
            }
            Ok(_) => {
                tracing::info!("No proxy positions — falling back to exchange balances");
                match exchange.get_balances().await {
                    Ok(balances) => engine.restore_balances(&balances),
                    Err(e) => tracing::warn!(error = %e, "Failed to fetch balances — positions may be stale"),
                }
            }
            Err(e) => {
                tracing::info!(error = %e, "Proxy /positions not available — falling back to exchange balances");
                match exchange.get_balances().await {
                    Ok(balances) => engine.restore_balances(&balances),
                    Err(e) => tracing::warn!(error = %e, "Failed to fetch balances — positions may be stale"),
                }
            }
        }
    }

    // Event channel: feeds -> engine
    let (event_tx, event_rx) = mpsc::channel::<EngineEvent>(1024);
    // Command channel: engine -> dispatcher
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(256);

    let result = if config.trading.dry_run {
        run_dry(config, engine, pairs, event_tx, event_rx, cmd_tx, cmd_rx, logger, exchange, ss_cmd_rx, ss_snapshot_tx).await
    } else {
        run_live(config, engine, pairs, event_tx, event_rx, cmd_tx, cmd_rx, logger, exchange, ss_cmd_rx, ss_snapshot_tx).await
    };

    // Abort state store task on shutdown
    ss_handle.abort();

    result
}

/// Holds the initial snapshot data from the state store.
struct InitialSnapshot {
    pairs: Vec<kraken_core::state_store::messages::PairRecord>,
    defaults: kraken_core::types::GlobalDefaults,
}

/// Wait for the first Snapshot command from the state store.
/// Returns the snapshot data and the receiver (for continued use).
async fn wait_for_snapshot(
    mut rx: mpsc::Receiver<StateStoreCommand>,
) -> anyhow::Result<(InitialSnapshot, mpsc::Receiver<StateStoreCommand>)> {
    tracing::info!("Waiting for initial snapshot from state store...");
    let timeout = std::time::Duration::from_secs(30);
    match tokio::time::timeout(timeout, async {
        while let Some(cmd) = rx.recv().await {
            if let StateStoreCommand::Snapshot { pairs, defaults } = cmd {
                return Ok(InitialSnapshot { pairs, defaults });
            }
            // Ignore non-snapshot messages during startup
        }
        anyhow::bail!("State store channel closed before snapshot received")
    }).await {
        Ok(result) => result.map(|snap| (snap, rx)),
        Err(_) => anyhow::bail!("Timed out waiting for state store snapshot ({}s)", timeout.as_secs()),
    }
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
        let pos = state.position(&mp.pair);
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
            pair: mp.pair.clone(),
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
        StateStoreCommand::PairRemoved { pair } => {
            EngineEvent::StateStoreCommand(StateStoreAction::PairRemoved { pair })
        }
        StateStoreCommand::DefaultsUpdated(defaults) => {
            EngineEvent::StateStoreCommand(StateStoreAction::DefaultsUpdated(defaults))
        }
    }
}

async fn run_dry(
    config: Config,
    engine: Engine,
    pairs: Vec<Ticker>,
    event_tx: mpsc::Sender<EngineEvent>,
    event_rx: mpsc::Receiver<EngineEvent>,
    cmd_tx: mpsc::Sender<EngineCommand>,
    mut cmd_rx: mpsc::Receiver<EngineCommand>,
    mut logger: CsvTradeLogger,
    exchange: Arc<dyn ExchangeClient>,
    mut ss_cmd_rx: mpsc::Receiver<StateStoreCommand>,
    ss_snapshot_tx: mpsc::Sender<EngineSnapshot>,
) -> Result<()> {
    use kraken_core::exchange::ws::*;
    use exchange_api::{ProxyCommand, ProxyEvent, parse_proxy_event};
    use trading_primitives::book::LevelUpdate;
    use chrono::Utc;

    // Connect public WS through proxy
    let base = config.exchange.proxy_url.trim_end_matches('/');
    let ws_base = base
        .replacen("http://", "ws://", 1)
        .replacen("https://", "wss://", 1);
    let pub_ws_url = format!("{}/ws/public", ws_base);
    let pub_ws = WsConnection::connect_with_token(&pub_ws_url, &config.exchange.proxy_token).await?;
    tracing::info!(url = pub_ws_url, "Public WS connected");
    let (mut pub_writer, mut pub_reader) = pub_ws.into_split();
    let depth = config.exchange.book_depth;

    // Subscribe to initial pairs
    let pair_strings: Vec<String> = pairs.iter().map(|t| t.to_string()).collect();
    if !pairs.is_empty() {
        let sub_msg = serde_json::to_string(&ProxyCommand::Subscribe {
            channel: "book".to_string(),
            symbols: pair_strings.clone(),
            depth: Some(depth),
        })?;
        pub_writer.send_raw(&sub_msg).await?;
    }

    // Channel for dynamic book subscriptions
    let (book_sub_tx, mut book_sub_rx) = mpsc::channel::<Vec<String>>(16);

    // Spawn book feed reader with dynamic subscription support
    let tx = event_tx.clone();
    let book_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                raw = pub_reader.recv() => {
                    match raw {
                        Some(text) => {
                            let event = match parse_proxy_event(&text) {
                                Ok(e) => e,
                                Err(_) => continue,
                            };
                            let engine_event = match event {
                                ProxyEvent::BookSnapshot { symbol, bids, asks } => {
                                    let pair = trading_primitives::Ticker::from(symbol.as_str());
                                    Some(EngineEvent::BookSnapshot {
                                        pair,
                                        bids: bids.into_iter().map(|l| LevelUpdate { price: l.price, qty: l.qty }).collect(),
                                        asks: asks.into_iter().map(|l| LevelUpdate { price: l.price, qty: l.qty }).collect(),
                                        timestamp: Utc::now(),
                                    })
                                }
                                ProxyEvent::BookUpdate { symbol, bids, asks } => {
                                    let pair = trading_primitives::Ticker::from(symbol.as_str());
                                    Some(EngineEvent::BookUpdate {
                                        pair,
                                        bid_updates: bids.into_iter().map(|l| LevelUpdate { price: l.price, qty: l.qty }).collect(),
                                        ask_updates: asks.into_iter().map(|l| LevelUpdate { price: l.price, qty: l.qty }).collect(),
                                        timestamp: Utc::now(),
                                    })
                                }
                                ProxyEvent::Subscribed { channel } => {
                                    tracing::info!(channel, "Subscription confirmed");
                                    None
                                }
                                _ => None,
                            };
                            if let Some(e) = engine_event {
                                if tx.send(e).await.is_err() { return; }
                            }
                        }
                        None => return, // WS closed
                    }
                }
                new_pairs = book_sub_rx.recv() => {
                    if let Some(new_pairs) = new_pairs {
                        if !new_pairs.is_empty() {
                            tracing::info!(?new_pairs, "Subscribing to new pairs");
                            let sub_msg = serde_json::to_string(&ProxyCommand::Subscribe {
                                channel: "book".to_string(),
                                symbols: new_pairs,
                                depth: Some(depth),
                            }).unwrap();
                            if let Err(e) = pub_writer.send_raw(&sub_msg).await {
                                tracing::error!(error = %e, "Failed to subscribe to new pairs");
                            }
                        }
                    }
                }
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

    // Forward state store commands to the engine, enriching new pairs with pair_info
    let tx3 = event_tx.clone();
    let ss_forward_handle = tokio::spawn(async move {
        let mut known_pairs: HashSet<Ticker> = pairs.into_iter().collect();

        while let Some(cmd) = ss_cmd_rx.recv().await {
            // Extract new pairs from Snapshot or PairUpdated
            let new_pairs: Vec<Ticker> = match &cmd {
                StateStoreCommand::Snapshot { pairs, .. } => {
                    pairs.iter()
                        .filter(|p| !known_pairs.contains(&p.pair))
                        .map(|p| p.pair.clone())
                        .collect()
                }
                StateStoreCommand::PairUpdated(record) => {
                    if known_pairs.contains(&record.pair) {
                        vec![]
                    } else {
                        vec![record.pair.clone()]
                    }
                }
                _ => vec![],
            };

            // Forward the command to the engine as-is
            let event = state_store_cmd_to_event(cmd);
            if tx3.send(event).await.is_err() {
                break;
            }

            // For new pairs: fetch pair_info, send to engine, subscribe to book
            if !new_pairs.is_empty() {
                tracing::info!(?new_pairs, "New pairs discovered — fetching pair info");
                match exchange.get_pair_info(&new_pairs).await {
                    Ok(info) => {
                        if !info.is_empty() {
                            for pair in info.keys() {
                                tracing::info!(pair = %pair, "Pair info fetched for new pair");
                            }
                            let subscribed: Vec<Ticker> = info.keys().cloned().collect();
                            let subscribed_strings: Vec<String> = subscribed.iter().map(|t| t.to_string()).collect();

                            // Send PairInfoFetched to engine
                            let pi_event = EngineEvent::PairInfoFetched { info };
                            if tx3.send(pi_event).await.is_err() {
                                break;
                            }

                            // Subscribe to book data for new pairs
                            if let Err(e) = book_sub_tx.send(subscribed_strings).await {
                                tracing::error!(error = %e, "Failed to send book subscription");
                            }

                            known_pairs.extend(subscribed);
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, ?new_pairs, "Failed to fetch pair info for new pairs");
                    }
                }
                // Track as known even if pair_info fetch failed
                known_pairs.extend(new_pairs);
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
                        side = %req.side, pair = %req.pair,
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
    pairs: Vec<Ticker>,
    event_tx: mpsc::Sender<EngineEvent>,
    event_rx: mpsc::Receiver<EngineEvent>,
    cmd_tx: mpsc::Sender<EngineCommand>,
    mut cmd_rx: mpsc::Receiver<EngineCommand>,
    mut logger: CsvTradeLogger,
    exchange: Arc<dyn ExchangeClient>,
    mut ss_cmd_rx: mpsc::Receiver<StateStoreCommand>,
    ss_snapshot_tx: mpsc::Sender<EngineSnapshot>,
) -> Result<()> {
    let live = LiveExchange::connect(config.clone(), event_tx.clone(), exchange.clone()).await?;
    let live = std::sync::Arc::new(live);

    // Clear any ghost orders from previous sessions (one-time cleanup)
    tracing::info!("Cancelling any leftover orders from previous sessions...");
    live.cancel_all().await?;

    // Channel for dynamic book subscriptions (new pairs discovered via state store)
    let (book_sub_tx, book_sub_rx) = mpsc::channel::<Vec<String>>(16);

    // Spawn feeds (private WS read loop already spawned by connect)
    let pair_strings: Vec<String> = pairs.iter().map(|t| t.to_string()).collect();
    let book_handle = live.spawn_book_feed(event_tx.clone(), &pair_strings, Some(book_sub_rx)).await?;
    let ticker_handle = live.spawn_ticker(event_tx.clone(), 10);

    // Forward state store commands to the engine, enriching new pairs with pair_info
    let tx_ss = event_tx.clone();
    let ss_forward_handle = tokio::spawn(async move {
        let mut known_pairs: HashSet<Ticker> = pairs.into_iter().collect();

        while let Some(cmd) = ss_cmd_rx.recv().await {
            // Extract new pairs from Snapshot or PairUpdated
            let new_pairs: Vec<Ticker> = match &cmd {
                StateStoreCommand::Snapshot { pairs, .. } => {
                    pairs.iter()
                        .filter(|p| !known_pairs.contains(&p.pair))
                        .map(|p| p.pair.clone())
                        .collect()
                }
                StateStoreCommand::PairUpdated(record) => {
                    if known_pairs.contains(&record.pair) {
                        vec![]
                    } else {
                        vec![record.pair.clone()]
                    }
                }
                _ => vec![],
            };

            // Forward the command to the engine as-is
            let event = state_store_cmd_to_event(cmd);
            if tx_ss.send(event).await.is_err() {
                break;
            }

            // For new pairs: fetch pair_info, send to engine, subscribe to book
            if !new_pairs.is_empty() {
                tracing::info!(?new_pairs, "New pairs discovered — fetching pair info");
                match exchange.get_pair_info(&new_pairs).await {
                    Ok(info) => {
                        if !info.is_empty() {
                            for pair in info.keys() {
                                tracing::info!(pair = %pair, "Pair info fetched for new pair");
                            }
                            let subscribed: Vec<Ticker> = info.keys().cloned().collect();
                            let subscribed_strings: Vec<String> = subscribed.iter().map(|t| t.to_string()).collect();

                            // Send PairInfoFetched to engine
                            let pi_event = EngineEvent::PairInfoFetched { info };
                            if tx_ss.send(pi_event).await.is_err() {
                                break;
                            }

                            // Subscribe to book data for new pairs
                            if let Err(e) = book_sub_tx.send(subscribed_strings).await {
                                tracing::error!(error = %e, "Failed to send book subscription");
                            }

                            known_pairs.extend(subscribed);
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, ?new_pairs, "Failed to fetch pair info for new pairs");
                    }
                }
                // Track as known even if pair_info fetch failed, to avoid retrying every message
                known_pairs.extend(new_pairs);
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
                            pair = %req.pair,
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
