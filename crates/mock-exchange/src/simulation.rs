use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, RwLock};

use crate::messages;
use crate::orders::{OrderStatus, Side};
use crate::scenario::ScenarioFile;
use crate::state::ExchangeState;

/// Run the simulation loop: periodically walk prices and check for resting order fills.
pub async fn run_simulation(
    state: Arc<RwLock<ExchangeState>>,
    book_tx: broadcast::Sender<String>,
    exec_tx: broadcast::Sender<String>,
) {
    let (interval_ms, volatility, fill_prob, seed, scenario) = {
        let st = state.read().await;
        (
            st.config.update_interval_ms,
            st.config.volatility,
            st.config.fill_probability,
            st.config.seed,
            st.config.scenario.clone(),
        )
    };

    if let Some(ref sc) = scenario {
        tracing::info!(
            name = sc.name,
            pairs = sc.pairs.len(),
            end_behavior = ?sc.end_behavior,
            "Scenario mode active"
        );
    }

    let mut rng: StdRng = match seed {
        Some(s) => StdRng::seed_from_u64(s),
        None => StdRng::from_entropy(),
    };

    let scenario_start = std::time::Instant::now();

    let mut tick_interval = tokio::time::interval(Duration::from_millis(interval_ms));
    let mut heartbeat_interval = tokio::time::interval(Duration::from_secs(5));

    loop {
        tokio::select! {
            _ = tick_interval.tick() => {
                let elapsed_ms = scenario_start.elapsed().as_millis() as u64;
                do_book_tick(
                    &state, &book_tx, &exec_tx,
                    volatility, fill_prob, &mut rng,
                    scenario.as_ref(), elapsed_ms,
                ).await;
            }
            _ = heartbeat_interval.tick() => {
                let hb = serde_json::to_string(&messages::heartbeat()).unwrap();
                let _ = book_tx.send(hb.clone());
                let _ = exec_tx.send(hb);
            }
        }
    }
}

/// One tick of the simulation: update books, check for fills.
async fn do_book_tick(
    state: &Arc<RwLock<ExchangeState>>,
    book_tx: &broadcast::Sender<String>,
    exec_tx: &broadcast::Sender<String>,
    volatility: f64,
    fill_probability: f64,
    rng: &mut StdRng,
    scenario: Option<&ScenarioFile>,
    elapsed_ms: u64,
) {
    let mut st = state.write().await;

    // Collect symbols to iterate (avoid borrow issues)
    let symbols: Vec<String> = st.books.keys().cloned().collect();

    for symbol in &symbols {
        // Drive the book: either from scenario or random walk
        let effective_fill_prob = if let Some(book) = st.books.get_mut(symbol) {
            if let Some(sc) = scenario {
                if let Some(pair_scenario) = sc.pairs.get(symbol.as_str()) {
                    // Deterministic mode for this pair
                    let defaults = sc.defaults.clone().unwrap_or_default();
                    let end = sc.end_behavior.unwrap_or_default();
                    let sample = pair_scenario.sample_at(elapsed_ms, &defaults, end);

                    if !sample.finished {
                        book.set_mid(sample.mid, sample.spread_pct);
                    }
                    // Use per-segment fill probability, fall back to global
                    sample.fill_probability.unwrap_or(fill_probability)
                } else {
                    // Pair not in scenario — random walk
                    book.random_walk(volatility, rng);
                    fill_probability
                }
            } else {
                // No scenario loaded — random walk (original behavior)
                book.random_walk(volatility, rng);
                fill_probability
            }
        } else {
            continue;
        };

        // Send full book snapshot on every tick instead of an incremental
        // update.  Because `regenerate_levels` creates an entirely new set
        // of price levels, incremental updates would leave stale levels in
        // the bot's order book (the mock never sends qty=0 removals).
        // Sending a snapshot lets the bot clear and replace its book each
        // tick, keeping it in sync with the mock.
        if let Some(book) = st.books.get(symbol) {
            let update = messages::book_snapshot(symbol, book);
            let msg = serde_json::to_string(&update).unwrap();
            let _ = book_tx.send(msg);
        }

        // Check resting orders for fills.
        // Use rounded prices (same precision as book update messages) so that
        // the fill logic is consistent with what the bot sees.
        let (best_bid, best_ask) = {
            let book = st.books.get(symbol).unwrap();
            let pd = crate::state::price_decimals_for(book.mid_price());
            let factor = 10f64.powi(pd as i32);
            let round = |v: f64| (v * factor).round() / factor;
            (
                round(book.best_bid().unwrap_or(0.0)),
                round(book.best_ask().unwrap_or(0.0)),
            )
        };

        // Collect orders that might fill
        let fillable: Vec<String> = st
            .orders
            .orders
            .values()
            .filter(|o| {
                o.symbol == *symbol
                    && o.status == OrderStatus::New
                    && o.filled_qty == 0.0
            })
            .filter(|o| {
                match o.side {
                    // Buy order: fills when ask drops to or below order price
                    Side::Buy => best_ask > 0.0 && best_ask <= o.price,
                    // Sell order: fills when bid rises to or above order price
                    Side::Sell => best_bid > 0.0 && best_bid >= o.price,
                }
            })
            .map(|o| o.cl_ord_id.clone())
            .collect();

        // Also check probabilistic fills for orders near the spread
        let probabilistic: Vec<String> = st
            .orders
            .orders
            .values()
            .filter(|o| {
                o.symbol == *symbol
                    && o.status == OrderStatus::New
                    && o.filled_qty == 0.0
            })
            .filter(|o| {
                // Only try probabilistic fill if the order didn't cross
                !fillable.contains(&o.cl_ord_id)
            })
            .filter(|o| {
                // Check if order is within a reasonable range of the spread.
                // Use 5% range so probabilistic fills trigger for typical
                // market-making spreads (bot places bids ~1-2% inside the book).
                match o.side {
                    Side::Buy => {
                        best_ask > 0.0 && o.price >= best_ask * 0.95
                    }
                    Side::Sell => {
                        best_bid > 0.0 && o.price <= best_bid * 1.05
                    }
                }
            })
            .filter(|_| rng.gen::<f64>() < effective_fill_prob)
            .map(|o| o.cl_ord_id.clone())
            .collect();

        // Execute all fills
        let all_fills: Vec<String> = fillable
            .into_iter()
            .chain(probabilistic.into_iter())
            .collect();

        for cl_ord_id in all_fills {
            if let Some(order) = st.orders.get_mut(&cl_ord_id) {
                let fill_qty = order.remaining_qty();
                let fill_price = match order.side {
                    Side::Buy => best_ask.min(order.price),
                    Side::Sell => best_bid.max(order.price),
                };

                // Maker fill (resting order)
                let maker_fee_pct = 0.26 / 100.0;
                let cost = fill_price * fill_qty;
                let fee = cost * maker_fee_pct;

                order.filled_qty += fill_qty;
                order.status = OrderStatus::Filled;

                let order_clone = order.clone();
                let side = order.side;

                // Update balances
                crate::state::update_balances(
                    &mut st.balances,
                    symbol,
                    side,
                    fill_qty,
                    fill_price,
                    fee,
                );

                // Send trade execution report
                let trade =
                    messages::exec_report("trade", &order_clone, fill_qty, fill_price, fee, true);
                let msg = serde_json::to_string(&trade).unwrap();
                let _ = exec_tx.send(msg);

                tracing::info!(
                    cl_ord_id = cl_ord_id,
                    symbol = symbol,
                    side = %side,
                    price = fill_price,
                    qty = fill_qty,
                    fee = fee,
                    "Resting order filled (maker)"
                );
            }
        }
    }
}
