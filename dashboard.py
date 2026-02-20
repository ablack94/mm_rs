#!/usr/bin/env python3
"""
Kraken MM Bot Dashboard
~~~~~~~~~~~~~~~~~~~~~~~
Single-file web dashboard that shows portfolio, positions, PnL, and pair states.

Data sources:
  - State Store (localhost:3040) for pair configs/states, bot connection status
  - PnL Analyzer (localhost:3031) for edge metrics, positions, realized PnL
  - Kraken proxy (10.255.255.254:8080) for live ticker prices

Usage:
  python3 dashboard.py          # serves on port 8888
  python3 dashboard.py --port 9999
"""

import json
import os
import sys
import time
import urllib.request
import urllib.error
from http.server import HTTPServer, BaseHTTPRequestHandler
from decimal import Decimal, InvalidOperation

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------
STATE_STORE_URL = os.environ.get("STATE_STORE_URL", "http://localhost:3040")
STATE_STORE_TOKEN = os.environ.get("STATE_STORE_TOKEN", "")
PNL_API_URL = os.environ.get("PNL_API_URL", "http://localhost:3031")
PNL_API_TOKEN = os.environ.get("PNL_API_TOKEN", "")
PROXY_URL = os.environ.get("PROXY_URL", "http://10.255.255.254:8080")
PROXY_TOKEN = os.environ.get(
    "PROXY_TOKEN",
    "0ba154d4e4886b262f7752ebcb5213a57ea8161b88c4b5f4eed5b0f79363d7ab",
)
SESSION_FILE = os.environ.get("SESSION_FILE", "session.json")
DEFAULT_PORT = 8888

# ---------------------------------------------------------------------------
# Data helpers
# ---------------------------------------------------------------------------

def _state_store_get(path: str) -> dict:
    """GET from the state store REST API."""
    headers = {}
    if STATE_STORE_TOKEN:
        headers["Authorization"] = f"Bearer {STATE_STORE_TOKEN}"
    req = urllib.request.Request(f"{STATE_STORE_URL}{path}", headers=headers)
    try:
        resp = urllib.request.urlopen(req, timeout=5)
        return json.loads(resp.read())
    except Exception as e:
        return {"_error": str(e)}


def _proxy_get(path: str) -> dict:
    """GET from the Kraken signing proxy."""
    req = urllib.request.Request(
        f"{PROXY_URL}{path}",
        headers={"Authorization": f"Bearer {PROXY_TOKEN}"},
    )
    try:
        resp = urllib.request.urlopen(req, timeout=10)
        return json.loads(resp.read())
    except Exception as e:
        return {"_error": str(e)}


def _pnl_get(path: str) -> dict:
    """GET from the PnL analyzer API."""
    headers = {}
    if PNL_API_TOKEN:
        headers["Authorization"] = f"Bearer {PNL_API_TOKEN}"
    req = urllib.request.Request(f"{PNL_API_URL}{path}", headers=headers)
    try:
        resp = urllib.request.urlopen(req, timeout=5)
        return json.loads(resp.read())
    except Exception as e:
        return {"_error": str(e)}


def _symbol_to_kraken_pair(symbol: str) -> str:
    """Convert bot symbol like 'OMG/USD' to Kraken ticker pair like 'OMGUSD'."""
    return symbol.replace("/", "")


def _fetch_ticker(pair: str) -> dict:
    """Fetch ticker data from proxy. Returns the ticker dict for the pair."""
    data = _proxy_get(f"/0/public/Ticker?pair={pair}")
    if "_error" in data or data.get("error"):
        return {}
    result = data.get("result", {})
    if not result:
        return {}
    # Kraken returns {REST_KEY: {ticker data}} -- take first
    return next(iter(result.values()), {})


def _fetch_all_prices(symbols: list) -> dict:
    """Fetch current prices for all symbols. Returns {symbol: price_float}."""
    prices = {}
    # Batch into a single request if possible (Kraken supports comma-separated)
    if not symbols:
        return prices
    pairs = [_symbol_to_kraken_pair(s) for s in symbols]
    pair_str = ",".join(pairs)
    data = _proxy_get(f"/0/public/Ticker?pair={pair_str}")
    if "_error" in data or data.get("error"):
        # Fall back to individual requests
        for sym in symbols:
            ticker = _fetch_ticker(_symbol_to_kraken_pair(sym))
            if ticker:
                last = ticker.get("c", [None])[0]
                if last:
                    prices[sym] = float(last)
        return prices

    result = data.get("result", {})
    # Map kraken keys back to our symbols
    # Build a lookup: OMGUSD -> OMG/USD
    pair_to_sym = {}
    for sym in symbols:
        pair_to_sym[_symbol_to_kraken_pair(sym).upper()] = sym

    for kraken_key, ticker in result.items():
        sym = pair_to_sym.get(kraken_key.upper())
        if sym and ticker:
            last = ticker.get("c", [None])[0]
            if last:
                prices[sym] = float(last)
    return prices


def _safe_float(val, default=0.0) -> float:
    """Convert a string/number to float safely."""
    if val is None:
        return default
    try:
        return float(Decimal(str(val)))
    except (InvalidOperation, ValueError):
        return default


def build_dashboard_data() -> dict:
    """Assemble all data for the dashboard JSON endpoint."""

    # 1. State store health — bot connection status
    health = _state_store_get("/health")
    bot_connected = False
    ss_up = "_error" not in health
    if ss_up:
        bot_connected = health.get("bot_connected", False)

    # 2. State store pairs — pair states and configs
    ss_pairs_resp = _state_store_get("/pairs")
    ss_pairs = []
    if "_error" not in ss_pairs_resp:
        ss_pairs = ss_pairs_resp.get("pairs", [])

    # 3. State store defaults — global trading parameters
    defaults_resp = _state_store_get("/defaults")
    defaults = {}
    if "_error" not in defaults_resp:
        defaults = defaults_resp

    # 4. PnL analyzer summary — realized/unrealized PnL, fees, overall edge
    pnl_data = _pnl_get("/pnl")
    pnl_up = "_error" not in pnl_data
    realized_pnl = 0.0
    unrealized_pnl = 0.0
    total_fees = 0.0
    trade_count = 0
    overall_edge = None
    if pnl_up:
        pnl_section = pnl_data.get("pnl", {})
        realized_pnl = _safe_float(pnl_section.get("realized_pnl"))
        unrealized_pnl = _safe_float(pnl_section.get("unrealized_pnl"))
        total_fees = _safe_float(pnl_section.get("total_fees"))
        trade_count = pnl_section.get("trade_count", 0)
        edge_section = pnl_data.get("edge", {})
        overall_edge = edge_section.get("overall_edge")

    # 4b. Last fill time for staleness detection
    last_fill_time = None
    if pnl_up:
        last_fill_time = pnl_data.get("last_fill_time")

    # 5. PnL analyzer per-pair — edge metrics and positions
    pnl_pairs_resp = _pnl_get("/pairs")
    pnl_pairs_map = {}
    if "_error" not in pnl_pairs_resp:
        for pp in pnl_pairs_resp.get("pairs", []):
            pnl_pairs_map[pp["symbol"]] = pp

    # 5b. Fetch individual pair details for position data
    #     The /pairs list doesn't include positions; /pairs/{symbol} does
    active_symbols = set()
    for p in ss_pairs:
        if p.get("state") in ("active", "wind_down", "liquidating"):
            active_symbols.add(p["symbol"])
    for sym in pnl_pairs_map:
        active_symbols.add(sym)
    for sym in active_symbols:
        encoded = sym.replace("/", "%2F")
        detail = _pnl_get(f"/pairs/{encoded}")
        if "_error" not in detail and "position" in detail:
            if sym in pnl_pairs_map:
                pnl_pairs_map[sym]["position"] = detail["position"]
            else:
                pnl_pairs_map[sym] = detail

    # 5c. Session PnL from monitor
    session_data = None
    try:
        with open(SESSION_FILE) as f:
            session_data = json.load(f)
    except Exception:
        pass

    # 6. Collect all known symbols and fetch live prices
    all_symbols = set()
    for p in ss_pairs:
        all_symbols.add(p["symbol"])
    for sym in pnl_pairs_map:
        all_symbols.add(sym)
    prices = _fetch_all_prices(list(all_symbols))

    # 7. Merge state store pairs with PnL analyzer data
    merged_pairs = []
    seen_symbols = set()

    for sp in ss_pairs:
        symbol = sp["symbol"]
        seen_symbols.add(symbol)
        state = sp.get("state", "disabled")
        config = sp.get("config", {})
        disabled_reason = sp.get("disabled_reason")

        # PnL analyzer data for this pair
        pp = pnl_pairs_map.get(symbol, {})
        windows = pp.get("windows", {})

        # Position data — fetch from per-pair detail if available
        position = pp.get("position")
        pos_qty = 0.0
        pos_avg_cost = 0.0
        current_price = prices.get(symbol, 0.0)
        current_value = 0.0
        pair_unrealized = 0.0

        if position:
            pos_qty = _safe_float(position.get("qty"))
            pos_avg_cost = _safe_float(position.get("avg_cost"))
            # Prefer PnL analyzer's current_price if available, fall back to proxy
            pnl_price = _safe_float(position.get("current_price"))
            if pnl_price > 0:
                current_price = pnl_price
            current_value = _safe_float(position.get("current_value", pos_qty * current_price))
            pair_unrealized = _safe_float(position.get("unrealized_pnl", current_value - pos_qty * pos_avg_cost))

        # Edge metrics per window
        w1h = windows.get("1h", {})
        w6h = windows.get("6h", {})
        w24h = windows.get("24h", {})

        merged_pairs.append({
            "symbol": symbol,
            "state": state,
            "config": config,
            "disabled_reason": disabled_reason,
            "position_qty": pos_qty,
            "position_avg_cost": pos_avg_cost,
            "current_price": current_price,
            "current_value": current_value,
            "unrealized_pnl": pair_unrealized,
            "edge_1h": w1h.get("edge"),
            "edge_6h": w6h.get("edge"),
            "edge_24h": w24h.get("edge"),
            "trade_count_24h": w24h.get("trade_count", 0),
        })

    # Include pairs from PnL analyzer that aren't in state store
    for sym, pp in pnl_pairs_map.items():
        if sym in seen_symbols:
            continue
        windows = pp.get("windows", {})
        position = pp.get("position")
        pos_qty = 0.0
        pos_avg_cost = 0.0
        current_price = prices.get(sym, 0.0)
        current_value = 0.0
        pair_unrealized = 0.0

        if position:
            pos_qty = _safe_float(position.get("qty"))
            pos_avg_cost = _safe_float(position.get("avg_cost"))
            pnl_price = _safe_float(position.get("current_price"))
            if pnl_price > 0:
                current_price = pnl_price
            current_value = _safe_float(position.get("current_value", pos_qty * current_price))
            pair_unrealized = _safe_float(position.get("unrealized_pnl", current_value - pos_qty * pos_avg_cost))

        w1h = windows.get("1h", {})
        w6h = windows.get("6h", {})
        w24h = windows.get("24h", {})

        merged_pairs.append({
            "symbol": sym,
            "state": "unknown",
            "config": {},
            "disabled_reason": None,
            "position_qty": pos_qty,
            "position_avg_cost": pos_avg_cost,
            "current_price": current_price,
            "current_value": current_value,
            "unrealized_pnl": pair_unrealized,
            "edge_1h": w1h.get("edge"),
            "edge_6h": w6h.get("edge"),
            "edge_24h": w24h.get("edge"),
            "trade_count_24h": w24h.get("trade_count", 0),
        })

    merged_pairs.sort(key=lambda p: p["symbol"])

    # 8. Recent trades from PnL analyzer
    trades_resp = _pnl_get("/trades?limit=100")
    recent_trades = []
    if isinstance(trades_resp, list):
        recent_trades = trades_resp
    elif isinstance(trades_resp, dict) and "_error" not in trades_resp:
        # In case the endpoint wraps in an object
        recent_trades = trades_resp.get("trades", trades_resp)

    # Positions: filtered to pairs with qty > 0
    positions = []
    total_cost_basis = 0.0
    total_current_value = 0.0
    total_unrealized_pnl = 0.0

    for p in merged_pairs:
        if p["position_qty"] <= 0:
            continue
        cost_basis = p["position_qty"] * p["position_avg_cost"]
        pct_change = ((p["current_price"] / p["position_avg_cost"]) - 1.0) * 100 if p["position_avg_cost"] > 0 else 0.0

        # Skip dust
        if cost_basis < 0.01 and p["current_value"] < 0.01:
            continue

        total_cost_basis += cost_basis
        total_current_value += p["current_value"]
        total_unrealized_pnl += p["unrealized_pnl"]

        positions.append({
            "symbol": p["symbol"],
            "qty": p["position_qty"],
            "avg_cost": p["position_avg_cost"],
            "current_price": p["current_price"],
            "cost_basis": cost_basis,
            "current_value": p["current_value"],
            "unrealized_pnl": p["unrealized_pnl"],
            "pct_change": pct_change,
        })

    net_pnl = realized_pnl + total_unrealized_pnl
    portfolio_value = total_current_value

    return {
        "timestamp": time.strftime("%Y-%m-%d %H:%M:%S UTC", time.gmtime()),
        "bot_connected": bot_connected,
        "state_store_up": ss_up,
        "pnl_analyzer_up": pnl_up,
        "pnl_source": "pnl-analyzer" if pnl_up else "unavailable",
        "portfolio_value": portfolio_value,
        "total_cost_basis": total_cost_basis,
        "total_current_value": total_current_value,
        "realized_pnl": realized_pnl,
        "unrealized_pnl": total_unrealized_pnl,
        "net_pnl": net_pnl,
        "total_fees": total_fees,
        "trade_count": trade_count,
        "overall_edge": overall_edge,
        "defaults": defaults,
        "pairs": merged_pairs,
        "positions": positions,
        "recent_trades": recent_trades,
        "session": session_data,
        "last_fill_time": last_fill_time,
    }


# ---------------------------------------------------------------------------
# HTML Dashboard (inline)
# ---------------------------------------------------------------------------

DASHBOARD_HTML = r"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Kraken MM Dashboard</title>
<style>
  :root {
    --bg: #0d1117;
    --bg-card: #161b22;
    --bg-card-alt: #1c2129;
    --border: #30363d;
    --text: #e6edf3;
    --text-dim: #8b949e;
    --green: #3fb950;
    --red: #f85149;
    --blue: #58a6ff;
    --yellow: #d29922;
    --orange: #db6d28;
    --purple: #bc8cff;
  }
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body {
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Helvetica, Arial, sans-serif;
    background: var(--bg);
    color: var(--text);
    padding: 16px;
    line-height: 1.5;
  }
  h1 {
    font-size: 1.4em;
    margin-bottom: 4px;
    display: flex;
    align-items: center;
    gap: 12px;
  }
  .header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 16px;
    flex-wrap: wrap;
    gap: 8px;
  }
  .header-right {
    display: flex;
    align-items: center;
    gap: 12px;
    font-size: 0.85em;
    color: var(--text-dim);
  }
  .status-dot {
    width: 8px; height: 8px; border-radius: 50%;
    display: inline-block;
  }
  .status-dot.live { background: var(--green); }
  .status-dot.disconnected { background: var(--yellow); }
  .status-dot.error { background: var(--red); }

  /* Summary cards */
  .summary-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
    gap: 12px;
    margin-bottom: 20px;
  }
  .card {
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 14px 16px;
  }
  .card-label {
    font-size: 0.75em;
    color: var(--text-dim);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    margin-bottom: 4px;
  }
  .card-value {
    font-size: 1.5em;
    font-weight: 600;
    font-variant-numeric: tabular-nums;
  }
  .card-sub {
    font-size: 0.8em;
    color: var(--text-dim);
    margin-top: 2px;
  }

  .positive { color: var(--green); }
  .negative { color: var(--red); }
  .neutral  { color: var(--text-dim); }

  /* Defaults section */
  .defaults-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
    gap: 8px;
    margin-bottom: 20px;
  }
  .default-item {
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 8px 12px;
  }
  .default-item .label {
    font-size: 0.72em;
    color: var(--text-dim);
    text-transform: uppercase;
    letter-spacing: 0.3px;
  }
  .default-item .value {
    font-size: 1.05em;
    font-weight: 600;
    font-variant-numeric: tabular-nums;
  }

  /* Tables */
  .section-title {
    font-size: 1.05em;
    font-weight: 600;
    margin-bottom: 8px;
    padding-bottom: 4px;
    border-bottom: 1px solid var(--border);
  }
  .section { margin-bottom: 20px; }
  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 0.88em;
    font-variant-numeric: tabular-nums;
  }
  thead th {
    text-align: left;
    padding: 8px 10px;
    color: var(--text-dim);
    font-weight: 500;
    font-size: 0.82em;
    text-transform: uppercase;
    letter-spacing: 0.3px;
    border-bottom: 1px solid var(--border);
    white-space: nowrap;
  }
  tbody td {
    padding: 7px 10px;
    border-bottom: 1px solid #21262d;
  }
  tbody tr:hover { background: var(--bg-card-alt); }
  .num { text-align: right; }

  .empty-msg {
    color: var(--text-dim);
    font-style: italic;
    padding: 16px;
    text-align: center;
  }

  /* Refresh indicator */
  .refresh-bar {
    height: 2px;
    background: var(--border);
    border-radius: 1px;
    margin-bottom: 16px;
    overflow: hidden;
  }
  .refresh-bar-inner {
    height: 100%;
    background: var(--blue);
    width: 0%;
    transition: width 1s linear;
  }

  /* Pair status badges */
  .badge {
    display: inline-block;
    padding: 2px 8px;
    border-radius: 4px;
    font-size: 0.82em;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.3px;
  }
  .badge-active      { background: rgba(63,185,80,0.15); color: var(--green); }
  .badge-disabled     { background: rgba(248,81,73,0.15); color: var(--red); }
  .badge-wind_down    { background: rgba(210,153,34,0.15); color: var(--yellow); }
  .badge-liquidating  { background: rgba(219,109,40,0.15); color: var(--orange); }
  .badge-unknown      { background: rgba(139,148,158,0.15); color: var(--text-dim); }

  /* Edge coloring */
  .edge-positive { color: var(--green); }
  .edge-negative { color: var(--red); }
  .edge-neutral  { color: var(--text-dim); }

  /* Config tooltip */
  .config-hover {
    cursor: help;
    text-decoration: underline dotted var(--text-dim);
    text-underline-offset: 2px;
  }
  .config-hover:hover { color: var(--blue); }

  /* Side badges for trades */
  .badge-buy  { background: rgba(63,185,80,0.15); color: var(--green); }
  .badge-sell { background: rgba(248,81,73,0.15); color: var(--red); }

  /* Staleness warning banner */
  .stale-banner {
    display: none;
    background: rgba(248,81,73,0.15);
    border: 1px solid var(--red);
    border-radius: 8px;
    padding: 10px 16px;
    margin-bottom: 16px;
    color: var(--red);
    font-weight: 600;
    font-size: 0.9em;
  }
  .stale-banner.visible { display: block; }

  @media (max-width: 600px) {
    .summary-grid { grid-template-columns: repeat(2, 1fr); }
    .defaults-grid { grid-template-columns: repeat(2, 1fr); }
    table { font-size: 0.8em; }
  }
</style>
</head>
<body>

<div class="header">
  <h1>
    Kraken MM Bot
    <span id="statusDot" class="status-dot live" title="Live"></span>
  </h1>
  <div class="header-right">
    <span id="serviceStatus" style="font-size:0.9em"></span>
    <span id="lastUpdate">--</span>
    <span id="countdown"></span>
  </div>
</div>

<div class="refresh-bar"><div id="refreshBar" class="refresh-bar-inner"></div></div>

<div id="staleBanner" class="stale-banner">
  PnL data may be stale — no fills received in over 5 minutes. Dashboard numbers may not reflect recent trades.
</div>

<!-- Summary Cards -->
<div class="summary-grid">
  <div class="card">
    <div class="card-label">Portfolio Value</div>
    <div class="card-value" id="portfolioValue">--</div>
    <div class="card-sub" id="costBasis">Cost basis: --</div>
  </div>
  <div class="card">
    <div class="card-label">Unrealized P&L</div>
    <div class="card-value" id="unrealizedPnl">--</div>
    <div class="card-sub" id="unrealizedPct">--</div>
  </div>
  <div class="card">
    <div class="card-label">Realized P&L</div>
    <div class="card-value" id="realizedPnl">--</div>
    <div class="card-sub" id="tradeCount">-- trades</div>
  </div>
  <div class="card">
    <div class="card-label">Net P&L</div>
    <div class="card-value" id="netPnl">--</div>
    <div class="card-sub" id="totalFees">Fees: --</div>
  </div>
  <div class="card">
    <div class="card-label">Overall Edge</div>
    <div class="card-value" id="overallEdge">--</div>
    <div class="card-sub" id="pnlSource">--</div>
  </div>
  <div class="card">
    <div class="card-label">Session P&amp;L</div>
    <div class="card-value" id="sessionPnl">--</div>
    <div class="card-sub" id="sessionInfo">--</div>
  </div>
</div>

<!-- Global Defaults -->
<div class="section">
  <div class="section-title">Global Defaults</div>
  <div class="defaults-grid" id="defaultsGrid">
    <div class="default-item"><div class="label">Loading...</div></div>
  </div>
</div>

<!-- Pair Status -->
<div class="section">
  <div class="section-title">Pairs</div>
  <table>
    <thead>
      <tr>
        <th>Pair</th>
        <th>State</th>
        <th class="num">Edge (1h)</th>
        <th class="num">Edge (6h)</th>
        <th class="num">Edge (24h)</th>
        <th class="num">Position</th>
        <th>Config</th>
      </tr>
    </thead>
    <tbody id="pairStatusBody">
      <tr><td colspan="7" class="empty-msg">Loading...</td></tr>
    </tbody>
  </table>
</div>

<!-- Positions Table -->
<div class="section">
  <div class="section-title">Positions</div>
  <table>
    <thead>
      <tr>
        <th>Symbol</th>
        <th class="num">Qty</th>
        <th class="num">Avg Cost</th>
        <th class="num">Price</th>
        <th class="num">Value</th>
        <th class="num">Unrealized P&L</th>
        <th class="num">Change</th>
      </tr>
    </thead>
    <tbody id="positionsBody">
      <tr><td colspan="7" class="empty-msg">Loading...</td></tr>
    </tbody>
  </table>
</div>

<!-- Recent Trades -->
<div class="section">
  <div class="section-title">Recent Trades</div>
  <table>
    <thead>
      <tr>
        <th>Time</th>
        <th>Pair</th>
        <th>Side</th>
        <th class="num">Price</th>
        <th class="num">Qty</th>
        <th class="num">Value</th>
        <th class="num">Fee</th>
        <th class="num">P&L</th>
      </tr>
    </thead>
    <tbody id="tradesBody">
      <tr><td colspan="8" class="empty-msg">Loading...</td></tr>
    </tbody>
  </table>
</div>

<script>
const REFRESH_INTERVAL = 15; // seconds
let countdown = REFRESH_INTERVAL;
let timerInterval = null;

function fmt(n, decimals) {
  if (n === null || n === undefined || isNaN(n)) return '--';
  decimals = decimals !== undefined ? decimals : 2;
  return n.toLocaleString('en-US', {minimumFractionDigits: decimals, maximumFractionDigits: decimals});
}

function fmtUsd(n) {
  if (n === null || n === undefined || isNaN(n)) return '--';
  const sign = n < 0 ? '-' : '';
  return sign + '$' + fmt(Math.abs(n), 2);
}

function fmtPct(n) {
  if (n === null || n === undefined || isNaN(n)) return '--';
  const sign = n > 0 ? '+' : '';
  return sign + fmt(n, 2) + '%';
}

function fmtPrice(n) {
  if (n === null || n === undefined || isNaN(n) || n === 0) return '--';
  if (n >= 100) return '$' + fmt(n, 2);
  if (n >= 1)   return '$' + fmt(n, 4);
  if (n >= 0.01) return '$' + fmt(n, 5);
  return '$' + fmt(n, 8);
}

function fmtQty(n) {
  if (n === null || n === undefined || isNaN(n)) return '--';
  if (n >= 1000) return fmt(n, 2);
  if (n >= 1) return fmt(n, 4);
  return fmt(n, 6);
}

function pnlClass(n) {
  if (n > 0.005) return 'positive';
  if (n < -0.005) return 'negative';
  return 'neutral';
}

function fmtTradeTime(ts) {
  if (!ts) return '--';
  try {
    const d = new Date(ts);
    if (isNaN(d.getTime())) return '--';
    const now = new Date();
    const diffMs = now - d;
    const diffSec = Math.floor(diffMs / 1000);
    if (diffSec < 60) return diffSec + 's ago';
    if (diffSec < 3600) return Math.floor(diffSec / 60) + 'm ago';
    if (diffSec < 86400) return d.toLocaleTimeString('en-US', {hour12: false, hour: '2-digit', minute: '2-digit', second: '2-digit'});
    return d.toLocaleDateString('en-US', {month: 'short', day: 'numeric'}) + ' ' +
           d.toLocaleTimeString('en-US', {hour12: false, hour: '2-digit', minute: '2-digit'});
  } catch (e) { return '--'; }
}

function fmtEdge(val) {
  if (val === null || val === undefined) return '--';
  const n = parseFloat(val);
  if (isNaN(n)) return '--';
  // Edge is a ratio like 0.0025 → display as bps (0.25%) or raw
  const pct = (n * 100).toFixed(3);
  return (n >= 0 ? '+' : '') + pct + '%';
}

function edgeClass(val) {
  if (val === null || val === undefined) return 'edge-neutral';
  const n = parseFloat(val);
  if (isNaN(n)) return 'edge-neutral';
  if (n > 0.0001) return 'edge-positive';
  if (n < -0.0001) return 'edge-negative';
  return 'edge-neutral';
}

function fmtDefaultKey(key) {
  // order_size_usd → Order Size USD
  return key.replace(/_/g, ' ').replace(/\b\w/g, c => c.toUpperCase());
}

function fmtDefaultVal(key, val) {
  if (val === null || val === undefined) return '--';
  if (key.endsWith('_usd')) return '$' + fmt(val, 2);
  if (key.endsWith('_bps')) return fmt(val, 0) + ' bps';
  if (key.endsWith('_pct')) return (val * 100).toFixed(1) + '%';
  return String(val);
}

function buildConfigSummary(config) {
  if (!config) return '--';
  const parts = [];
  for (const [k, v] of Object.entries(config)) {
    if (v === null || v === undefined) continue;
    parts.push(fmtDefaultKey(k) + ': ' + fmtDefaultVal(k, v));
  }
  return parts.length > 0 ? parts.join(', ') : 'defaults';
}

function updateDashboard(data) {
  // Status dot — based on bot connection
  const dot = document.getElementById('statusDot');
  const svcEl = document.getElementById('serviceStatus');
  if (data._error) {
    dot.className = 'status-dot error';
    dot.title = 'Dashboard error';
    svcEl.textContent = 'Error';
  } else if (!data.state_store_up) {
    dot.className = 'status-dot error';
    dot.title = 'State store down';
    svcEl.textContent = 'State store down';
  } else if (!data.bot_connected) {
    dot.className = 'status-dot disconnected';
    dot.title = 'Bot disconnected';
    svcEl.textContent = 'Bot disconnected';
  } else {
    dot.className = 'status-dot live';
    dot.title = 'Bot connected';
    svcEl.textContent = '';
  }

  document.getElementById('lastUpdate').textContent = data.timestamp || '--';

  // Staleness warning: show banner if last_fill_time is >5 min old while bot is connected
  const staleBanner = document.getElementById('staleBanner');
  if (data.bot_connected && data.last_fill_time) {
    const fillAge = (new Date() - new Date(data.last_fill_time)) / 1000;
    if (fillAge > 300) {
      const mins = Math.floor(fillAge / 60);
      staleBanner.textContent = 'PnL data may be stale \u2014 last fill was ' + mins + ' minutes ago. Dashboard numbers may not reflect recent trades.';
      staleBanner.classList.add('visible');
    } else {
      staleBanner.classList.remove('visible');
    }
  } else {
    staleBanner.classList.remove('visible');
  }

  // Summary cards
  document.getElementById('portfolioValue').textContent = fmtUsd(data.portfolio_value);
  document.getElementById('costBasis').textContent = 'Cost basis: ' + fmtUsd(data.total_cost_basis);

  const urpEl = document.getElementById('unrealizedPnl');
  urpEl.textContent = fmtUsd(data.unrealized_pnl);
  urpEl.className = 'card-value ' + pnlClass(data.unrealized_pnl);
  const urpPct = data.total_cost_basis > 0
    ? (data.unrealized_pnl / data.total_cost_basis) * 100 : 0;
  document.getElementById('unrealizedPct').textContent = fmtPct(urpPct) + ' on cost';

  const rpEl = document.getElementById('realizedPnl');
  rpEl.textContent = fmtUsd(data.realized_pnl);
  rpEl.className = 'card-value ' + pnlClass(data.realized_pnl);
  document.getElementById('tradeCount').textContent = (data.trade_count || 0) + ' trades';

  const npEl = document.getElementById('netPnl');
  npEl.textContent = fmtUsd(data.net_pnl);
  npEl.className = 'card-value ' + pnlClass(data.net_pnl);
  document.getElementById('totalFees').textContent = 'Fees: ' + fmtUsd(data.total_fees);

  // Overall edge card
  const edgeEl = document.getElementById('overallEdge');
  edgeEl.textContent = fmtEdge(data.overall_edge);
  edgeEl.className = 'card-value ' + edgeClass(data.overall_edge);
  document.getElementById('pnlSource').textContent =
    'Source: ' + (data.pnl_source || '--');

  // Session PnL card
  const spEl = document.getElementById('sessionPnl');
  const siEl = document.getElementById('sessionInfo');
  if (data.session) {
    const sp = data.session.session_pnl || 0;
    spEl.textContent = fmtUsd(sp);
    spEl.className = 'card-value ' + pnlClass(sp);
    const limit = data.session.session_loss_limit || 0;
    const started = data.session.started_at || '';
    let startStr = '';
    if (started) {
      try { startStr = new Date(started).toLocaleTimeString('en-US', {hour12: true, hour: 'numeric', minute: '2-digit'}); } catch(e) {}
    }
    siEl.textContent = 'Limit: -$' + limit + (startStr ? ' | Since ' + startStr : '');
  } else {
    spEl.textContent = '--';
    spEl.className = 'card-value neutral';
    siEl.textContent = 'No active session';
  }

  // Global defaults
  const dg = document.getElementById('defaultsGrid');
  const defaults = data.defaults || {};
  const defaultKeys = Object.keys(defaults).sort();
  if (defaultKeys.length === 0) {
    dg.innerHTML = '<div class="default-item"><div class="label">No defaults available</div></div>';
  } else {
    let html = '';
    for (const k of defaultKeys) {
      html += '<div class="default-item">';
      html += '<div class="label">' + fmtDefaultKey(k) + '</div>';
      html += '<div class="value">' + fmtDefaultVal(k, defaults[k]) + '</div>';
      html += '</div>';
    }
    dg.innerHTML = html;
  }

  // Pairs table
  const psBody = document.getElementById('pairStatusBody');
  const pairs = data.pairs || [];
  if (pairs.length === 0) {
    psBody.innerHTML = '<tr><td colspan="7" class="empty-msg">No pair data</td></tr>';
  } else {
    let html = '';
    for (const p of pairs) {
      const st = p.state || 'unknown';
      const badgeClass = 'badge-' + st;
      const label = st.toUpperCase().replace('_', ' ');

      const posVal = p.position_qty > 0 ? fmtUsd(p.current_value) : '--';
      const configTitle = buildConfigSummary(p.config);
      const configLabel = (configTitle === 'defaults') ? 'defaults' : 'custom';
      let reasonHtml = '';
      if (p.disabled_reason) {
        reasonHtml = ' <span style="color:var(--text-dim);font-size:0.85em" title="' +
          p.disabled_reason.replace(/"/g, '&quot;') + '">(' + p.disabled_reason + ')</span>';
      }

      html += '<tr>';
      html += '<td><strong>' + p.symbol + '</strong></td>';
      html += '<td><span class="badge ' + badgeClass + '">' + label + '</span>' + reasonHtml + '</td>';
      html += '<td class="num ' + edgeClass(p.edge_1h) + '">' + fmtEdge(p.edge_1h) + '</td>';
      html += '<td class="num ' + edgeClass(p.edge_6h) + '">' + fmtEdge(p.edge_6h) + '</td>';
      html += '<td class="num ' + edgeClass(p.edge_24h) + '">' + fmtEdge(p.edge_24h) + '</td>';
      html += '<td class="num">' + posVal + '</td>';
      html += '<td><span class="config-hover" title="' + configTitle.replace(/"/g, '&quot;') + '">' + configLabel + '</span></td>';
      html += '</tr>';
    }
    psBody.innerHTML = html;
  }

  // Positions table
  const posBody = document.getElementById('positionsBody');
  if (!data.positions || data.positions.length === 0) {
    posBody.innerHTML = '<tr><td colspan="7" class="empty-msg">No open positions</td></tr>';
  } else {
    let html = '';
    for (const p of data.positions) {
      const cls = pnlClass(p.unrealized_pnl);
      html += '<tr>';
      html += '<td><strong>' + p.symbol + '</strong></td>';
      html += '<td class="num">' + fmtQty(p.qty) + '</td>';
      html += '<td class="num">' + fmtPrice(p.avg_cost) + '</td>';
      html += '<td class="num">' + fmtPrice(p.current_price) + '</td>';
      html += '<td class="num">' + fmtUsd(p.current_value) + '</td>';
      html += '<td class="num ' + cls + '">' + fmtUsd(p.unrealized_pnl) + '</td>';
      html += '<td class="num ' + cls + '">' + fmtPct(p.pct_change) + '</td>';
      html += '</tr>';
    }
    posBody.innerHTML = html;
  }

  // Recent Trades table
  const tradesBody = document.getElementById('tradesBody');
  const trades = data.recent_trades || [];
  if (trades.length === 0) {
    tradesBody.innerHTML = '<tr><td colspan="8" class="empty-msg">No recent trades</td></tr>';
  } else {
    let html = '';
    for (const t of trades) {
      const sideClass = t.side === 'buy' ? 'badge-buy' : 'badge-sell';
      const sideLabel = (t.side || '').toUpperCase();
      const pCls = pnlClass(parseFloat(t.pnl) || 0);
      html += '<tr>';
      html += '<td>' + fmtTradeTime(t.timestamp) + '</td>';
      html += '<td><strong>' + (t.symbol || '--') + '</strong></td>';
      html += '<td><span class="badge ' + sideClass + '">' + sideLabel + '</span></td>';
      html += '<td class="num">' + fmtPrice(parseFloat(t.price)) + '</td>';
      html += '<td class="num">' + fmtQty(parseFloat(t.qty)) + '</td>';
      html += '<td class="num">' + fmtUsd(parseFloat(t.value)) + '</td>';
      html += '<td class="num">' + fmtUsd(parseFloat(t.fee)) + '</td>';
      html += '<td class="num ' + pCls + '">' + fmtUsd(parseFloat(t.pnl)) + '</td>';
      html += '</tr>';
    }
    tradesBody.innerHTML = html;
  }
}

async function refresh() {
  try {
    const resp = await fetch('/api/data');
    if (!resp.ok) throw new Error('HTTP ' + resp.status);
    const data = await resp.json();
    updateDashboard(data);
  } catch (e) {
    console.error('Refresh error:', e);
    updateDashboard({_error: e.message});
  }
}

function startCountdown() {
  countdown = REFRESH_INTERVAL;
  const bar = document.getElementById('refreshBar');
  bar.style.transition = 'none';
  bar.style.width = '100%';
  // Force reflow
  void bar.offsetWidth;
  bar.style.transition = 'width ' + REFRESH_INTERVAL + 's linear';
  bar.style.width = '0%';

  if (timerInterval) clearInterval(timerInterval);
  timerInterval = setInterval(() => {
    countdown--;
    document.getElementById('countdown').textContent = countdown + 's';
    if (countdown <= 0) {
      clearInterval(timerInterval);
      refresh().then(startCountdown);
    }
  }, 1000);
}

// Initial load
refresh().then(startCountdown);
</script>
</body>
</html>
"""


# ---------------------------------------------------------------------------
# HTTP Server
# ---------------------------------------------------------------------------

class DashboardHandler(BaseHTTPRequestHandler):
    """Serves the dashboard HTML and JSON API."""

    def log_message(self, format, *args):
        """Quieter logging."""
        sys.stderr.write(
            f"[{time.strftime('%H:%M:%S')}] {args[0] if args else ''}\n"
        )

    def do_GET(self):
        if self.path == "/" or self.path == "/index.html":
            self._serve_html()
        elif self.path == "/api/data":
            self._serve_json()
        elif self.path == "/health":
            self._respond(200, "application/json", json.dumps({"status": "ok"}))
        else:
            self._respond(404, "text/plain", "Not Found")

    def _serve_html(self):
        self._respond(200, "text/html; charset=utf-8", DASHBOARD_HTML)

    def _serve_json(self):
        try:
            data = build_dashboard_data()
            self._respond(200, "application/json", json.dumps(data))
        except Exception as e:
            self._respond(
                500,
                "application/json",
                json.dumps({"_error": str(e)}),
            )

    def _respond(self, status: int, content_type: str, body: str):
        encoded = body.encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(encoded)))
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Access-Control-Allow-Origin", "*")
        self.end_headers()
        self.wfile.write(encoded)


def main():
    port = DEFAULT_PORT
    if len(sys.argv) > 1:
        if sys.argv[1] == "--port" and len(sys.argv) > 2:
            port = int(sys.argv[2])
        else:
            try:
                port = int(sys.argv[1])
            except ValueError:
                pass

    server = HTTPServer(("0.0.0.0", port), DashboardHandler)
    print(f"Dashboard server starting on http://0.0.0.0:{port}")
    print(f"  State Store:  {STATE_STORE_URL}")
    print(f"  PnL Analyzer: {PNL_API_URL}")
    print(f"  Proxy:        {PROXY_URL}")
    print()
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nShutting down.")
        server.server_close()


if __name__ == "__main__":
    main()
