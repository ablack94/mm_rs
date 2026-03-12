//! Trend Trader -- EMA crossover trend-following bracket trader
//!
//! Volume-farming strategy for BTC/USDC, ETH/USDC, SOL/USDC:
//! 1. Track mid prices, compute fast EMA (20-period) and slow EMA (50-period) on ~10s intervals
//! 2. When fast EMA crosses above slow EMA -> BUY at market (limit at best ask)
//! 3. Once filled, place take-profit sell at +0.3% and stop-loss sell at -0.2%
//! 4. When either bracket leg fills, cancel the other, go back to watching
//! 5. Only enter when trend is up (fast EMA > slow EMA)
//! 6. Max 1 concurrent position per pair
//!
//! Uses the existing Coinbase proxy infrastructure.

use anyhow::{bail, Result};
use exchange_api::{parse_proxy_event, BookLevel, OrderPriority, ProxyCommand, ProxyEvent};
use futures::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Config {
    proxy_url: String,
    proxy_token: String,
    pairs: Vec<String>,
    size_usd: Decimal,
    /// Take-profit percentage above entry price.
    take_profit_pct: Decimal,
    /// Stop-loss percentage below entry price.
    stop_loss_pct: Decimal,
    /// Fast EMA period (number of samples).
    fast_ema_period: usize,
    /// Slow EMA period (number of samples).
    slow_ema_period: usize,
    /// Sampling interval in seconds for EMA updates.
    sample_interval_secs: u64,
    /// Minimum number of EMA samples before trading is allowed.
    min_samples: usize,
}

impl Config {
    fn from_env() -> Self {
        let pairs_str = env_or("TREND_PAIRS", "BTC/USDC,ETH/USDC,SOL/USDC");
        let pairs: Vec<String> = pairs_str
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        Self {
            proxy_url: env_or("PROXY_URL", "http://localhost:8081"),
            proxy_token: env_or("PROXY_TOKEN", "mm-ops"),
            pairs,
            size_usd: env_decimal("TREND_ORDER_SIZE_USD", dec!(50)),
            take_profit_pct: env_decimal("TREND_TAKE_PROFIT_PCT", dec!(0.003)),
            stop_loss_pct: env_decimal("TREND_STOP_LOSS_PCT", dec!(0.002)),
            fast_ema_period: env_usize("TREND_FAST_EMA_PERIOD", 20),
            slow_ema_period: env_usize("TREND_SLOW_EMA_PERIOD", 50),
            sample_interval_secs: std::env::var("TREND_SAMPLE_INTERVAL_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(10),
            min_samples: env_usize("TREND_MIN_SAMPLES", 50),
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_decimal(key: &str, default: Decimal) -> Decimal {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse::<Decimal>().ok())
        .unwrap_or(default)
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// EMA tracker
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Ema {
    #[allow(dead_code)]
    period: usize,
    multiplier: Decimal,
    value: Option<Decimal>,
    sample_count: usize,
}

impl Ema {
    fn new(period: usize) -> Self {
        // EMA multiplier = 2 / (period + 1)
        let multiplier = dec!(2) / Decimal::from(period as u64 + 1);
        Self {
            period,
            multiplier,
            value: None,
            sample_count: 0,
        }
    }

    fn update(&mut self, price: Decimal) {
        self.sample_count += 1;
        match self.value {
            None => {
                self.value = Some(price);
            }
            Some(prev) => {
                // EMA = price * multiplier + prev_ema * (1 - multiplier)
                let new = price * self.multiplier + prev * (Decimal::ONE - self.multiplier);
                self.value = Some(new);
            }
        }
    }

    fn get(&self) -> Option<Decimal> {
        self.value
    }
}

// ---------------------------------------------------------------------------
// Per-pair state
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct PairTracker {
    symbol: String,
    fast_ema: Ema,
    slow_ema: Ema,
    /// Whether fast EMA was above slow EMA on the previous sample (for crossover detection).
    was_fast_above: Option<bool>,
    /// Current best bid from the order book.
    best_bid: Option<Decimal>,
    /// Current best ask from the order book.
    best_ask: Option<Decimal>,
    /// Current position state.
    position: PositionState,
    /// Pair info (decimals, minimums) from the exchange.
    pair_info: Option<PairInfo>,
    /// Total samples received.
    sample_count: usize,
    /// Stats tracking.
    stats: PairStats,
}

#[derive(Debug)]
struct PairStats {
    total_entries: u32,
    wins: u32,
    losses: u32,
    total_pnl: Decimal,
}

impl PairStats {
    fn new() -> Self {
        Self {
            total_entries: 0,
            wins: 0,
            losses: 0,
            total_pnl: Decimal::ZERO,
        }
    }
}

#[derive(Debug)]
enum PositionState {
    /// No position, watching for EMA crossover.
    Flat,
    /// Buy order placed, waiting for fill.
    BuyPending {
        cl_ord_id: String,
        price: Decimal,
        #[allow(dead_code)]
        qty: Decimal,
    },
    /// Bought, bracket orders (TP + SL) placed.
    InPosition {
        entry_price: Decimal,
        entry_qty: Decimal,
        tp_cl_ord_id: String,
        tp_price: Decimal,
        sl_cl_ord_id: String,
        sl_price: Decimal,
    },
    /// One bracket leg filled, cancelling the other.
    Exiting {
        cancel_cl_ord_id: String,
        result_pnl: Decimal,
        was_tp: bool,
        entered_at: std::time::Instant,
    },
}

impl std::fmt::Display for PositionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PositionState::Flat => write!(f, "Flat"),
            PositionState::BuyPending { price, .. } => write!(f, "BuyPending({})", price),
            PositionState::InPosition {
                entry_price,
                tp_price,
                sl_price,
                ..
            } => write!(
                f,
                "InPosition(entry={}, tp={}, sl={})",
                entry_price, tp_price, sl_price
            ),
            PositionState::Exiting { was_tp, .. } => {
                write!(f, "Exiting(tp={})", was_tp)
            }
        }
    }
}

impl PairTracker {
    fn new(symbol: String, fast_period: usize, slow_period: usize) -> Self {
        Self {
            symbol,
            fast_ema: Ema::new(fast_period),
            slow_ema: Ema::new(slow_period),
            was_fast_above: None,
            best_bid: None,
            best_ask: None,
            position: PositionState::Flat,
            pair_info: None,
            sample_count: 0,
            stats: PairStats::new(),
        }
    }

    fn mid_price(&self) -> Option<Decimal> {
        match (self.best_bid, self.best_ask) {
            (Some(bid), Some(ask)) if bid > Decimal::ZERO && ask > Decimal::ZERO => {
                Some((bid + ask) / dec!(2))
            }
            _ => None,
        }
    }

    /// Update EMAs with current mid price. Returns true if a bullish crossover just occurred.
    fn sample_ema(&mut self) -> bool {
        let mid = match self.mid_price() {
            Some(m) => m,
            None => return false,
        };

        self.fast_ema.update(mid);
        self.slow_ema.update(mid);
        self.sample_count += 1;

        let (fast, slow) = match (self.fast_ema.get(), self.slow_ema.get()) {
            (Some(f), Some(s)) => (f, s),
            _ => return false,
        };

        let is_fast_above = fast > slow;
        let crossover = match self.was_fast_above {
            Some(was_above) => !was_above && is_fast_above, // crossed from below to above
            None => false,
        };
        self.was_fast_above = Some(is_fast_above);

        crossover
    }

    /// Whether the trend is currently bullish (fast EMA > slow EMA).
    fn is_trend_up(&self) -> bool {
        match (self.fast_ema.get(), self.slow_ema.get()) {
            (Some(f), Some(s)) => f > s,
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Pair info from REST
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct PairInfo {
    price_decimals: u32,
    qty_decimals: u32,
    min_order_qty: Decimal,
    #[allow(dead_code)]
    min_cost: Decimal,
}

async fn fetch_pair_info(config: &Config) -> Result<HashMap<String, PairInfo>> {
    let client = reqwest::Client::new();

    // Convert BTC/USDC -> BTC-USDC for REST query
    let rest_pairs: Vec<String> = config.pairs.iter().map(|p| p.replace('/', "-")).collect();
    let pair_param = rest_pairs.join(",");

    let resp: serde_json::Value = client
        .get(format!(
            "{}/0/public/AssetPairs?pair={}",
            config.proxy_url, pair_param
        ))
        .header("Authorization", format!("Bearer {}", config.proxy_token))
        .send()
        .await?
        .json()
        .await?;

    let result = resp["result"]
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("No 'result' in AssetPairs response: {}", resp))?;

    let mut infos = HashMap::new();

    for (_key, info) in result {
        let wsname = info["wsname"].as_str().unwrap_or_default().to_string();
        if config.pairs.contains(&wsname) {
            let pi = PairInfo {
                price_decimals: info["pair_decimals"].as_u64().unwrap_or(2) as u32,
                qty_decimals: info["lot_decimals"].as_u64().unwrap_or(8) as u32,
                min_order_qty: info["ordermin"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(dec!(0.00001)),
                min_cost: info["costmin"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(dec!(1)),
            };
            infos.insert(wsname, pi);
        }
    }

    Ok(infos)
}

// ---------------------------------------------------------------------------
// Rounding helpers
// ---------------------------------------------------------------------------

fn round_price(price: Decimal, decimals: u32) -> Decimal {
    price.round_dp(decimals)
}

fn round_qty_down(qty: Decimal, decimals: u32) -> Decimal {
    let mut factor = Decimal::ONE;
    for _ in 0..decimals {
        factor *= Decimal::TEN;
    }
    (qty * factor).trunc() / factor
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

enum TrendEvent {
    /// Book data updated for a pair.
    BookData {
        symbol: String,
        bids: Vec<BookLevel>,
        asks: Vec<BookLevel>,
    },
    /// EMA sampling tick (every sample_interval_secs).
    SampleTick,
    /// Order accepted by exchange.
    OrderAccepted {
        cl_ord_id: String,
    },
    /// Fill notification.
    Fill {
        cl_ord_id: String,
        symbol: String,
        side: String,
        price: Decimal,
        qty: Decimal,
        fee: Decimal,
    },
    /// Order cancelled.
    OrderCancelled {
        cl_ord_id: String,
        reason: Option<String>,
    },
    /// Order rejected.
    OrderRejected {
        cl_ord_id: String,
        error: String,
    },
    /// Shutdown signal.
    Shutdown,
}

// ---------------------------------------------------------------------------
// Global req_id counter
// ---------------------------------------------------------------------------

static REQ_ID: AtomicU64 = AtomicU64::new(1);

fn next_req_id() -> u64 {
    REQ_ID.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// WS helpers
// ---------------------------------------------------------------------------

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn ws_connect(url: &str, token: &str) -> Result<WsStream> {
    let mut request = url.into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", token).parse()?,
    );
    let (ws, _) = connect_async(request).await?;
    Ok(ws)
}

async fn ws_send(writer: &mpsc::Sender<String>, msg: &impl serde::Serialize) -> Result<()> {
    let text = serde_json::to_string(msg)?;
    tracing::debug!(msg = %text, "WS send");
    writer
        .send(text)
        .await
        .map_err(|_| anyhow::anyhow!("WS write channel closed"))
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "trend_trader=info,warn".to_string()),
        )
        .init();

    let config = Config::from_env();

    tracing::info!(
        pairs = ?config.pairs,
        size_usd = %config.size_usd,
        take_profit_pct = %config.take_profit_pct,
        stop_loss_pct = %config.stop_loss_pct,
        fast_ema = config.fast_ema_period,
        slow_ema = config.slow_ema_period,
        sample_interval = config.sample_interval_secs,
        min_samples = config.min_samples,
        "Trend Trader starting"
    );

    // Fetch pair info for all pairs
    let pair_infos = fetch_pair_info(&config).await?;
    for (sym, pi) in &pair_infos {
        tracing::info!(
            pair = sym,
            price_decimals = pi.price_decimals,
            qty_decimals = pi.qty_decimals,
            min_order_qty = %pi.min_order_qty,
            "Pair info"
        );
    }

    // Validate all pairs have info
    for pair in &config.pairs {
        if !pair_infos.contains_key(pair) {
            bail!(
                "No pair info for {}. Available: {:?}",
                pair,
                pair_infos.keys().collect::<Vec<_>>()
            );
        }
    }

    // Initialize per-pair trackers
    let mut trackers: HashMap<String, PairTracker> = HashMap::new();
    for pair in &config.pairs {
        let mut tracker =
            PairTracker::new(pair.clone(), config.fast_ema_period, config.slow_ema_period);
        tracker.pair_info = pair_infos.get(pair).cloned();
        trackers.insert(pair.clone(), tracker);
    }

    // Derive WS URLs
    let ws_base = config
        .proxy_url
        .trim_end_matches('/')
        .replacen("http://", "ws://", 1)
        .replacen("https://", "wss://", 1);
    let pub_ws_url = format!("{}/ws/public", ws_base);
    let priv_ws_url = format!("{}/ws/private", ws_base);

    // Connect public WS
    let pub_ws = ws_connect(&pub_ws_url, &config.proxy_token).await?;
    tracing::info!(url = %pub_ws_url, "Public WS connected");
    let (mut pub_write, mut pub_read) = pub_ws.split();

    // Subscribe to book data for all pairs
    let sub = ProxyCommand::Subscribe {
        channel: "book".to_string(),
        symbols: config.pairs.clone(),
        depth: Some(10),
    };
    let sub_text = serde_json::to_string(&sub)?;
    pub_write.send(Message::Text(sub_text.into())).await?;
    tracing::info!(pairs = ?config.pairs, "Subscribed to book data");

    // Connect private WS
    let priv_ws = ws_connect(&priv_ws_url, &config.proxy_token).await?;
    tracing::info!(url = %priv_ws_url, "Private WS connected");
    let (priv_write_half, mut priv_read) = priv_ws.split();

    // Private WS write channel
    let (priv_tx, mut priv_rx) = mpsc::channel::<String>(64);
    tokio::spawn(async move {
        let mut writer = priv_write_half;
        while let Some(msg) = priv_rx.recv().await {
            if let Err(e) = writer.send(Message::Text(msg.into())).await {
                tracing::error!(error = %e, "Private WS write error");
                break;
            }
        }
    });

    // Event channel
    let (event_tx, mut event_rx) = mpsc::channel::<TrendEvent>(512);

    // Spawn public WS reader
    let pub_event_tx = event_tx.clone();
    let tracked_pairs: Vec<String> = config.pairs.clone();
    tokio::spawn(async move {
        while let Some(Ok(msg)) = pub_read.next().await {
            let text = match msg {
                Message::Text(t) => t.to_string(),
                _ => continue,
            };
            match parse_proxy_event(&text) {
                Ok(ProxyEvent::BookSnapshot {
                    symbol, bids, asks, ..
                })
                | Ok(ProxyEvent::BookUpdate {
                    symbol, bids, asks, ..
                }) => {
                    if tracked_pairs.contains(&symbol) {
                        let _ = pub_event_tx
                            .send(TrendEvent::BookData {
                                symbol,
                                bids,
                                asks,
                            })
                            .await;
                    }
                }
                Ok(ProxyEvent::Subscribed { channel }) => {
                    tracing::info!(channel, "Subscription confirmed");
                }
                Ok(ProxyEvent::Heartbeat) => {}
                Ok(_) => {}
                Err(_) => {
                    tracing::trace!(raw = %text, "Unparsed public WS");
                }
            }
        }
        tracing::warn!("Public WS closed");
    });

    // Spawn private WS reader
    let priv_event_tx = event_tx.clone();
    tokio::spawn(async move {
        while let Some(Ok(msg)) = priv_read.next().await {
            let text = match msg {
                Message::Text(t) => t.to_string(),
                _ => continue,
            };
            match parse_proxy_event(&text) {
                Ok(ProxyEvent::OrderAccepted { cl_ord_id, .. }) => {
                    let _ = priv_event_tx
                        .send(TrendEvent::OrderAccepted { cl_ord_id })
                        .await;
                }
                Ok(ProxyEvent::Fill {
                    cl_ord_id,
                    symbol,
                    side,
                    price,
                    qty,
                    fee,
                    ..
                }) => {
                    let _ = priv_event_tx
                        .send(TrendEvent::Fill {
                            cl_ord_id,
                            symbol,
                            side,
                            price,
                            qty,
                            fee,
                        })
                        .await;
                }
                Ok(ProxyEvent::OrderCancelled {
                    cl_ord_id, reason, ..
                }) => {
                    let _ = priv_event_tx
                        .send(TrendEvent::OrderCancelled { cl_ord_id, reason })
                        .await;
                }
                Ok(ProxyEvent::OrderRejected {
                    cl_ord_id, error, ..
                }) => {
                    let _ = priv_event_tx
                        .send(TrendEvent::OrderRejected { cl_ord_id, error })
                        .await;
                }
                Ok(ProxyEvent::CommandAck { req_id, cmd }) => {
                    tracing::debug!(req_id, cmd, "Command ack");
                }
                Ok(ProxyEvent::Heartbeat) | Ok(ProxyEvent::Pong { .. }) => {}
                Ok(_) => {}
                Err(_) => {
                    tracing::trace!(raw = %text, "Unparsed private WS");
                }
            }
        }
        tracing::warn!("Private WS closed");
    });

    // Spawn EMA sampling ticker
    let tick_tx = event_tx.clone();
    let sample_interval = config.sample_interval_secs;
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(sample_interval));
        loop {
            interval.tick().await;
            if tick_tx.send(TrendEvent::SampleTick).await.is_err() {
                break;
            }
        }
    });

    // Spawn SIGINT handler
    let shutdown_tx = event_tx.clone();
    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            tracing::info!("SIGINT received, shutting down");
            let _ = shutdown_tx.send(TrendEvent::Shutdown).await;
        }
    });

    // Drop our clone so channel closes when all producers exit
    drop(event_tx);

    // -----------------------------------------------------------------------
    // Main event loop
    // -----------------------------------------------------------------------

    tracing::info!("Entering main event loop");

    while let Some(event) = event_rx.recv().await {
        match event {
            TrendEvent::BookData {
                symbol,
                bids,
                asks,
            } => {
                if let Some(tracker) = trackers.get_mut(&symbol) {
                    // Update best bid/ask from the book levels
                    if let Some(best_bid) = bids.iter().map(|l| l.price).max() {
                        tracker.best_bid = Some(best_bid);
                    }
                    if let Some(best_ask) = asks.iter().map(|l| l.price).min() {
                        tracker.best_ask = Some(best_ask);
                    }
                }
            }

            TrendEvent::SampleTick => {
                // Check for stale Exiting states (timeout after 30s)
                for (_symbol, tracker) in trackers.iter_mut() {
                    if let PositionState::Exiting { entered_at, result_pnl, was_tp, .. } = &tracker.position {
                        if entered_at.elapsed() > std::time::Duration::from_secs(30) {
                            tracing::warn!(
                                pair = tracker.symbol.as_str(),
                                pnl = %result_pnl,
                                exit_type = if *was_tp { "TP" } else { "SL" },
                                elapsed_secs = entered_at.elapsed().as_secs(),
                                "Exiting state timed out — forcing to Flat"
                            );
                            tracker.position = PositionState::Flat;
                        }
                    }
                }

                // Sample EMAs for all pairs and check for crossovers
                let mut entries_to_make: Vec<(String, Decimal, Decimal)> = Vec::new();

                for (symbol, tracker) in trackers.iter_mut() {
                    let crossover = tracker.sample_ema();

                    // Log EMA values periodically (every 30 samples ~ 5 min at 10s interval)
                    if tracker.sample_count % 30 == 0 && tracker.sample_count > 0 {
                        if let (Some(fast), Some(slow)) =
                            (tracker.fast_ema.get(), tracker.slow_ema.get())
                        {
                            tracing::info!(
                                pair = symbol.as_str(),
                                samples = tracker.sample_count,
                                fast_ema = %fast.round_dp(4),
                                slow_ema = %slow.round_dp(4),
                                trend = if tracker.is_trend_up() { "UP" } else { "DOWN" },
                                mid = %tracker.mid_price().unwrap_or(Decimal::ZERO).round_dp(4),
                                state = %tracker.position,
                                "EMA status"
                            );
                        }
                    }

                    // Check if we should enter
                    if crossover
                        && tracker.sample_count >= config.min_samples
                        && matches!(tracker.position, PositionState::Flat)
                    {
                        if let (Some(_ask), Some(bid)) = (tracker.best_ask, tracker.best_bid) {
                            entries_to_make.push((symbol.clone(), bid, bid));
                        }
                    }
                }

                // Place buy orders for crossovers
                for (symbol, best_ask, _) in entries_to_make {
                    let tracker = trackers.get_mut(&symbol).unwrap();
                    let pair_info = match &tracker.pair_info {
                        Some(pi) => pi.clone(),
                        None => continue,
                    };

                    let buy_price = round_price(best_ask, pair_info.price_decimals);
                    let buy_qty = round_qty_down(config.size_usd / buy_price, pair_info.qty_decimals);

                    if buy_qty < pair_info.min_order_qty {
                        tracing::warn!(
                            pair = symbol.as_str(),
                            qty = %buy_qty,
                            min = %pair_info.min_order_qty,
                            "Computed qty below minimum, skipping entry"
                        );
                        continue;
                    }

                    let cl_ord_id = format!(
                        "trend-buy-{}-{}",
                        symbol.replace('/', "").to_lowercase(),
                        &uuid::Uuid::new_v4().to_string()[..8]
                    );

                    tracing::info!(
                        pair = symbol.as_str(),
                        cl_ord_id = cl_ord_id.as_str(),
                        price = %buy_price,
                        qty = %buy_qty,
                        fast_ema = %tracker.fast_ema.get().unwrap_or(Decimal::ZERO).round_dp(4),
                        slow_ema = %tracker.slow_ema.get().unwrap_or(Decimal::ZERO).round_dp(4),
                        "EMA CROSSOVER - placing buy"
                    );

                    let cmd = ProxyCommand::PlaceOrder {
                        req_id: next_req_id(),
                        symbol: symbol.clone(),
                        side: "buy".to_string(),
                        order_type: "limit".to_string(),
                        price: buy_price,
                        qty: buy_qty,
                        cl_ord_id: cl_ord_id.clone(),
                        post_only: false,
                        priority: OrderPriority::Normal,
                        market: false,
                    };
                    ws_send(&priv_tx, &cmd).await?;

                    tracker.position = PositionState::BuyPending {
                        cl_ord_id,
                        price: buy_price,
                        qty: buy_qty,
                    };
                }
            }

            TrendEvent::OrderAccepted { cl_ord_id } => {
                tracing::debug!(cl_ord_id, "Order accepted");
            }

            TrendEvent::Fill {
                cl_ord_id,
                symbol,
                side,
                price,
                qty,
                fee,
            } => {
                let tracker = match trackers.get_mut(&symbol) {
                    Some(t) => t,
                    None => {
                        // Try to find by cl_ord_id prefix
                        let found = trackers
                            .values_mut()
                            .find(|t| match &t.position {
                                PositionState::BuyPending { cl_ord_id: id, .. } => *id == cl_ord_id,
                                PositionState::InPosition {
                                    tp_cl_ord_id,
                                    sl_cl_ord_id,
                                    ..
                                } => *tp_cl_ord_id == cl_ord_id || *sl_cl_ord_id == cl_ord_id,
                                _ => false,
                            });
                        match found {
                            Some(t) => t,
                            None => {
                                tracing::warn!(
                                    cl_ord_id,
                                    symbol,
                                    side,
                                    price = %price,
                                    qty = %qty,
                                    "Fill for unknown pair/order"
                                );
                                continue;
                            }
                        }
                    }
                };

                match &tracker.position {
                    PositionState::BuyPending {
                        cl_ord_id: buy_id, ..
                    } if *buy_id == cl_ord_id => {
                        let pair_info = tracker.pair_info.clone().unwrap();
                        let entry_price = price;
                        let entry_qty = round_qty_down(qty, pair_info.qty_decimals);

                        // Place bracket: take-profit and stop-loss
                        let tp_price = round_price(
                            entry_price * (Decimal::ONE + config.take_profit_pct),
                            pair_info.price_decimals,
                        );
                        let sl_price = round_price(
                            entry_price * (Decimal::ONE - config.stop_loss_pct),
                            pair_info.price_decimals,
                        );

                        let pair_slug = tracker
                            .symbol
                            .replace('/', "")
                            .to_lowercase();
                        let tp_cl_ord_id = format!(
                            "trend-tp-{}-{}",
                            pair_slug,
                            &uuid::Uuid::new_v4().to_string()[..8]
                        );
                        let sl_cl_ord_id = format!(
                            "trend-sl-{}-{}",
                            pair_slug,
                            &uuid::Uuid::new_v4().to_string()[..8]
                        );

                        tracing::info!(
                            pair = tracker.symbol.as_str(),
                            entry_price = %entry_price,
                            entry_qty = %entry_qty,
                            fee = %fee,
                            tp_price = %tp_price,
                            sl_price = %sl_price,
                            tp_id = tp_cl_ord_id.as_str(),
                            sl_id = sl_cl_ord_id.as_str(),
                            "BUY FILLED - placing TP + SL bracket"
                        );

                        // Place take-profit sell (limit, maker)
                        let tp_cmd = ProxyCommand::PlaceOrder {
                            req_id: next_req_id(),
                            symbol: tracker.symbol.clone(),
                            side: "sell".to_string(),
                            order_type: "limit".to_string(),
                            price: tp_price,
                            qty: entry_qty,
                            cl_ord_id: tp_cl_ord_id.clone(),
                            post_only: false,
                            priority: OrderPriority::Normal,
                            market: false,
                        };
                        ws_send(&priv_tx, &tp_cmd).await?;

                        // Place stop-loss sell (limit maker)
                        let sl_cmd = ProxyCommand::PlaceOrder {
                            req_id: next_req_id(),
                            symbol: tracker.symbol.clone(),
                            side: "sell".to_string(),
                            order_type: "limit".to_string(),
                            price: sl_price,
                            qty: entry_qty,
                            cl_ord_id: sl_cl_ord_id.clone(),
                            post_only: false,
                            priority: OrderPriority::Normal,
                            market: false,
                        };
                        ws_send(&priv_tx, &sl_cmd).await?;

                        tracker.position = PositionState::InPosition {
                            entry_price,
                            entry_qty,
                            tp_cl_ord_id,
                            tp_price,
                            sl_cl_ord_id,
                            sl_price,
                        };
                        tracker.stats.total_entries += 1;
                    }

                    PositionState::InPosition {
                        entry_price,
                        tp_cl_ord_id,
                        sl_cl_ord_id,
                        ..
                    } => {
                        let is_tp = cl_ord_id == *tp_cl_ord_id;
                        let is_sl = cl_ord_id == *sl_cl_ord_id;

                        if is_tp || is_sl {
                            let pnl = (price - *entry_price) * qty;
                            let cancel_id = if is_tp {
                                sl_cl_ord_id.clone()
                            } else {
                                tp_cl_ord_id.clone()
                            };

                            tracing::info!(
                                pair = tracker.symbol.as_str(),
                                exit_type = if is_tp { "TAKE_PROFIT" } else { "STOP_LOSS" },
                                entry_price = %entry_price,
                                exit_price = %price,
                                qty = %qty,
                                fee = %fee,
                                pnl = %pnl,
                                "BRACKET EXIT - cancelling other leg"
                            );

                            // Cancel the other leg
                            let cancel_cmd = ProxyCommand::CancelOrders {
                                req_id: next_req_id(),
                                cl_ord_ids: vec![cancel_id.clone()],
                            };
                            ws_send(&priv_tx, &cancel_cmd).await?;

                            // Update stats
                            if is_tp {
                                tracker.stats.wins += 1;
                            } else {
                                tracker.stats.losses += 1;
                            }
                            tracker.stats.total_pnl += pnl;

                            tracker.position = PositionState::Exiting {
                                cancel_cl_ord_id: cancel_id,
                                result_pnl: pnl,
                                was_tp: is_tp,
                                entered_at: std::time::Instant::now(),
                            };
                        } else {
                            tracing::warn!(
                                cl_ord_id,
                                pair = tracker.symbol.as_str(),
                                side,
                                "Unexpected fill while in position"
                            );
                        }
                    }

                    _ => {
                        tracing::warn!(
                            cl_ord_id,
                            symbol = tracker.symbol.as_str(),
                            side,
                            state = %tracker.position,
                            "Fill in unexpected state"
                        );
                    }
                }
            }

            TrendEvent::OrderCancelled { cl_ord_id, reason } => {
                tracing::debug!(cl_ord_id, ?reason, "Order cancelled");

                // Find which tracker owns this cl_ord_id
                let tracker = trackers.values_mut().find(|t| match &t.position {
                    PositionState::BuyPending { cl_ord_id: id, .. } => *id == cl_ord_id,
                    PositionState::InPosition {
                        tp_cl_ord_id,
                        sl_cl_ord_id,
                        ..
                    } => *tp_cl_ord_id == cl_ord_id || *sl_cl_ord_id == cl_ord_id,
                    PositionState::Exiting {
                        cancel_cl_ord_id, ..
                    } => *cancel_cl_ord_id == cl_ord_id,
                    _ => false,
                });

                if let Some(tracker) = tracker {
                    match &tracker.position {
                        PositionState::BuyPending { .. } => {
                            tracing::warn!(
                                pair = tracker.symbol.as_str(),
                                "Buy cancelled before fill, returning to flat"
                            );
                            tracker.position = PositionState::Flat;
                        }
                        PositionState::InPosition {
                            entry_price,
                            entry_qty,
                            tp_cl_ord_id,
                            sl_cl_ord_id,
                            ..
                        } => {
                            // One of our bracket legs was cancelled externally.
                            // This is bad - we still have a position. Place an emergency market sell.
                            let is_tp_cancelled = cl_ord_id == *tp_cl_ord_id;
                            tracing::warn!(
                                pair = tracker.symbol.as_str(),
                                cancelled_leg = if is_tp_cancelled { "TP" } else { "SL" },
                                "Bracket leg cancelled externally! Cancelling other leg and market selling"
                            );

                            // Cancel the remaining leg too
                            let other_id = if is_tp_cancelled {
                                sl_cl_ord_id.clone()
                            } else {
                                tp_cl_ord_id.clone()
                            };
                            let cancel_cmd = ProxyCommand::CancelOrders {
                                req_id: next_req_id(),
                                cl_ord_ids: vec![other_id],
                            };
                            ws_send(&priv_tx, &cancel_cmd).await?;

                            // Emergency market sell
                            let sell_price = tracker
                                .best_bid
                                .unwrap_or(*entry_price);
                            let pair_info = tracker.pair_info.clone().unwrap();
                            let sell_price =
                                round_price(sell_price, pair_info.price_decimals);

                            let emrg_id = format!(
                                "trend-emrg-{}",
                                &uuid::Uuid::new_v4().to_string()[..8]
                            );

                            let sell_cmd = ProxyCommand::PlaceOrder {
                                req_id: next_req_id(),
                                symbol: tracker.symbol.clone(),
                                side: "sell".to_string(),
                                order_type: "limit".to_string(),
                                price: sell_price,
                                qty: *entry_qty,
                                cl_ord_id: emrg_id,
                                post_only: false,
                                priority: OrderPriority::Urgent,
                                market: true,
                            };
                            ws_send(&priv_tx, &sell_cmd).await?;

                            // Go flat - the emergency sell will fill asynchronously
                            // (not ideal but keeps the state machine simple)
                            tracker.stats.losses += 1;
                            tracker.position = PositionState::Flat;
                        }
                        PositionState::Exiting {
                            result_pnl, was_tp, ..
                        } => {
                            // Expected cancel of the other bracket leg
                            tracing::info!(
                                pair = tracker.symbol.as_str(),
                                pnl = %result_pnl,
                                exit_type = if *was_tp { "TP" } else { "SL" },
                                wins = tracker.stats.wins,
                                losses = tracker.stats.losses,
                                total_pnl = %tracker.stats.total_pnl,
                                "Bracket complete, returning to flat"
                            );
                            tracker.position = PositionState::Flat;
                        }
                        _ => {}
                    }
                }
            }

            TrendEvent::OrderRejected { cl_ord_id, error } => {
                tracing::error!(cl_ord_id, error, "Order rejected");

                let tracker = trackers.values_mut().find(|t| match &t.position {
                    PositionState::BuyPending { cl_ord_id: id, .. } => *id == cl_ord_id,
                    PositionState::InPosition {
                        tp_cl_ord_id,
                        sl_cl_ord_id,
                        ..
                    } => *tp_cl_ord_id == cl_ord_id || *sl_cl_ord_id == cl_ord_id,
                    PositionState::Exiting { cancel_cl_ord_id, .. } => *cancel_cl_ord_id == cl_ord_id,
                    _ => false,
                });

                if let Some(tracker) = tracker {
                    match &tracker.position {
                        PositionState::BuyPending { .. } => {
                            tracing::warn!(
                                pair = tracker.symbol.as_str(),
                                error,
                                "Buy rejected, returning to flat"
                            );
                            tracker.position = PositionState::Flat;
                        }
                        PositionState::InPosition {
                            entry_price,
                            entry_qty,
                            tp_cl_ord_id,
                            sl_cl_ord_id,
                            ..
                        } => {
                            // A bracket leg was rejected. Cancel the other and market sell.
                            let is_tp_rejected = cl_ord_id == *tp_cl_ord_id;
                            tracing::warn!(
                                pair = tracker.symbol.as_str(),
                                rejected_leg = if is_tp_rejected { "TP" } else { "SL" },
                                error,
                                "Bracket leg rejected! Emergency exit"
                            );

                            let other_id = if is_tp_rejected {
                                sl_cl_ord_id.clone()
                            } else {
                                tp_cl_ord_id.clone()
                            };
                            let cancel_cmd = ProxyCommand::CancelOrders {
                                req_id: next_req_id(),
                                cl_ord_ids: vec![other_id],
                            };
                            ws_send(&priv_tx, &cancel_cmd).await?;

                            // Emergency market sell
                            let sell_price = tracker.best_bid.unwrap_or(*entry_price);
                            let pair_info = tracker.pair_info.clone().unwrap();
                            let sell_price =
                                round_price(sell_price, pair_info.price_decimals);

                            let emrg_id = format!(
                                "trend-emrg-{}",
                                &uuid::Uuid::new_v4().to_string()[..8]
                            );

                            let sell_cmd = ProxyCommand::PlaceOrder {
                                req_id: next_req_id(),
                                symbol: tracker.symbol.clone(),
                                side: "sell".to_string(),
                                order_type: "limit".to_string(),
                                price: sell_price,
                                qty: *entry_qty,
                                cl_ord_id: emrg_id,
                                post_only: false,
                                priority: OrderPriority::Urgent,
                                market: true,
                            };
                            ws_send(&priv_tx, &sell_cmd).await?;

                            tracker.stats.losses += 1;
                            tracker.position = PositionState::Flat;
                        }
                        PositionState::Exiting { result_pnl, was_tp, .. } => {
                            tracing::warn!(
                                pair = tracker.symbol.as_str(),
                                pnl = %result_pnl,
                                exit_type = if *was_tp { "TP" } else { "SL" },
                                error,
                                "Cancel rejected in Exiting state — forcing to Flat"
                            );
                            tracker.position = PositionState::Flat;
                        }
                        _ => {}
                    }
                }
            }

            TrendEvent::Shutdown => {
                tracing::info!("Shutdown: cancelling all orders");
                let cancel_all = ProxyCommand::CancelAll {
                    req_id: next_req_id(),
                };
                ws_send(&priv_tx, &cancel_all).await?;

                // Brief wait for cancel-all to propagate
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;

                // Print final stats
                for (symbol, tracker) in &trackers {
                    tracing::info!(
                        pair = symbol.as_str(),
                        entries = tracker.stats.total_entries,
                        wins = tracker.stats.wins,
                        losses = tracker.stats.losses,
                        total_pnl = %tracker.stats.total_pnl,
                        samples = tracker.sample_count,
                        "Final stats"
                    );
                }

                break;
            }
        }
    }

    // Print summary
    let total_entries: u32 = trackers.values().map(|t| t.stats.total_entries).sum();
    let total_wins: u32 = trackers.values().map(|t| t.stats.wins).sum();
    let total_losses: u32 = trackers.values().map(|t| t.stats.losses).sum();
    let total_pnl: Decimal = trackers.values().map(|t| t.stats.total_pnl).sum();

    tracing::info!(
        total_entries,
        total_wins,
        total_losses,
        total_pnl = %total_pnl,
        "Trend Trader session finished"
    );

    Ok(())
}
