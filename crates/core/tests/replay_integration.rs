//! Integration tests: replay recorded Kraken WS data through the engine
//! and verify invariants hold across all events.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use tokio::sync::mpsc;

use kraken_core::config::Config;
use kraken_core::engine::core::Engine;
use kraken_core::exchange::messages::{parse_ws_message, WsMessage};
use kraken_core::exchange::replay::ReplaySource;
use kraken_core::state::bot_state::BotState;
use kraken_core::traits::EventSource;
use kraken_core::types::*;

const DATA_PATH: &str = "../../test_data/recorded.jsonl";

fn test_config(pairs: Vec<String>) -> Config {
    let mut cfg = Config::default();
    cfg.trading.pairs = pairs;
    cfg.trading.order_size_usd = dec!(50);
    cfg.trading.min_spread_bps = dec!(100); // 1%
    cfg.trading.maker_fee_pct = dec!(0.0023);
    cfg.trading.spread_capture_pct = dec!(0.50);
    cfg.trading.requote_threshold_pct = dec!(0.005);
    cfg.risk.max_inventory_usd = dec!(200);
    cfg.risk.max_total_exposure_usd = dec!(2000);
    cfg.risk.kill_switch_loss_usd = dec!(-100);
    cfg.risk.stale_order_secs = 300;
    cfg
}

fn test_pair_info(symbol: &str) -> PairInfo {
    PairInfo {
        symbol: symbol.to_string(),
        rest_key: symbol.replace('/', ""),
        min_order_qty: dec!(1),
        min_cost: dec!(0.5),
        price_decimals: 7,
        qty_decimals: 5,
        maker_fee_pct: dec!(0.0023),
        base_asset: symbol.split('/').next().unwrap_or("").to_string(),
    }
}

fn default_pairs() -> Vec<String> {
    vec![
        "CAMP/USD".to_string(),
        "SUP/USD".to_string(),
        "MIM/USD".to_string(),
    ]
}

fn default_pair_info_map() -> HashMap<String, PairInfo> {
    let mut m = HashMap::new();
    for p in &default_pairs() {
        m.insert(p.clone(), test_pair_info(p));
    }
    m
}

#[derive(serde::Deserialize)]
struct RecordedMessage {
    timestamp: DateTime<Utc>,
    raw: String,
}

/// Synchronously replay recorded data through the engine.
/// Returns (engine, commands) so caller can inspect both.
fn run_sync_replay(
    path: &str,
    config: Config,
    pair_info: HashMap<String, PairInfo>,
    state: BotState,
) -> (Engine, Vec<EngineCommand>) {
    let mut engine = Engine::new(config, pair_info, state);
    let mut commands = vec![];

    let file = File::open(path).unwrap();
    let reader = BufReader::new(file);

    for line in reader.lines() {
        let line = line.unwrap();
        if line.trim().is_empty() {
            continue;
        }
        let recorded: RecordedMessage = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue,
        };

        let msg = parse_ws_message(&recorded.raw);
        let event = match msg {
            WsMessage::BookSnapshot { symbol, bids, asks } => {
                Some(EngineEvent::BookSnapshot {
                    symbol,
                    bids,
                    asks,
                    timestamp: recorded.timestamp,
                })
            }
            WsMessage::BookUpdate {
                symbol,
                bid_updates,
                ask_updates,
            } => Some(EngineEvent::BookUpdate {
                symbol,
                bid_updates,
                ask_updates,
                timestamp: recorded.timestamp,
            }),
            _ => None,
        };

        if let Some(e) = event {
            let cmds = engine.handle_event(e);
            commands.extend(cmds);
        }
    }

    (engine, commands)
}

/// Extract all PlaceOrder commands from a command list.
fn placed_orders(cmds: &[EngineCommand]) -> Vec<&OrderRequest> {
    cmds.iter()
        .filter_map(|c| match c {
            EngineCommand::PlaceOrder(req) => Some(req),
            _ => None,
        })
        .collect()
}

// ============================================================================
// Tests
// ============================================================================

/// Core replay test: events parse correctly, orders are placed, books populated.
#[tokio::test]
async fn replay_async_through_channels() {
    if !std::path::Path::new(DATA_PATH).exists() {
        eprintln!("Skipping — no recorded data at {DATA_PATH}");
        return;
    }

    let config = test_config(default_pairs());
    let mut engine = Engine::new(config, default_pair_info_map(), BotState::default());

    let (tx, mut rx) = mpsc::channel::<EngineEvent>(4096);
    let mut source = ReplaySource::instant(DATA_PATH);
    let feed_handle = tokio::spawn(async move {
        source.run(tx).await.unwrap();
    });

    let mut all_commands: Vec<EngineCommand> = vec![];
    let mut event_count = 0u64;
    let mut snapshot_count = 0u64;

    while let Some(event) = rx.recv().await {
        if matches!(&event, EngineEvent::BookSnapshot { .. }) {
            snapshot_count += 1;
        }
        event_count += 1;
        let cmds = engine.handle_event(event);
        all_commands.extend(cmds);
    }
    feed_handle.await.unwrap();

    assert!(event_count > 10, "Expected >10 events, got {event_count}");
    assert!(
        snapshot_count >= 3,
        "Expected snapshot per pair, got {snapshot_count}"
    );

    let orders = placed_orders(&all_commands);
    assert!(!orders.is_empty(), "Expected at least one PlaceOrder");

    // Verify book state
    for symbol in &["CAMP/USD", "SUP/USD", "MIM/USD"] {
        let book = engine.books().get(*symbol).expect("Book should exist");
        let (bid, _) = book.best_bid().expect("Should have bids");
        let (ask, _) = book.best_ask().expect("Should have asks");
        assert!(bid < ask, "{symbol} bid {bid} >= ask {ask}");
    }
}

/// No sell orders should be placed when the engine starts with zero inventory.
#[test]
fn no_sells_without_inventory() {
    if !std::path::Path::new(DATA_PATH).exists() {
        return;
    }

    let config = test_config(default_pairs());
    let (_, commands) = run_sync_replay(DATA_PATH, config, default_pair_info_map(), BotState::default());

    let orders = placed_orders(&commands);
    let sell_orders: Vec<_> = orders.iter().filter(|o| o.side == OrderSide::Sell).collect();

    assert!(
        sell_orders.is_empty(),
        "Should NOT place sell orders with zero inventory. Got {} sells: {:?}",
        sell_orders.len(),
        sell_orders
            .iter()
            .map(|o| format!("{} {} @ {}", o.symbol, o.side, o.price))
            .collect::<Vec<_>>()
    );
}

/// All placed orders should be buy-only on cold start (no inventory).
/// Verify each buy has valid price, qty, and doesn't cross the book.
#[test]
fn cold_start_only_buys_with_valid_prices() {
    if !std::path::Path::new(DATA_PATH).exists() {
        return;
    }

    let config = test_config(default_pairs());
    let (engine, commands) = run_sync_replay(DATA_PATH, config, default_pair_info_map(), BotState::default());

    let orders = placed_orders(&commands);
    assert!(!orders.is_empty(), "Expected some orders");

    for order in &orders {
        // All orders should be buys (no inventory to sell)
        assert_eq!(
            order.side,
            OrderSide::Buy,
            "Cold start should only place buys, got {} for {}",
            order.side,
            order.symbol
        );

        // Valid price and qty
        assert!(order.price > Decimal::ZERO, "Price must be > 0: {:?}", order);
        assert!(order.qty > Decimal::ZERO, "Qty must be > 0: {:?}", order);
        assert!(!order.symbol.is_empty(), "Symbol must not be empty");
        assert!(!order.cl_ord_id.is_empty(), "cl_ord_id must not be empty");

        // Bid should be below best ask (post-only safe)
        if let Some(book) = engine.books().get(&order.symbol) {
            if let Some((best_ask, _)) = book.best_ask() {
                assert!(
                    order.price < best_ask,
                    "{} bid {} would cross best ask {}",
                    order.symbol,
                    order.price,
                    best_ask
                );
            }
        }
    }

    // Should have orders for each pair (all 3 have wide spreads)
    let symbols_with_orders: std::collections::HashSet<_> =
        orders.iter().map(|o| o.symbol.as_str()).collect();
    for pair in &["CAMP/USD", "SUP/USD", "MIM/USD"] {
        assert!(
            symbols_with_orders.contains(pair),
            "Expected orders for {pair}, only got: {:?}",
            symbols_with_orders
        );
    }
}

/// When the engine has inventory, it should place sell orders.
#[test]
fn with_inventory_places_sells() {
    if !std::path::Path::new(DATA_PATH).exists() {
        return;
    }

    // Pre-load modest positions for all 3 pairs (under max_inventory_usd)
    let mut state = BotState::default();
    for symbol in &["CAMP/USD", "SUP/USD", "MIM/USD"] {
        let mut pos = Position::default();
        pos.apply_buy(dec!(50000), dec!(0.001)); // $50 worth at current prices
        state.positions.insert(symbol.to_string(), pos);
    }

    let mut config = test_config(default_pairs());
    config.risk.max_inventory_usd = dec!(500); // Allow room for both buys and sells
    let (_, commands) = run_sync_replay(DATA_PATH, config, default_pair_info_map(), state);

    let orders = placed_orders(&commands);
    let sell_orders: Vec<_> = orders.iter().filter(|o| o.side == OrderSide::Sell).collect();
    let buy_orders: Vec<_> = orders.iter().filter(|o| o.side == OrderSide::Buy).collect();

    assert!(
        !sell_orders.is_empty(),
        "With inventory, should place sell orders"
    );
    assert!(
        !buy_orders.is_empty(),
        "With inventory below max, should still place buy orders"
    );

    // All sell prices should be above all buy prices for the same symbol
    for sell in &sell_orders {
        for buy in &buy_orders {
            if sell.symbol == buy.symbol {
                assert!(
                    sell.price > buy.price,
                    "{}: sell {} should be > buy {}",
                    sell.symbol,
                    sell.price,
                    buy.price
                );
            }
        }
    }
}

/// Determinism: same data replayed twice produces identical commands.
#[test]
fn replay_is_deterministic() {
    if !std::path::Path::new(DATA_PATH).exists() {
        return;
    }

    let config = test_config(default_pairs());
    let (_, cmds1) =
        run_sync_replay(DATA_PATH, config.clone(), default_pair_info_map(), BotState::default());
    let (_, cmds2) =
        run_sync_replay(DATA_PATH, config, default_pair_info_map(), BotState::default());

    assert_eq!(cmds1.len(), cmds2.len(), "Command count differs");

    for (i, (c1, c2)) in cmds1.iter().zip(cmds2.iter()).enumerate() {
        match (c1, c2) {
            (EngineCommand::PlaceOrder(r1), EngineCommand::PlaceOrder(r2)) => {
                assert_eq!(r1.symbol, r2.symbol, "Cmd {i}: symbol");
                assert_eq!(r1.side, r2.side, "Cmd {i}: side");
                assert_eq!(r1.price, r2.price, "Cmd {i}: price");
                assert_eq!(r1.qty, r2.qty, "Cmd {i}: qty");
                assert_eq!(r1.cl_ord_id, r2.cl_ord_id, "Cmd {i}: cl_ord_id");
            }
            (
                EngineCommand::AmendOrder {
                    cl_ord_id: id1,
                    new_price: p1,
                    ..
                },
                EngineCommand::AmendOrder {
                    cl_ord_id: id2,
                    new_price: p2,
                    ..
                },
            ) => {
                assert_eq!(id1, id2, "Cmd {i}: amend id");
                assert_eq!(p1, p2, "Cmd {i}: amend price");
            }
            _ => {
                assert_eq!(
                    std::mem::discriminant(c1),
                    std::mem::discriminant(c2),
                    "Cmd {i}: variant mismatch"
                );
            }
        }
    }
}

/// No orders placed when min_spread_bps is impossibly high.
#[test]
fn narrow_spread_filter_blocks_all_orders() {
    if !std::path::Path::new(DATA_PATH).exists() {
        return;
    }

    let mut config = test_config(default_pairs());
    config.trading.min_spread_bps = dec!(5000); // 50%

    let (_, commands) = run_sync_replay(DATA_PATH, config, default_pair_info_map(), BotState::default());

    let orders = placed_orders(&commands);
    assert!(
        orders.is_empty(),
        "No orders should pass 50% min spread filter, got {}",
        orders.len()
    );
}

/// No kill switch triggered on pure book data (no fills = no P&L).
#[test]
fn no_shutdown_on_book_data() {
    if !std::path::Path::new(DATA_PATH).exists() {
        return;
    }

    let config = test_config(default_pairs());
    let (_, commands) = run_sync_replay(DATA_PATH, config, default_pair_info_map(), BotState::default());

    let shutdowns: Vec<_> = commands
        .iter()
        .filter(|c| matches!(c, EngineCommand::Shutdown { .. }))
        .collect();
    assert!(
        shutdowns.is_empty(),
        "Kill switch should not fire on pure book data"
    );

    let cancel_alls: Vec<_> = commands
        .iter()
        .filter(|c| matches!(c, EngineCommand::CancelAll))
        .collect();
    assert!(
        cancel_alls.is_empty(),
        "CancelAll should not fire on pure book data"
    );
}

/// Amendments should only reference cl_ord_ids that were previously placed.
#[test]
fn amend_references_valid_order_ids() {
    if !std::path::Path::new(DATA_PATH).exists() {
        return;
    }

    let config = test_config(default_pairs());
    let (_, commands) = run_sync_replay(DATA_PATH, config, default_pair_info_map(), BotState::default());

    let mut placed_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for cmd in &commands {
        match cmd {
            EngineCommand::PlaceOrder(req) => {
                placed_ids.insert(req.cl_ord_id.clone());
            }
            EngineCommand::AmendOrder { cl_ord_id, .. } => {
                assert!(
                    placed_ids.contains(cl_ord_id),
                    "Amend references unknown cl_ord_id '{cl_ord_id}'. Known: {:?}",
                    placed_ids
                );
            }
            _ => {}
        }
    }
}

/// Order quantities meet minimum order requirements.
#[test]
fn order_qty_meets_minimums() {
    if !std::path::Path::new(DATA_PATH).exists() {
        return;
    }

    let pair_info = default_pair_info_map();
    let config = test_config(default_pairs());
    let (_, commands) = run_sync_replay(DATA_PATH, config, pair_info.clone(), BotState::default());

    for cmd in &commands {
        if let EngineCommand::PlaceOrder(req) = cmd {
            if let Some(info) = pair_info.get(&req.symbol) {
                assert!(
                    req.qty >= info.min_order_qty,
                    "{}: qty {} < min {}",
                    req.symbol,
                    req.qty,
                    info.min_order_qty
                );
            }
        }
    }
}
