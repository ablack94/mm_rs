#!/usr/bin/env python3
"""
Kraken MM Bot Dashboard
~~~~~~~~~~~~~~~~~~~~~~~
Single-file web dashboard that shows portfolio, positions, PnL, and open orders.

Data sources:
  - Bot REST API (localhost:3030) for state (positions, orders, realized PnL)
  - Kraken proxy (10.255.255.254:8080) for live ticker prices
  - logs/trades.csv for recent trade history

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
from pathlib import Path
from decimal import Decimal, InvalidOperation

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------
BOT_API_URL = os.environ.get("BOT_API_URL", "http://localhost:3030")
BOT_API_TOKEN = os.environ.get("BOT_API_TOKEN", "mcp-bot-token-2026")
PROXY_URL = os.environ.get("PROXY_URL", "http://10.255.255.254:8080")
PROXY_TOKEN = os.environ.get(
    "PROXY_TOKEN",
    "0ba154d4e4886b262f7752ebcb5213a57ea8161b88c4b5f4eed5b0f79363d7ab",
)
STATE_JSON = os.environ.get(
    "STATE_JSON",
    str(Path(__file__).resolve().parent / "state.json"),
)
TRADES_CSV = os.environ.get(
    "TRADES_CSV",
    str(Path(__file__).resolve().parent / "logs" / "trades.csv"),
)
DEFAULT_PORT = 8888

# ---------------------------------------------------------------------------
# Data helpers
# ---------------------------------------------------------------------------

def _bot_get(path: str) -> dict:
    """GET from the bot REST API."""
    req = urllib.request.Request(
        f"{BOT_API_URL}{path}",
        headers={"Authorization": f"Bearer {BOT_API_TOKEN}"},
    )
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


def _read_state_file() -> dict:
    """Read state.json directly from disk as fallback."""
    try:
        with open(STATE_JSON, "r") as f:
            return json.loads(f.read())
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


def _recent_trades(limit: int = 20) -> list:
    """Read last N trades from trades.csv."""
    try:
        with open(TRADES_CSV, "r") as f:
            lines = f.readlines()
    except Exception:
        return []
    if len(lines) <= 1:
        return []
    header = lines[0].strip().split(",")
    data_lines = lines[1:]
    recent = data_lines[-limit:]
    trades = []
    for line in reversed(recent):
        parts = line.strip().split(",")
        if len(parts) >= len(header):
            trades.append(dict(zip(header, parts)))
    return trades


def build_dashboard_data() -> dict:
    """Assemble all data for the dashboard JSON endpoint."""
    # 1. Get bot state (prefer API, fall back to file)
    state = _bot_get("/api/state")
    if "_error" in state:
        state = _read_state_file()

    positions_raw = state.get("positions", {})
    open_orders_raw = state.get("open_orders", {})
    realized_pnl = float(Decimal(state.get("realized_pnl", "0")))
    total_fees = float(Decimal(state.get("total_fees", "0")))
    trade_count = state.get("trade_count", 0)
    paused = state.get("paused", False)

    # 2. Fetch live prices for all positions
    symbols = list(positions_raw.keys())
    prices = _fetch_all_prices(symbols)

    # 3. Build position breakdown
    positions = []
    total_cost_basis = 0.0
    total_current_value = 0.0
    total_unrealized_pnl = 0.0

    for symbol, pos in sorted(positions_raw.items()):
        qty = float(Decimal(pos.get("qty", "0")))
        avg_cost = float(Decimal(pos.get("avg_cost", "0")))
        current_price = prices.get(symbol, 0.0)
        cost_basis = qty * avg_cost
        current_value = qty * current_price
        unrealized = current_value - cost_basis
        pct_change = ((current_price / avg_cost) - 1.0) * 100 if avg_cost > 0 else 0.0

        total_cost_basis += cost_basis
        total_current_value += current_value
        total_unrealized_pnl += unrealized

        positions.append({
            "symbol": symbol,
            "qty": qty,
            "avg_cost": avg_cost,
            "current_price": current_price,
            "cost_basis": cost_basis,
            "current_value": current_value,
            "unrealized_pnl": unrealized,
            "pct_change": pct_change,
        })

    # 4. Build open orders list
    orders = []
    for cl_ord_id, order in open_orders_raw.items():
        orders.append({
            "cl_ord_id": cl_ord_id,
            "symbol": order.get("symbol", ""),
            "side": order.get("side", ""),
            "price": float(Decimal(order.get("price", "0"))),
            "qty": float(Decimal(order.get("qty", "0"))),
            "placed_at": order.get("placed_at", ""),
            "acked": order.get("acked", False),
        })

    # 5. Recent trades
    recent_trades = _recent_trades(20)

    # 6. Portfolio totals
    net_pnl = realized_pnl + total_unrealized_pnl
    portfolio_value = total_current_value  # value of held positions

    return {
        "timestamp": time.strftime("%Y-%m-%d %H:%M:%S UTC", time.gmtime()),
        "paused": paused,
        "portfolio_value": portfolio_value,
        "total_cost_basis": total_cost_basis,
        "total_current_value": total_current_value,
        "realized_pnl": realized_pnl,
        "unrealized_pnl": total_unrealized_pnl,
        "net_pnl": net_pnl,
        "total_fees": total_fees,
        "trade_count": trade_count,
        "positions": positions,
        "open_orders": orders,
        "open_order_count": len(orders),
        "recent_trades": recent_trades,
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
  .status-dot.paused { background: var(--yellow); }
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
  .side-buy  { color: var(--green); font-weight: 600; }
  .side-sell { color: var(--red); font-weight: 600; }

  /* Bar for pnl visualization */
  .pnl-bar {
    display: inline-block;
    height: 4px;
    border-radius: 2px;
    vertical-align: middle;
    margin-left: 6px;
  }

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

  @media (max-width: 600px) {
    .summary-grid { grid-template-columns: repeat(2, 1fr); }
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
    <span id="lastUpdate">--</span>
    <span id="countdown"></span>
  </div>
</div>

<div class="refresh-bar"><div id="refreshBar" class="refresh-bar-inner"></div></div>

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
    <div class="card-label">Open Orders</div>
    <div class="card-value" id="openOrderCount">--</div>
    <div class="card-sub" id="botStatus">--</div>
  </div>
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
        <th class="num">Cost Basis</th>
        <th class="num">Value</th>
        <th class="num">Unrealized P&L</th>
        <th class="num">Change</th>
      </tr>
    </thead>
    <tbody id="positionsBody">
      <tr><td colspan="8" class="empty-msg">Loading...</td></tr>
    </tbody>
  </table>
</div>

<!-- Open Orders Table -->
<div class="section">
  <div class="section-title">Open Orders</div>
  <table>
    <thead>
      <tr>
        <th>Symbol</th>
        <th>Side</th>
        <th class="num">Price</th>
        <th class="num">Qty</th>
        <th>Placed At</th>
        <th>Status</th>
      </tr>
    </thead>
    <tbody id="ordersBody">
      <tr><td colspan="6" class="empty-msg">Loading...</td></tr>
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
        <th>Symbol</th>
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
  // Auto-detect decimals based on magnitude
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

function sideClass(s) {
  if (!s) return '';
  return s.toLowerCase() === 'buy' ? 'side-buy' : 'side-sell';
}

function updateDashboard(data) {
  // Status
  const dot = document.getElementById('statusDot');
  const statusEl = document.getElementById('botStatus');
  if (data._error) {
    dot.className = 'status-dot error';
    dot.title = 'Error';
    statusEl.textContent = 'Error fetching data';
  } else if (data.paused) {
    dot.className = 'status-dot paused';
    dot.title = 'Paused';
    statusEl.textContent = 'Bot paused';
  } else {
    dot.className = 'status-dot live';
    dot.title = 'Live';
    statusEl.textContent = 'Bot active';
  }

  document.getElementById('lastUpdate').textContent = data.timestamp || '--';

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

  document.getElementById('openOrderCount').textContent = data.open_order_count || 0;

  // Positions table
  const posBody = document.getElementById('positionsBody');
  if (!data.positions || data.positions.length === 0) {
    posBody.innerHTML = '<tr><td colspan="8" class="empty-msg">No open positions</td></tr>';
  } else {
    let html = '';
    for (const p of data.positions) {
      const cls = pnlClass(p.unrealized_pnl);
      html += '<tr>';
      html += '<td><strong>' + p.symbol + '</strong></td>';
      html += '<td class="num">' + fmtQty(p.qty) + '</td>';
      html += '<td class="num">' + fmtPrice(p.avg_cost) + '</td>';
      html += '<td class="num">' + fmtPrice(p.current_price) + '</td>';
      html += '<td class="num">' + fmtUsd(p.cost_basis) + '</td>';
      html += '<td class="num">' + fmtUsd(p.current_value) + '</td>';
      html += '<td class="num ' + cls + '">' + fmtUsd(p.unrealized_pnl) + '</td>';
      html += '<td class="num ' + cls + '">' + fmtPct(p.pct_change) + '</td>';
      html += '</tr>';
    }
    posBody.innerHTML = html;
  }

  // Open orders table
  const ordBody = document.getElementById('ordersBody');
  if (!data.open_orders || data.open_orders.length === 0) {
    ordBody.innerHTML = '<tr><td colspan="6" class="empty-msg">No open orders</td></tr>';
  } else {
    let html = '';
    for (const o of data.open_orders) {
      html += '<tr>';
      html += '<td><strong>' + o.symbol + '</strong></td>';
      html += '<td class="' + sideClass(o.side) + '">' + (o.side || '').toUpperCase() + '</td>';
      html += '<td class="num">' + fmtPrice(o.price) + '</td>';
      html += '<td class="num">' + fmtQty(o.qty) + '</td>';
      html += '<td>' + (o.placed_at || '--') + '</td>';
      html += '<td>' + (o.acked ? 'Acked' : 'Pending') + '</td>';
      html += '</tr>';
    }
    ordBody.innerHTML = html;
  }

  // Recent trades table
  const trBody = document.getElementById('tradesBody');
  if (!data.recent_trades || data.recent_trades.length === 0) {
    trBody.innerHTML = '<tr><td colspan="8" class="empty-msg">No recent trades</td></tr>';
  } else {
    let html = '';
    for (const t of data.recent_trades) {
      const pnl = parseFloat(t.pnl || 0);
      const cls = pnlClass(pnl);
      html += '<tr>';
      html += '<td>' + (t.timestamp || '--') + '</td>';
      html += '<td><strong>' + (t.symbol || '--') + '</strong></td>';
      html += '<td class="' + sideClass(t.side) + '">' + (t.side || '').toUpperCase() + '</td>';
      html += '<td class="num">' + fmtPrice(parseFloat(t.price)) + '</td>';
      html += '<td class="num">' + fmtQty(parseFloat(t.qty)) + '</td>';
      html += '<td class="num">' + fmtUsd(parseFloat(t.value_usd)) + '</td>';
      html += '<td class="num">' + fmtUsd(parseFloat(t.fee)) + '</td>';
      html += '<td class="num ' + cls + '">' + fmtUsd(pnl) + '</td>';
      html += '</tr>';
    }
    trBody.innerHTML = html;
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
    print(f"  Bot API: {BOT_API_URL}")
    print(f"  Proxy:   {PROXY_URL}")
    print(f"  State:   {STATE_JSON}")
    print(f"  Trades:  {TRADES_CSV}")
    print()
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nShutting down.")
        server.server_close()


if __name__ == "__main__":
    main()
