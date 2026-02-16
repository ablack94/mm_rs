use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::{MockConfig, PairConfig};
use crate::orderbook::OrderBook;
use crate::orders::OrderStore;

/// Central shared state for the mock exchange.
pub struct ExchangeState {
    /// Per-pair order books (keyed by wsname, e.g. "OMG/USD").
    pub books: HashMap<String, OrderBook>,
    /// Open orders placed by clients.
    pub orders: OrderStore,
    /// Account balances (keyed by Kraken-style asset names like "ZUSD", "XXBT").
    pub balances: HashMap<String, f64>,
    /// Pair metadata (keyed by wsname).
    pub pair_configs: Vec<PairConfig>,
    /// Global order ID counter.
    pub next_order_id: u64,
    /// Configuration.
    pub config: MockConfig,
}

pub type SharedState = Arc<RwLock<ExchangeState>>;

impl ExchangeState {
    pub fn new(config: MockConfig) -> Self {
        let mut books = HashMap::new();
        for pc in &config.pairs {
            books.insert(
                pc.symbol.clone(),
                OrderBook::new(pc.starting_price, config.spread_pct),
            );
        }

        let mut balances = HashMap::new();
        balances.insert("ZUSD".to_string(), config.starting_usd);
        // Initialize zero balances for each base asset
        for pc in &config.pairs {
            let asset = symbol_to_kraken_asset(&pc.symbol);
            balances.entry(asset).or_insert(0.0);
        }

        ExchangeState {
            books,
            orders: OrderStore::new(),
            balances,
            pair_configs: config.pairs.clone(),
            next_order_id: 1,
            config,
        }
    }

    pub fn alloc_order_id(&mut self) -> String {
        let id = format!("MOCK-ORD-{:06}", self.next_order_id);
        self.next_order_id += 1;
        id
    }
}

/// Convert a pair's wsname (e.g. "OMG/USD") to its base asset in Kraken REST style.
/// For small-cap alts, Kraken uses the ticker directly (e.g. "OMG").
/// For BTC/ETH, Kraken uses "XXBT"/"XETH".
pub fn symbol_to_kraken_asset(wsname: &str) -> String {
    let base = wsname.split('/').next().unwrap_or(wsname);
    match base {
        "BTC" | "XBT" => "XXBT".to_string(),
        "ETH" => "XETH".to_string(),
        other => other.to_string(),
    }
}

/// Convert a wsname like "OMG/USD" to a Kraken REST key like "OMGUSD".
pub fn symbol_to_rest_key(wsname: &str) -> String {
    wsname.replace('/', "")
}

/// Determine price decimals based on price magnitude.
pub fn price_decimals_for(price: f64) -> u32 {
    if price >= 1.0 {
        4
    } else if price >= 0.01 {
        5
    } else if price >= 0.001 {
        6
    } else {
        7
    }
}

/// Determine lot (quantity) decimals based on price magnitude.
pub fn qty_decimals_for(price: f64) -> u32 {
    if price >= 1.0 {
        4
    } else if price >= 0.01 {
        5
    } else if price >= 0.001 {
        2
    } else {
        0
    }
}

/// Determine order minimum quantity based on price.
pub fn order_min_for(price: f64) -> f64 {
    if price >= 1.0 {
        1.0
    } else if price >= 0.01 {
        10.0
    } else if price >= 0.001 {
        100.0
    } else {
        1000.0
    }
}

/// Determine cost minimum based on price.
pub fn cost_min_for(_price: f64) -> f64 {
    0.50
}

/// Update balances after a fill.
pub fn update_balances(
    balances: &mut HashMap<String, f64>,
    symbol: &str,
    side: crate::orders::Side,
    qty: f64,
    price: f64,
    fee: f64,
) {
    let base_asset = symbol_to_kraken_asset(symbol);
    let cost = price * qty;

    match side {
        crate::orders::Side::Buy => {
            // Deduct USD (cost + fee), credit base asset
            *balances.entry("ZUSD".to_string()).or_insert(0.0) -= cost + fee;
            *balances.entry(base_asset).or_insert(0.0) += qty;
        }
        crate::orders::Side::Sell => {
            // Credit USD (proceeds - fee), deduct base asset
            *balances.entry("ZUSD".to_string()).or_insert(0.0) += cost - fee;
            *balances.entry(base_asset).or_insert(0.0) -= qty;
        }
    }
}
