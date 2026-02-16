use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::engine::inventory::inventory_skew;
use crate::engine::quoter::{QuoteState, Quoter};
use crate::risk::kill_switch::KillSwitch;
use crate::risk::limits::can_open_buy;
use crate::risk::rate_limiter::{RateLimiter, RateStatus};
use crate::state::bot_state::{BotState, TrackedOrder};
use crate::types::*;

/// Truncate a Decimal to `dp` decimal places (floor, never rounds up).
/// This prevents sending a qty slightly larger than available balance.
fn truncate_decimal(value: Decimal, dp: u32) -> Decimal {
    let factor = Decimal::from(10u64.pow(dp));
    (value * factor).floor() / factor
}

/// The core engine: processes events, emits commands.
/// Contains no I/O — fully deterministic and testable.
pub struct Engine {
    config: Config,
    books: HashMap<String, OrderBook>,
    quoters: HashMap<String, Quoter>,
    pair_info: HashMap<String, PairInfo>,
    pub state: BotState,
    kill_switch: KillSwitch,
    rate_limiter: RateLimiter,
    prices: HashMap<String, Decimal>,
    disabled_pairs: HashSet<String>,
    sell_only_pairs: HashSet<String>,
    pending_liquidation: HashSet<String>,
    liq_retry_count: HashMap<String, u32>,
    cl_ord_counter: u64,
    last_dms_refresh: Option<DateTime<Utc>>,
}

impl Engine {
    pub fn new(
        config: Config,
        pair_info: HashMap<String, PairInfo>,
        state: BotState,
    ) -> Self {
        let quoters = pair_info
            .keys()
            .map(|s| (s.clone(), Quoter::new(s.clone())))
            .collect();
        let rate_limiter = RateLimiter::new(&config.risk);
        Self {
            config,
            books: HashMap::new(),
            quoters,
            pair_info,
            state,
            kill_switch: KillSwitch::default(),
            rate_limiter,
            prices: HashMap::new(),
            disabled_pairs: HashSet::new(),
            sell_only_pairs: HashSet::new(),
            pending_liquidation: HashSet::new(),
            liq_retry_count: HashMap::new(),
            cl_ord_counter: 0,
            last_dms_refresh: None,
        }
    }

    /// Mark pairs as sell-only (downtrend filter). No new buys will be placed.
    pub fn set_sell_only(&mut self, pairs: HashSet<String>) {
        self.sell_only_pairs = pairs;
    }

    pub fn books(&self) -> &HashMap<String, OrderBook> {
        &self.books
    }

    /// Process a single event and return resulting commands.
    /// This is the core logic — fully deterministic.
    pub fn handle_event(&mut self, event: EngineEvent) -> Vec<EngineCommand> {
        match event {
            EngineEvent::BookSnapshot {
                symbol,
                bids,
                asks,
                timestamp,
            } => self.on_book_snapshot(symbol, bids, asks, timestamp),
            EngineEvent::BookUpdate {
                symbol,
                bid_updates,
                ask_updates,
                timestamp,
            } => self.on_book_update(symbol, bid_updates, ask_updates, timestamp),
            EngineEvent::Fill(fill) => self.on_fill(fill),
            EngineEvent::OrderAcknowledged { cl_ord_id, order_id } => {
                if let Some(order) = self.state.open_orders.get_mut(&cl_ord_id) {
                    order.acked = true;
                    tracing::info!(
                        cl_ord_id,
                        order_id,
                        symbol = order.symbol,
                        side = %order.side,
                        price = %order.price,
                        "Order live on exchange"
                    );
                } else {
                    tracing::debug!(cl_ord_id, order_id, "Ack for unknown order (already cancelled?)");
                }
                vec![]
            }
            EngineEvent::OrderCancelled { cl_ord_id, symbol } => {
                self.on_order_cancelled(&cl_ord_id, &symbol)
            }
            EngineEvent::OrderRejected {
                cl_ord_id,
                symbol,
                reason,
            } => {
                // Look up symbol from tracked orders if not provided
                let sym = if symbol.is_empty() {
                    self.state
                        .open_orders
                        .get(&cl_ord_id)
                        .map(|o| o.symbol.clone())
                        .unwrap_or_default()
                } else {
                    symbol
                };
                tracing::warn!(cl_ord_id, symbol = sym, reason, "Order rejected");

                // If pair is restricted (jurisdiction/permissions), disable it permanently
                if reason.contains("restricted") || reason.contains("Invalid permissions") {
                    tracing::warn!(symbol = sym, reason, "Pair restricted — disabling permanently");
                    self.disabled_pairs.insert(sym.clone());
                    // Cancel any remaining orders for this pair
                    let mut cmds = self.cancel_pair_quotes(&sym);
                    cmds.extend(self.on_order_cancelled(&cl_ord_id, &sym));
                    return cmds;
                }

                // If a liquidation order was rejected, increment retry counter
                // and keep in pending_liquidation so phase 2 retries next tick.
                if cl_ord_id.starts_with("liq") {
                    let count = self.liq_retry_count.entry(sym.clone()).or_insert(0);
                    *count += 1;
                    let pos = self.state.position(&sym);
                    tracing::error!(
                        cl_ord_id,
                        symbol = sym,
                        reason,
                        retry = *count,
                        state_qty = %pos.qty,
                        state_avg_cost = %pos.avg_cost,
                        "LIQUIDATION SELL REJECTED — will retry next tick (attempt {}/3)",
                        *count
                    );
                }

                self.on_order_cancelled(&cl_ord_id, &sym)
            }
            EngineEvent::Tick { timestamp } => self.on_tick(timestamp),
            EngineEvent::ApiCommand(action) => self.on_api_command(action),
        }
    }

    fn on_api_command(&mut self, action: crate::types::event::ApiAction) -> Vec<EngineCommand> {
        use crate::types::event::ApiAction;
        match action {
            ApiAction::CancelAll => {
                tracing::info!("API: Cancel all orders");
                // Clear quoters and open orders
                for quoter in self.quoters.values_mut() {
                    quoter.bid_cl_ord_id = None;
                    quoter.ask_cl_ord_id = None;
                    quoter.state = crate::engine::quoter::QuoteState::Idle;
                    quoter.last_mid = None;
                }
                self.state.open_orders.clear();
                vec![EngineCommand::CancelAll]
            }
            ApiAction::CancelOrder { cl_ord_id } => {
                tracing::info!(cl_ord_id, "API: Cancel order");
                if let Some(order) = self.state.open_orders.remove(&cl_ord_id) {
                    if let Some(quoter) = self.quoters.get_mut(&order.symbol) {
                        quoter.mark_cancelled(&cl_ord_id);
                    }
                }
                vec![EngineCommand::CancelOrders(vec![cl_ord_id])]
            }
            ApiAction::Pause => {
                tracing::info!("API: Pausing — sell-only mode for all pairs");
                self.state.paused = true;
                for symbol in self.pair_info.keys() {
                    self.sell_only_pairs.insert(symbol.clone());
                }
                vec![EngineCommand::PersistState(self.state.clone())]
            }
            ApiAction::Resume => {
                tracing::info!("API: Resuming — clearing sell-only mode");
                self.state.paused = false;
                self.sell_only_pairs.clear();
                vec![EngineCommand::PersistState(self.state.clone())]
            }
            ApiAction::Shutdown => {
                tracing::info!("API: Shutdown requested");
                // Cancel all, persist, then shutdown
                for quoter in self.quoters.values_mut() {
                    quoter.bid_cl_ord_id = None;
                    quoter.ask_cl_ord_id = None;
                    quoter.state = crate::engine::quoter::QuoteState::Idle;
                }
                self.state.open_orders.clear();
                vec![
                    EngineCommand::CancelAll,
                    EngineCommand::PersistState(self.state.clone()),
                    EngineCommand::Shutdown { reason: "API shutdown request".into() },
                ]
            }
            ApiAction::Liquidate { symbol } => {
                tracing::info!(symbol, "API: Liquidate position");
                self.on_liquidate(&symbol)
            }
        }
    }

    /// Run the engine, consuming events and sending commands via channels.
    pub async fn run(
        &mut self,
        mut events: mpsc::Receiver<EngineEvent>,
        commands: mpsc::Sender<EngineCommand>,
    ) -> anyhow::Result<()> {
        while let Some(event) = events.recv().await {
            for cmd in self.handle_event(event) {
                let is_shutdown = matches!(cmd, EngineCommand::Shutdown { .. });
                commands.send(cmd).await?;
                if is_shutdown {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    fn next_cl_ord_id(&mut self, prefix: &str) -> String {
        self.cl_ord_counter += 1;
        format!("{}-{}", prefix, self.cl_ord_counter)
    }

    // --- Book Events ---

    fn on_book_snapshot(
        &mut self,
        symbol: String,
        bids: Vec<LevelUpdate>,
        asks: Vec<LevelUpdate>,
        timestamp: DateTime<Utc>,
    ) -> Vec<EngineCommand> {
        let mut book = OrderBook::new();
        book.apply_snapshot(
            bids.into_iter().map(|l| (l.price, l.qty)),
            asks.into_iter().map(|l| (l.price, l.qty)),
        );
        self.books.insert(symbol.clone(), book);

        let book = self.books.get(&symbol).unwrap();
        let mid = book.mid_price();
        let spread_bps = book.spread_bps();
        if let Some(m) = mid {
            self.prices.insert(symbol.clone(), m);
        }
        tracing::info!(
            symbol,
            mid = %mid.unwrap_or_default(),
            spread_bps = %spread_bps.unwrap_or_default().round_dp(0),
            bid_levels = book.bid_depth(),
            ask_levels = book.ask_depth(),
            "Book snapshot received"
        );

        self.maybe_quote(&symbol, timestamp)
    }

    fn on_book_update(
        &mut self,
        symbol: String,
        bid_updates: Vec<LevelUpdate>,
        ask_updates: Vec<LevelUpdate>,
        timestamp: DateTime<Utc>,
    ) -> Vec<EngineCommand> {
        if let Some(book) = self.books.get_mut(&symbol) {
            for lu in &bid_updates {
                book.update_bid(lu.price, lu.qty);
            }
            for lu in &ask_updates {
                book.update_ask(lu.price, lu.qty);
            }
        }

        if let Some(mid) = self.books.get(&symbol).and_then(|b| b.mid_price()) {
            self.prices.insert(symbol.clone(), mid);
        }

        self.maybe_quote(&symbol, timestamp)
    }

    // --- Fill ---

    fn on_fill(&mut self, fill: Fill) -> Vec<EngineCommand> {
        let mut cmds = vec![];

        tracing::info!(
            symbol = fill.symbol,
            side = %fill.side,
            qty = %fill.qty,
            price = %fill.price,
            fee = %fill.fee,
            maker = fill.is_maker,
            "FILL"
        );

        let mut pnl = Decimal::ZERO;
        match fill.side {
            OrderSide::Buy => {
                let pos = self.state.positions.entry(fill.symbol.clone()).or_default();
                pos.apply_buy(fill.qty, fill.price);
                self.state.total_fees += fill.fee;
                self.state.trade_count += 1;
            }
            OrderSide::Sell => {
                let pos = self.state.positions.entry(fill.symbol.clone()).or_default();
                pnl = pos.apply_sell(fill.qty, fill.price) - fill.fee;
                self.state.realized_pnl += pnl;
                self.state.total_fees += fill.fee;
                self.state.trade_count += 1;
                if pos.is_empty() {
                    self.state.positions.remove(&fill.symbol);
                    // Liquidation complete — allow pair to resume normal quoting
                    if self.pending_liquidation.remove(&fill.symbol) {
                        self.disabled_pairs.remove(&fill.symbol);
                        self.liq_retry_count.remove(&fill.symbol);
                        tracing::info!(symbol = fill.symbol, "Liquidation complete — pair re-enabled");
                    }
                }
                tracing::info!(pnl = %pnl, cumulative = %self.state.realized_pnl, "Round-trip P&L");
            }
        }

        // Log trade
        cmds.push(EngineCommand::LogTrade(TradeRecord {
            timestamp: fill.timestamp,
            symbol: fill.symbol.clone(),
            side: fill.side.to_string(),
            price: fill.price,
            qty: fill.qty,
            value_usd: fill.price * fill.qty,
            fee: fill.fee,
            pnl,
            cumulative_pnl: self.state.realized_pnl,
        }));

        // Update quoter state
        if fill.is_fully_filled {
            self.state.open_orders.remove(&fill.cl_ord_id);
            if let Some(quoter) = self.quoters.get_mut(&fill.symbol) {
                match fill.side {
                    OrderSide::Buy => quoter.mark_bid_filled(),
                    OrderSide::Sell => quoter.mark_ask_filled(),
                }
            }
        }

        // Persist state
        cmds.push(EngineCommand::PersistState(self.state.clone()));

        // Check kill switch
        if self.kill_switch.check(self.state.realized_pnl, &self.config.risk) {
            cmds.push(EngineCommand::CancelAll);
            cmds.push(EngineCommand::Shutdown {
                reason: self.kill_switch.reason.clone(),
            });
        }

        // Immediately re-quote this pair (don't wait for next book update).
        // On low-liquidity pairs, book updates can be minutes apart.
        let symbol = fill.symbol.clone();
        tracing::info!(symbol, "Re-quoting after fill");
        let quote_cmds = self.maybe_quote(&symbol, fill.timestamp);
        cmds.extend(quote_cmds);

        cmds
    }

    // --- Cancellation ---

    fn on_order_cancelled(&mut self, cl_ord_id: &str, symbol: &str) -> Vec<EngineCommand> {
        let side_str = self.state.open_orders.get(cl_ord_id)
            .map(|o| o.side.to_string())
            .unwrap_or_else(|| "?".to_string());
        tracing::info!(cl_ord_id, symbol, side = side_str, "Order cancelled — returning to Idle for this side");
        self.state.open_orders.remove(cl_ord_id);
        if let Some(quoter) = self.quoters.get_mut(symbol) {
            quoter.mark_cancelled(cl_ord_id);
        }
        vec![]
    }

    // --- Liquidation (two-phase) ---

    /// Phase 1: Cancel tracked orders for this specific pair only (NOT CancelAll).
    /// Ghost orders should be cleared by the startup CancelAll in run_live().
    /// The actual sell happens on the next tick (phase 2) after cancels clear.
    pub fn on_liquidate(&mut self, symbol: &str) -> Vec<EngineCommand> {
        let mut cmds = vec![];

        // Skip if already pending liquidation
        if self.pending_liquidation.contains(symbol) {
            return cmds;
        }

        let position = self.state.position(symbol);
        if position.is_empty() {
            tracing::warn!(symbol, "Liquidation requested but no position held");
            return cmds;
        }

        let pair_info = self.pair_info.get(symbol);
        let qty_decimals = pair_info.map_or(8, |pi| pi.qty_decimals);

        tracing::error!(
            symbol,
            qty = %position.qty,
            avg_cost = %position.avg_cost,
            value_usd = %(position.qty * position.avg_cost).round_dp(2),
            qty_decimals,
            "LIQUIDATION PHASE 1: Cancelling orders for this pair"
        );

        // Cancel orders for THIS pair only (targeted, not CancelAll)
        let cancel_cmds = self.cancel_all_pair_orders(symbol);
        cmds.extend(cancel_cmds);

        // Mark pair as pending liquidation and disabled (no new quoting)
        self.pending_liquidation.insert(symbol.to_string());
        self.disabled_pairs.insert(symbol.to_string());
        self.liq_retry_count.insert(symbol.to_string(), 0);

        cmds
    }

    /// Phase 2: Called on tick when a pair is pending liquidation and all
    /// orders for it have been cleared. Now safe to send the market sell.
    /// No CancelAll — ghost orders are cleared at startup; targeted cancels
    /// in Phase 1 handle tracked orders.
    fn send_liquidation_sell(&mut self, symbol: &str) -> Vec<EngineCommand> {
        let mut cmds = vec![];

        // Check retry limit
        let retries = self.liq_retry_count.get(symbol).copied().unwrap_or(0);
        if retries >= 3 {
            tracing::error!(
                symbol,
                retries,
                "LIQUIDATION FAILED after {} attempts — giving up. Manual intervention needed. \
                 Position remains but pair is disabled to prevent new orders.",
                retries
            );
            self.pending_liquidation.remove(symbol);
            // Keep disabled to prevent new orders on a broken pair
            return cmds;
        }

        let position = self.state.position(symbol);
        if position.is_empty() {
            tracing::info!(symbol, "Liquidation: position already empty — clearing state");
            self.pending_liquidation.remove(symbol);
            self.disabled_pairs.remove(symbol);
            self.liq_retry_count.remove(symbol);
            return cmds;
        }

        let ref_price = self.books.get(symbol)
            .and_then(|b| b.best_bid())
            .map(|(p, _)| p)
            .or_else(|| self.prices.get(symbol).copied())
            .unwrap_or_default();

        if ref_price.is_zero() {
            tracing::error!(symbol, "Cannot liquidate — no price available");
            return cmds;
        }

        let pair_info = self.pair_info.get(symbol);
        let min_qty = pair_info.map_or(Decimal::ONE, |pi| pi.min_order_qty);
        let min_cost = pair_info.map_or(dec!(0.5), |pi| pi.min_cost);
        let qty_decimals = pair_info.map_or(8, |pi| pi.qty_decimals);

        if position.qty < min_qty || position.qty * ref_price < min_cost {
            tracing::warn!(
                symbol,
                qty = %position.qty,
                min_qty = %min_qty,
                min_cost = %min_cost,
                "Cannot liquidate — below exchange minimums. Clearing liquidation state."
            );
            self.pending_liquidation.remove(symbol);
            self.disabled_pairs.remove(symbol);
            self.liq_retry_count.remove(symbol);
            return cmds;
        }

        let cl_id = self.next_cl_ord_id("liq");
        // Truncate qty to pair's decimal precision (floor, not round)
        // to avoid exceeding Kraken's recorded balance due to precision mismatch.
        let raw_qty = position.qty;
        let qty = truncate_decimal(raw_qty, qty_decimals);

        tracing::error!(
            symbol,
            cl_ord_id = cl_id,
            raw_qty = %raw_qty,
            truncated_qty = %qty,
            qty_decimals,
            ref_price = %ref_price,
            avg_cost = %position.avg_cost,
            value_usd = %(qty * ref_price).round_dp(2),
            attempt = retries + 1,
            ">>> LIQUIDATION MARKET SELL (phase 2)"
        );

        self.state.open_orders.insert(
            cl_id.clone(),
            TrackedOrder {
                cl_ord_id: cl_id.clone(),
                symbol: symbol.to_string(),
                side: OrderSide::Sell,
                price: ref_price,
                qty,
                placed_at: Utc::now(),
                acked: false,
            },
        );

        cmds.push(EngineCommand::PlaceOrder(OrderRequest {
            cl_ord_id: cl_id,
            symbol: symbol.to_string(),
            side: OrderSide::Sell,
            price: ref_price,
            qty,
            post_only: false,
            market: true,
        }));

        cmds
    }

    /// Cancel ALL tracked orders for a symbol (not just quoter-tracked ones).
    fn cancel_all_pair_orders(&mut self, symbol: &str) -> Vec<EngineCommand> {
        // Cancel quoter state
        if let Some(quoter) = self.quoters.get_mut(symbol) {
            quoter.bid_cl_ord_id = None;
            quoter.ask_cl_ord_id = None;
            quoter.state = QuoteState::Idle;
            quoter.last_mid = None;
        }

        // Find ALL orders for this pair in open_orders
        let to_cancel: Vec<String> = self.state.open_orders.iter()
            .filter(|(_, o)| o.symbol == symbol)
            .map(|(id, _)| id.clone())
            .collect();

        for id in &to_cancel {
            self.state.open_orders.remove(id);
        }

        if to_cancel.is_empty() {
            vec![]
        } else {
            tracing::info!(symbol, orders = ?to_cancel, "Cancelling all pair orders for liquidation");
            vec![EngineCommand::CancelOrders(to_cancel)]
        }
    }

    // --- Tick ---

    fn on_tick(&mut self, timestamp: DateTime<Utc>) -> Vec<EngineCommand> {
        let mut cmds = vec![];

        // Liquidation phase 2: for pairs pending liquidation, check order state.
        // - If a liq order is already pending → wait for fill/rejection
        // - If old (non-liq) orders remain → re-cancel them
        // - If no orders at all → send the market sell
        let pending: Vec<String> = self.pending_liquidation.iter().cloned().collect();
        for symbol in pending {
            let has_liq_order = self.state.open_orders.values()
                .any(|o| o.symbol == symbol && o.cl_ord_id.starts_with("liq"));
            let has_other_orders = self.state.open_orders.values()
                .any(|o| o.symbol == symbol && !o.cl_ord_id.starts_with("liq"));

            if has_liq_order {
                tracing::debug!(symbol, "Liquidation sell pending — waiting for fill/rejection");
            } else if has_other_orders {
                // Old orders still present — re-cancel them
                let stale: Vec<String> = self.state.open_orders.iter()
                    .filter(|(_, o)| o.symbol == symbol && !o.cl_ord_id.starts_with("liq"))
                    .map(|(id, _)| id.clone())
                    .collect();
                tracing::warn!(symbol, remaining = ?stale, "Liquidation waiting for order cancels — re-cancelling");
                for id in &stale {
                    self.state.open_orders.remove(id);
                }
                if !stale.is_empty() {
                    cmds.push(EngineCommand::CancelOrders(stale));
                }
            } else {
                tracing::info!(symbol, "Liquidation phase 2: all orders cleared, sending market sell");
                cmds.extend(self.send_liquidation_sell(&symbol));
            }
        }

        // Stop-loss AND take-profit check: liquidate on extreme moves
        let stop_loss_pct = self.config.risk.stop_loss_pct;
        let take_profit_pct = self.config.risk.take_profit_pct;
        let symbols_to_liquidate: Vec<String> = self.state.positions.iter()
            .filter(|(symbol, pos)| !pos.is_empty() && !pos.avg_cost.is_zero() && !self.pending_liquidation.contains(symbol.as_str()))
            .filter_map(|(symbol, pos)| {
                let mid = self.prices.get(symbol)?;
                let change = (*mid - pos.avg_cost) / pos.avg_cost;
                if change < -stop_loss_pct {
                    tracing::error!(
                        symbol,
                        mid = %mid,
                        avg_cost = %pos.avg_cost,
                        change_pct = %(change * Decimal::from(100)).round_dp(2),
                        threshold_pct = %(-stop_loss_pct * Decimal::from(100)),
                        "STOP-LOSS TRIGGERED — starting liquidation"
                    );
                    Some(symbol.clone())
                } else if change > take_profit_pct {
                    tracing::error!(
                        symbol,
                        mid = %mid,
                        avg_cost = %pos.avg_cost,
                        change_pct = %(change * Decimal::from(100)).round_dp(2),
                        threshold_pct = %(take_profit_pct * Decimal::from(100)),
                        "TAKE-PROFIT TRIGGERED — starting liquidation"
                    );
                    Some(symbol.clone())
                } else {
                    None
                }
            })
            .collect();

        for symbol in symbols_to_liquidate {
            cmds.extend(self.on_liquidate(&symbol));
        }

        // Periodic status summary
        let open = self.state.open_orders.len();
        let acked = self.state.open_orders.values().filter(|o| o.acked).count();
        let total_exposure = self.state.total_exposure_usd(&self.prices);
        let active_pairs: Vec<_> = self.quoters.iter()
            .filter(|(_, q)| q.state != QuoteState::Idle)
            .map(|(s, q)| format!("{}:{:?}", s, q.state))
            .collect();
        tracing::info!(
            open_orders = open,
            acked,
            trades = self.state.trade_count,
            pnl = %self.state.realized_pnl,
            fees = %self.state.total_fees,
            exposure_usd = %total_exposure.round_dp(2),
            pairs = ?active_pairs,
            "Tick status"
        );

        // Stale order check
        let stale: Vec<String> = self
            .state
            .open_orders
            .iter()
            .filter(|(_, o)| {
                let age = (timestamp - o.placed_at).num_seconds();
                age > self.config.risk.stale_order_secs as i64
            })
            .map(|(id, _)| id.clone())
            .collect();

        if !stale.is_empty() {
            tracing::info!(count = stale.len(), ids = ?stale, "Cancelling stale orders");
            for id in &stale {
                if let Some(order) = self.state.open_orders.remove(id) {
                    if let Some(quoter) = self.quoters.get_mut(&order.symbol) {
                        quoter.mark_cancelled(id);
                    }
                }
            }
            cmds.push(EngineCommand::CancelOrders(stale));
        }

        // DMS refresh
        let should_refresh = match self.last_dms_refresh {
            None => true,
            Some(last) => {
                (timestamp - last).num_seconds() >= self.config.risk.dms_refresh_secs as i64
            }
        };
        if should_refresh {
            self.last_dms_refresh = Some(timestamp);
            cmds.push(EngineCommand::RefreshDms);
        }

        cmds
    }

    // --- Quoting Logic ---

    fn maybe_quote(&mut self, symbol: &str, timestamp: DateTime<Utc>) -> Vec<EngineCommand> {
        if self.disabled_pairs.contains(symbol) {
            return vec![];
        }
        // Sell-only with no position → nothing to do (avoid noisy logs)
        if self.sell_only_pairs.contains(symbol) && self.state.position(symbol).is_empty() {
            return vec![];
        }
        if self.kill_switch.triggered {
            tracing::debug!(symbol, "Skipping quote — kill switch triggered");
            return vec![];
        }
        if self.rate_limiter.status() == RateStatus::Block {
            tracing::debug!(symbol, "Skipping quote — rate limited");
            return vec![];
        }

        let book = match self.books.get(symbol) {
            Some(b) if !b.is_empty() => b,
            _ => {
                tracing::debug!(symbol, "Skipping quote — no book data");
                return vec![];
            }
        };
        let (best_bid, _) = match book.best_bid() {
            Some(b) => b,
            None => return vec![],
        };
        let (best_ask, _) = match book.best_ask() {
            Some(a) => a,
            None => return vec![],
        };
        let mid = match book.mid_price() {
            Some(m) => m,
            None => return vec![],
        };

        let pair_info = match self.pair_info.get(symbol) {
            Some(pi) => pi.clone(),
            None => {
                tracing::warn!(symbol, "Skipping quote — no pair_info");
                return vec![];
            }
        };

        let skew = inventory_skew(
            &self.state,
            symbol,
            mid,
            self.config.risk.max_inventory_usd,
        );

        // Clone config parts we need to avoid borrow issues
        let trading = self.config.trading.clone();
        let risk = self.config.risk.clone();

        let quoter = match self.quoters.get(symbol) {
            Some(q) => q,
            None => return vec![],
        };

        let quotes = quoter.compute_quotes(best_bid, best_ask, mid, skew, &trading, &pair_info);
        let qty = quoter.compute_qty(mid, &trading, &pair_info);

        let spread_bps_raw = if !mid.is_zero() {
            ((best_ask - best_bid) / mid * dec!(10000)).round_dp(0)
        } else {
            Decimal::ZERO
        };

        match quotes {
            None => {
                // Spread too narrow — cancel existing quotes
                let quoter = self.quoters.get(symbol).unwrap();
                if quoter.state != QuoteState::Idle {
                    tracing::info!(
                        symbol,
                        spread_bps = %spread_bps_raw,
                        state = ?quoter.state,
                        "Spread too narrow — cancelling quotes"
                    );
                }
                self.cancel_pair_quotes(symbol)
            }
            Some((bid_price, ask_price)) => {
                let quoter = self.quoters.get(symbol).unwrap();
                match quoter.state {
                    QuoteState::Idle => {
                        tracing::info!(
                            symbol,
                            mid = %mid,
                            spread_bps = %spread_bps_raw,
                            "Spread wide enough — placing fresh quotes"
                        );
                        self.place_fresh_quotes(symbol, bid_price, ask_price, qty, mid, timestamp, &risk)
                    }
                    QuoteState::Quoting => {
                        let should = quoter.should_requote(mid, &trading);
                        if should {
                            self.requote(symbol, bid_price, ask_price, mid)
                        } else {
                            vec![]
                        }
                    }
                    QuoteState::BidFilled => {
                        tracing::debug!(symbol, "Bid filled — managing ask side");
                        self.manage_one_side(symbol, OrderSide::Sell, ask_price, qty, mid, timestamp)
                    }
                    QuoteState::AskFilled => {
                        tracing::debug!(symbol, "Ask filled — managing bid side");
                        self.manage_one_side(symbol, OrderSide::Buy, bid_price, qty, mid, timestamp)
                    }
                }
            }
        }
    }

    fn place_fresh_quotes(
        &mut self,
        symbol: &str,
        bid_price: Decimal,
        ask_price: Decimal,
        qty: Decimal,
        mid: Decimal,
        timestamp: DateTime<Utc>,
        risk: &crate::config::RiskConfig,
    ) -> Vec<EngineCommand> {
        let mut cmds = vec![];

        let position = self.state.position(symbol);
        let pair_exposure = self.state.pair_exposure_usd(symbol, mid);
        let total_exposure = self.state.total_exposure_usd(&self.prices);
        let order_value = qty * mid;

        let can_buy = if self.sell_only_pairs.contains(symbol) {
            false
        } else {
            can_open_buy(
                &self.state,
                symbol,
                order_value,
                mid,
                &self.prices,
                risk,
            )
        };

        // Sell what we have, even if less than full order_size_usd.
        // Only need to meet the pair's minimum order requirements.
        let pair_info = self.pair_info.get(symbol);
        let min_qty = pair_info.map_or(Decimal::ONE, |pi| pi.min_order_qty);
        let min_cost = pair_info.map_or(dec!(0.5), |pi| pi.min_cost);
        let can_sell = position.qty >= min_qty && position.qty * mid >= min_cost;
        let qty_dp = pair_info.map_or(8, |pi| pi.qty_decimals);
        let sell_qty = if can_sell {
            // Sell the lesser of our position or the standard order qty.
            // Truncate (floor) to pair's qty precision to avoid exceeding balance.
            truncate_decimal(qty.min(position.qty), qty_dp)
        } else {
            Decimal::ZERO
        };

        // Cost-anchored sell: never sell below avg_cost * (1 + min_profit_pct)
        let price_decimals = pair_info.map_or(8, |pi| pi.price_decimals);
        let min_profit_pct = self.config.trading.min_profit_pct;
        let ask_price = if !position.avg_cost.is_zero() && can_sell {
            let cost_floor = (position.avg_cost * (Decimal::ONE + min_profit_pct))
                .round_dp(price_decimals);
            if cost_floor > ask_price {
                tracing::info!(
                    symbol,
                    book_ask = %ask_price,
                    cost_floor = %cost_floor,
                    avg_cost = %position.avg_cost,
                    "Raising ask to cost floor (guaranteeing profit)"
                );
            }
            ask_price.max(cost_floor)
        } else {
            ask_price
        };

        tracing::info!(
            symbol,
            position_qty = %position.qty,
            position_value_usd = %(position.qty * mid).round_dp(2),
            position_avg_cost = %position.avg_cost,
            order_qty = %qty,
            sell_qty = %sell_qty,
            order_value_usd = %order_value.round_dp(2),
            pair_exposure_usd = %pair_exposure.round_dp(2),
            total_exposure_usd = %total_exposure.round_dp(2),
            max_inventory_usd = %risk.max_inventory_usd,
            can_buy,
            can_sell,
            bid = %bid_price,
            ask = %ask_price,
            "Quote decision"
        );

        if !can_sell && !position.qty.is_zero() {
            tracing::info!(
                symbol,
                held = %position.qty,
                min_order_qty = %min_qty,
                min_cost_usd = %min_cost,
                "Cannot sell — position below exchange minimums"
            );
        }
        if !can_buy {
            if self.sell_only_pairs.contains(symbol) || self.disabled_pairs.contains(symbol) {
                tracing::info!(
                    symbol,
                    sell_only = self.sell_only_pairs.contains(symbol),
                    disabled = self.disabled_pairs.contains(symbol),
                    "Cannot buy — pair restricted"
                );
            } else {
                let reason = if pair_exposure + order_value > risk.max_inventory_usd {
                    "pair limit"
                } else {
                    "total exposure limit"
                };
                tracing::info!(
                    symbol,
                    reason,
                    pair_exposure_usd = %pair_exposure.round_dp(2),
                    total_exposure_usd = %total_exposure.round_dp(2),
                    order_value_usd = %order_value.round_dp(2),
                    max_inventory_usd = %risk.max_inventory_usd,
                    max_total_exposure_usd = %risk.max_total_exposure_usd,
                    "Cannot buy — risk limit reached"
                );
            }
        }

        let mut bid_id_placed: Option<String> = None;
        let mut ask_id_placed: Option<String> = None;

        // Buy side: place bid if risk limits allow
        if can_buy {
            let bid_id = self.next_cl_ord_id("bid");
            let req = OrderRequest {
                cl_ord_id: bid_id.clone(),
                symbol: symbol.to_string(),
                side: OrderSide::Buy,
                price: bid_price,
                qty,
                post_only: true,
                market: false,
            };
            self.state.open_orders.insert(
                bid_id.clone(),
                TrackedOrder {
                    cl_ord_id: bid_id.clone(),
                    symbol: symbol.to_string(),
                    side: OrderSide::Buy,
                    price: bid_price,
                    qty,
                    placed_at: timestamp,
                    acked: false,
                },
            );
            cmds.push(EngineCommand::PlaceOrder(req));
            bid_id_placed = Some(bid_id);
        }

        // Sell side: only place ask if we hold enough inventory
        if can_sell {
            let ask_id = self.next_cl_ord_id("ask");
            let ask_req = OrderRequest {
                cl_ord_id: ask_id.clone(),
                symbol: symbol.to_string(),
                side: OrderSide::Sell,
                price: ask_price,
                qty: sell_qty,
                post_only: true,
                market: false,
            };
            self.state.open_orders.insert(
                ask_id.clone(),
                TrackedOrder {
                    cl_ord_id: ask_id.clone(),
                    symbol: symbol.to_string(),
                    side: OrderSide::Sell,
                    price: ask_price,
                    qty: sell_qty,
                    placed_at: timestamp,
                    acked: false,
                },
            );
            cmds.push(EngineCommand::PlaceOrder(ask_req));
            ask_id_placed = Some(ask_id);
        }

        if bid_id_placed.is_none() && ask_id_placed.is_none() {
            return cmds;
        }

        if let Some(quoter) = self.quoters.get_mut(symbol) {
            quoter.state = crate::engine::quoter::QuoteState::Quoting;
            quoter.last_mid = Some(mid);
            quoter.last_quote_time = Some(timestamp);
            if let Some(ref bid_id) = bid_id_placed {
                quoter.bid_cl_ord_id = Some(bid_id.clone());
            }
            if let Some(ref ask_id) = ask_id_placed {
                quoter.ask_cl_ord_id = Some(ask_id.clone());
            }
        }

        if let Some(ref bid_id) = bid_id_placed {
            tracing::info!(
                symbol,
                side = "BUY",
                cl_ord_id = bid_id.as_str(),
                price = %bid_price,
                qty = %qty,
                ">>> PLACING BUY ORDER"
            );
        }
        if let Some(ref ask_id) = ask_id_placed {
            tracing::info!(
                symbol,
                side = "SELL",
                cl_ord_id = ask_id.as_str(),
                price = %ask_price,
                qty = %sell_qty,
                ">>> PLACING SELL ORDER"
            );
        }

        cmds
    }

    fn requote(
        &mut self,
        symbol: &str,
        bid_price: Decimal,
        ask_price: Decimal,
        mid: Decimal,
    ) -> Vec<EngineCommand> {
        let mut cmds = vec![];

        // Apply cost floor to ask price
        let ask_price = {
            let position = self.state.position(symbol);
            if !position.avg_cost.is_zero() {
                let price_decimals = self.pair_info.get(symbol).map_or(8, |pi| pi.price_decimals);
                let cost_floor = (position.avg_cost * (Decimal::ONE + self.config.trading.min_profit_pct))
                    .round_dp(price_decimals);
                ask_price.max(cost_floor)
            } else {
                ask_price
            }
        };

        let quoter = match self.quoters.get_mut(symbol) {
            Some(q) => q,
            None => return cmds,
        };

        if let Some(ref bid_id) = quoter.bid_cl_ord_id {
            let acked = self.state.open_orders.get(bid_id).map_or(false, |o| o.acked);
            if acked {
                cmds.push(EngineCommand::AmendOrder {
                    cl_ord_id: bid_id.clone(),
                    new_price: Some(bid_price),
                    new_qty: None,
                });
                if let Some(order) = self.state.open_orders.get_mut(bid_id) {
                    order.price = bid_price;
                }
            } else {
                tracing::debug!(symbol, cl_ord_id = bid_id.as_str(), "Skipping bid amend — not yet acked");
            }
        }
        if let Some(ref ask_id) = quoter.ask_cl_ord_id {
            let acked = self.state.open_orders.get(ask_id).map_or(false, |o| o.acked);
            if acked {
                cmds.push(EngineCommand::AmendOrder {
                    cl_ord_id: ask_id.clone(),
                    new_price: Some(ask_price),
                    new_qty: None,
                });
                if let Some(order) = self.state.open_orders.get_mut(ask_id) {
                    order.price = ask_price;
                }
            } else {
                tracing::debug!(symbol, cl_ord_id = ask_id.as_str(), "Skipping ask amend — not yet acked");
            }
        }

        quoter.last_mid = Some(mid);

        if !cmds.is_empty() {
            let spread_bps = (ask_price - bid_price) / mid * dec!(10000);
            tracing::info!(symbol, bid = %bid_price, ask = %ask_price, spread_bps = %spread_bps.round_dp(0), "Requoting — mid moved");
        }

        cmds
    }

    fn manage_one_side(
        &mut self,
        symbol: &str,
        side: OrderSide,
        price: Decimal,
        qty: Decimal,
        mid: Decimal,
        timestamp: DateTime<Utc>,
    ) -> Vec<EngineCommand> {
        // Apply cost floor for sells
        let price = if side == OrderSide::Sell {
            let position = self.state.position(symbol);
            if !position.avg_cost.is_zero() {
                let price_decimals = self.pair_info.get(symbol).map_or(8, |pi| pi.price_decimals);
                let cost_floor = (position.avg_cost * (Decimal::ONE + self.config.trading.min_profit_pct))
                    .round_dp(price_decimals);
                price.max(cost_floor)
            } else {
                price
            }
        } else {
            price
        };

        let quoter = match self.quoters.get(symbol) {
            Some(q) => q,
            None => return vec![],
        };

        let existing_id = match side {
            OrderSide::Buy => &quoter.bid_cl_ord_id,
            OrderSide::Sell => &quoter.ask_cl_ord_id,
        };

        if let Some(id) = existing_id {
            // Amend if price moved enough AND order is acked
            if let Some(order) = self.state.open_orders.get(id) {
                if !order.acked {
                    tracing::debug!(symbol, cl_ord_id = id.as_str(), "Skipping amend — not yet acked");
                    return vec![];
                }
                if order.price.is_zero()
                    || ((price - order.price) / order.price).abs()
                        > self.config.trading.requote_threshold_pct
                {
                    let cmd = EngineCommand::AmendOrder {
                        cl_ord_id: id.clone(),
                        new_price: Some(price),
                        new_qty: None,
                    };
                    if let Some(order) = self.state.open_orders.get_mut(id) {
                        order.price = price;
                    }
                    return vec![cmd];
                }
            }
            vec![]
        } else {
            // Compute actual order qty (for sells, cap to position)
            let actual_qty = if side == OrderSide::Sell {
                let position = self.state.position(symbol);
                let pair_info = self.pair_info.get(symbol);
                let min_qty = pair_info.map_or(Decimal::ONE, |pi| pi.min_order_qty);
                let min_cost = pair_info.map_or(dec!(0.5), |pi| pi.min_cost);
                if position.qty < min_qty || position.qty * mid < min_cost {
                    tracing::debug!(symbol, held = %position.qty, "Cannot sell — below exchange minimums");
                    return vec![];
                }
                let qty_dp = pair_info.map_or(8, |pi| pi.qty_decimals);
                truncate_decimal(qty.min(position.qty), qty_dp)
            } else {
                qty
            };

            // Place new order for this side
            if side == OrderSide::Buy {
                if self.sell_only_pairs.contains(symbol) {
                    return vec![];
                }
                if !can_open_buy(
                    &self.state,
                    symbol,
                    actual_qty * mid,
                    mid,
                    &self.prices,
                    &self.config.risk,
                ) {
                    return vec![];
                }
            }

            let cl_id = self.next_cl_ord_id(if side == OrderSide::Buy { "bid" } else { "ask" });
            self.state.open_orders.insert(
                cl_id.clone(),
                TrackedOrder {
                    cl_ord_id: cl_id.clone(),
                    symbol: symbol.to_string(),
                    side,
                    price,
                    qty: actual_qty,
                    placed_at: timestamp,
                    acked: false,
                },
            );

            let quoter = self.quoters.get_mut(symbol).unwrap();
            match side {
                OrderSide::Buy => quoter.bid_cl_ord_id = Some(cl_id.clone()),
                OrderSide::Sell => quoter.ask_cl_ord_id = Some(cl_id.clone()),
            }

            vec![EngineCommand::PlaceOrder(OrderRequest {
                cl_ord_id: cl_id,
                symbol: symbol.to_string(),
                side,
                price,
                qty: actual_qty,
                post_only: true,
                market: false,
            })]
        }
    }

    fn cancel_pair_quotes(&mut self, symbol: &str) -> Vec<EngineCommand> {
        let quoter = match self.quoters.get_mut(symbol) {
            Some(q) => q,
            None => return vec![],
        };

        let mut to_cancel = vec![];
        if let Some(id) = quoter.bid_cl_ord_id.take() {
            self.state.open_orders.remove(&id);
            to_cancel.push(id);
        }
        if let Some(id) = quoter.ask_cl_ord_id.take() {
            self.state.open_orders.remove(&id);
            to_cancel.push(id);
        }
        quoter.state = QuoteState::Idle;
        quoter.last_mid = None;

        if to_cancel.is_empty() {
            vec![]
        } else {
            vec![EngineCommand::CancelOrders(to_cancel)]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        let mut cfg = Config::default();
        cfg.trading.order_size_usd = dec!(100);
        cfg.trading.min_spread_bps = dec!(100);
        cfg.trading.maker_fee_pct = dec!(0.0023);
        cfg.risk.max_inventory_usd = dec!(200);
        cfg.risk.max_total_exposure_usd = dec!(2000);
        cfg.risk.kill_switch_loss_usd = dec!(-100);
        cfg
    }

    fn test_pair_info() -> HashMap<String, PairInfo> {
        let mut m = HashMap::new();
        m.insert(
            "TEST/USD".into(),
            PairInfo {
                symbol: "TEST/USD".into(),
                rest_key: "TESTUSD".into(),
                min_order_qty: dec!(1),
                min_cost: dec!(0.5),
                price_decimals: 5,
                qty_decimals: 4,
                maker_fee_pct: dec!(0.0023),
                base_asset: "TEST".into(),
            },
        );
        m
    }

    #[test]
    fn test_book_snapshot_triggers_quotes() {
        let mut engine = Engine::new(test_config(), test_pair_info(), BotState::default());
        let cmds = engine.handle_event(EngineEvent::BookSnapshot {
            symbol: "TEST/USD".into(),
            bids: vec![LevelUpdate { price: dec!(0.10), qty: dec!(100) }],
            asks: vec![LevelUpdate { price: dec!(0.12), qty: dec!(100) }],
            timestamp: Utc::now(),
        });

        // Should place bid only (no inventory to sell)
        let place_cmds: Vec<_> = cmds
            .iter()
            .filter(|c| matches!(c, EngineCommand::PlaceOrder(_)))
            .collect();
        assert_eq!(place_cmds.len(), 1);
        if let EngineCommand::PlaceOrder(req) = &place_cmds[0] {
            assert_eq!(req.side, OrderSide::Buy);
        }
    }

    #[test]
    fn test_narrow_spread_no_quote() {
        let mut engine = Engine::new(test_config(), test_pair_info(), BotState::default());
        let cmds = engine.handle_event(EngineEvent::BookSnapshot {
            symbol: "TEST/USD".into(),
            bids: vec![LevelUpdate { price: dec!(1.000), qty: dec!(100) }],
            asks: vec![LevelUpdate { price: dec!(1.001), qty: dec!(100) }],
            timestamp: Utc::now(),
        });

        // 0.1% spread < 1% min => no quotes
        let place_cmds: Vec<_> = cmds
            .iter()
            .filter(|c| matches!(c, EngineCommand::PlaceOrder(_)))
            .collect();
        assert_eq!(place_cmds.len(), 0);
    }

    #[test]
    fn test_fill_updates_pnl() {
        let mut engine = Engine::new(test_config(), test_pair_info(), BotState::default());

        // Buy
        engine.handle_event(EngineEvent::Fill(Fill {
            order_id: "o1".into(),
            cl_ord_id: "bid-1".into(),
            symbol: "TEST/USD".into(),
            side: OrderSide::Buy,
            price: dec!(0.10),
            qty: dec!(100),
            fee: dec!(0.023),
            is_maker: true,
            is_fully_filled: true,
            timestamp: Utc::now(),
        }));

        assert_eq!(engine.state.position("TEST/USD").qty, dec!(100));

        // Sell at profit
        let cmds = engine.handle_event(EngineEvent::Fill(Fill {
            order_id: "o2".into(),
            cl_ord_id: "ask-1".into(),
            symbol: "TEST/USD".into(),
            side: OrderSide::Sell,
            price: dec!(0.12),
            qty: dec!(100),
            fee: dec!(0.028),
            is_maker: true,
            is_fully_filled: true,
            timestamp: Utc::now(),
        }));

        assert!(engine.state.position("TEST/USD").is_empty());
        // pnl = 100 * (0.12 - 0.10) - 0.028 = 2.0 - 0.028 = 1.972
        assert_eq!(engine.state.realized_pnl, dec!(1.972));

        // Should have LogTrade and PersistState commands
        assert!(cmds.iter().any(|c| matches!(c, EngineCommand::LogTrade(_))));
        assert!(cmds.iter().any(|c| matches!(c, EngineCommand::PersistState(_))));
    }

    #[test]
    fn test_kill_switch_triggers_shutdown() {
        let mut cfg = test_config();
        cfg.risk.kill_switch_loss_usd = dec!(-1);

        let mut engine = Engine::new(cfg, test_pair_info(), BotState::default());

        // Buy at 0.10
        engine.handle_event(EngineEvent::Fill(Fill {
            order_id: "o1".into(),
            cl_ord_id: "bid-1".into(),
            symbol: "TEST/USD".into(),
            side: OrderSide::Buy,
            price: dec!(0.10),
            qty: dec!(100),
            fee: dec!(0.023),
            is_maker: true,
            is_fully_filled: true,
            timestamp: Utc::now(),
        }));

        // Sell at 0.05 — realize big loss: 100 * (0.05 - 0.10) - 0.5 = -5.5
        let cmds = engine.handle_event(EngineEvent::Fill(Fill {
            order_id: "o2".into(),
            cl_ord_id: "ask-1".into(),
            symbol: "TEST/USD".into(),
            side: OrderSide::Sell,
            price: dec!(0.05),
            qty: dec!(100),
            fee: dec!(0.5),
            is_maker: true,
            is_fully_filled: true,
            timestamp: Utc::now(),
        }));

        assert!(cmds.iter().any(|c| matches!(c, EngineCommand::CancelAll)));
        assert!(cmds.iter().any(|c| matches!(c, EngineCommand::Shutdown { .. })));
    }

    #[test]
    fn test_stop_loss_two_phase_liquidation() {
        let mut cfg = test_config();
        cfg.risk.stop_loss_pct = dec!(0.03); // 3% stop-loss

        let mut state = BotState::default();
        // Position bought at 0.10 avg_cost
        state.positions.insert("TEST/USD".into(), Position {
            qty: dec!(100),
            avg_cost: dec!(0.10),
        });

        let mut engine = Engine::new(cfg, test_pair_info(), state);

        // Feed book snapshot with mid at 0.096 (4% drop, > 3% threshold)
        engine.handle_event(EngineEvent::BookSnapshot {
            symbol: "TEST/USD".into(),
            bids: vec![LevelUpdate { price: dec!(0.095), qty: dec!(1000) }],
            asks: vec![LevelUpdate { price: dec!(0.097), qty: dec!(1000) }],
            timestamp: Utc::now(),
        });

        // PHASE 1: First tick triggers stop-loss — should cancel orders, NOT place sell yet
        let cmds1 = engine.handle_event(EngineEvent::Tick {
            timestamp: Utc::now(),
        });

        // No sell orders in phase 1 (only cancels)
        let sell_orders1: Vec<_> = cmds1.iter()
            .filter_map(|c| {
                if let EngineCommand::PlaceOrder(req) = c {
                    if req.side == OrderSide::Sell { return Some(req); }
                }
                None
            })
            .collect();
        assert!(sell_orders1.is_empty(), "Phase 1 should NOT place sell — only cancel");

        // Pair should be marked disabled and pending liquidation
        assert!(engine.disabled_pairs.contains("TEST/USD"));
        assert!(engine.pending_liquidation.contains("TEST/USD"));

        // PHASE 2: Second tick — orders cleared, now send the market sell
        let cmds2 = engine.handle_event(EngineEvent::Tick {
            timestamp: Utc::now(),
        });

        let sell_orders2: Vec<_> = cmds2.iter()
            .filter_map(|c| {
                if let EngineCommand::PlaceOrder(req) = c {
                    if req.side == OrderSide::Sell { return Some(req); }
                }
                None
            })
            .collect();

        assert!(!sell_orders2.is_empty(), "Phase 2 should place liquidation sell");
        assert!(sell_orders2[0].market, "Liquidation should be a market order");
        assert_eq!(sell_orders2[0].post_only, false, "Liquidation should not be post_only");
        assert_eq!(sell_orders2[0].qty, dec!(100), "Should sell entire position");

        // Third tick should NOT re-send (liq order is pending)
        let cmds3 = engine.handle_event(EngineEvent::Tick {
            timestamp: Utc::now(),
        });
        let sell_orders3: Vec<_> = cmds3.iter()
            .filter_map(|c| {
                if let EngineCommand::PlaceOrder(req) = c {
                    if req.side == OrderSide::Sell { return Some(req); }
                }
                None
            })
            .collect();
        assert!(sell_orders3.is_empty(), "Should NOT re-send while liq order pending");
    }
}
