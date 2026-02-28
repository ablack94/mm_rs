use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

use kraken_core::pnl::tracker::PnlTrade;

/// Per-pair edge metrics computed over a rolling window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairEdge {
    pub symbol: String,
    /// Net realized PnL (including fees) over the window.
    pub realized_pnl: Decimal,
    /// Total volume traded (buy + sell notional) over the window.
    pub total_volume: Decimal,
    /// Total fees paid over the window.
    pub total_fees: Decimal,
    /// Number of trades in the window.
    pub trade_count: u64,
    /// Edge = realized_pnl / total_volume. Zero if no volume.
    pub edge: Decimal,
    /// Timestamp of the last trade for this pair.
    pub last_trade_at: Option<DateTime<Utc>>,
}

impl PairEdge {
    fn empty(symbol: String) -> Self {
        Self {
            symbol,
            realized_pnl: Decimal::ZERO,
            total_volume: Decimal::ZERO,
            total_fees: Decimal::ZERO,
            trade_count: 0,
            edge: Decimal::ZERO,
            last_trade_at: None,
        }
    }
}

/// Overall summary of the analyzer state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyzerSummary {
    pub total_realized_pnl: Decimal,
    pub total_fees: Decimal,
    pub total_volume: Decimal,
    pub total_trade_count: u64,
    pub overall_edge: Decimal,
    pub pair_count: usize,
    pub uptime_secs: u64,
}

/// Stores all trades in a deque and computes windowed edge metrics per pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeTracker {
    /// All trades, ordered by timestamp. We keep trades up to max_history_hours.
    trades: VecDeque<PnlTrade>,
    /// Maximum hours of trade history to retain.
    #[serde(default = "default_max_history")]
    max_history_hours: u64,
    /// Timestamp of last trade seen per pair (even if pruned from deque).
    last_trade_per_pair: HashMap<String, DateTime<Utc>>,
    /// Cumulative all-time stats per pair (not windowed).
    #[serde(default)]
    cumulative: HashMap<String, CumulativeStats>,
}

fn default_max_history() -> u64 {
    48
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CumulativeStats {
    realized_pnl: Decimal,
    total_volume: Decimal,
    total_fees: Decimal,
    trade_count: u64,
}

impl EdgeTracker {
    pub fn new() -> Self {
        Self {
            trades: VecDeque::new(),
            max_history_hours: 48,
            last_trade_per_pair: HashMap::new(),
            cumulative: HashMap::new(),
        }
    }

    /// Record a trade from the fill stream.
    pub fn record_trade(&mut self, trade: &PnlTrade) {
        self.trades.push_back(trade.clone());
        self.last_trade_per_pair
            .insert(trade.pair.to_string(), trade.timestamp);

        // Update cumulative stats
        let stats = self
            .cumulative
            .entry(trade.pair.to_string())
            .or_default();
        stats.realized_pnl += trade.pnl;
        stats.total_fees += trade.fee;
        stats.total_volume += trade.price * trade.qty;
        stats.trade_count += 1;

        self.prune();
    }

    /// Remove trades older than max_history_hours.
    fn prune(&mut self) {
        let cutoff = Utc::now() - Duration::hours(self.max_history_hours as i64);
        while let Some(front) = self.trades.front() {
            if front.timestamp < cutoff {
                self.trades.pop_front();
            } else {
                break;
            }
        }
    }

    /// Compute edge metrics for a specific pair over a rolling window.
    pub fn pair_edge(&self, symbol: &str, window_hours: u64) -> PairEdge {
        let cutoff = Utc::now() - Duration::hours(window_hours as i64);
        let mut edge = PairEdge::empty(symbol.to_string());
        edge.last_trade_at = self.last_trade_per_pair.get(symbol).copied();

        for trade in &self.trades {
            if trade.pair.to_string() != symbol || trade.timestamp < cutoff {
                continue;
            }
            let notional = trade.price * trade.qty;
            edge.realized_pnl += trade.pnl;
            edge.total_volume += notional;
            edge.total_fees += trade.fee;
            edge.trade_count += 1;
        }

        if edge.total_volume > Decimal::ZERO {
            edge.edge = edge.realized_pnl / edge.total_volume;
        }

        edge
    }

    /// Compute edge metrics for all known pairs over a rolling window.
    pub fn all_pair_edges(&self, window_hours: u64) -> Vec<PairEdge> {
        let symbols: Vec<String> = self.last_trade_per_pair.keys().cloned().collect();
        symbols
            .iter()
            .map(|s| self.pair_edge(s, window_hours))
            .collect()
    }

    /// Compute edge for multiple windows at once for a single pair.
    pub fn pair_edge_multi_window(
        &self,
        symbol: &str,
        windows: &[u64],
    ) -> Vec<(u64, PairEdge)> {
        windows
            .iter()
            .map(|&w| (w, self.pair_edge(symbol, w)))
            .collect()
    }

    /// Get the last trade timestamp for a pair, if any.
    pub fn last_trade_time(&self, symbol: &str) -> Option<DateTime<Utc>> {
        self.last_trade_per_pair.get(symbol).copied()
    }

    /// Get overall summary across all pairs over a given window.
    pub fn summary(&self, window_hours: u64, uptime_secs: u64) -> AnalyzerSummary {
        let edges = self.all_pair_edges(window_hours);
        let mut total_pnl = Decimal::ZERO;
        let mut total_fees = Decimal::ZERO;
        let mut total_volume = Decimal::ZERO;
        let mut total_trades = 0u64;

        for e in &edges {
            total_pnl += e.realized_pnl;
            total_fees += e.total_fees;
            total_volume += e.total_volume;
            total_trades += e.trade_count;
        }

        let overall_edge = if total_volume > Decimal::ZERO {
            total_pnl / total_volume
        } else {
            Decimal::ZERO
        };

        AnalyzerSummary {
            total_realized_pnl: total_pnl,
            total_fees,
            total_volume,
            total_trade_count: total_trades,
            overall_edge,
            pair_count: edges.len(),
            uptime_secs,
        }
    }

    /// Get all known pair symbols.
    pub fn known_pairs(&self) -> Vec<String> {
        self.last_trade_per_pair.keys().cloned().collect()
    }
}

impl Default for EdgeTracker {
    fn default() -> Self {
        Self::new()
    }
}
