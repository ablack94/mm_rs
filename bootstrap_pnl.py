#!/usr/bin/env python3
"""Bootstrap pnl_state.json from Kraken trade history via proxy."""

import json
import os
import sys
import urllib.request
from decimal import Decimal, getcontext
from datetime import datetime, timezone

getcontext().prec = 50

PROXY_URL = os.environ.get("PROXY_URL", "http://10.255.255.254:8080")
PROXY_TOKEN = os.environ.get("PROXY_TOKEN", "")
OUTPUT_FILE = "pnl_state.json"

# Pairs to exclude (manual trades, not MM activity)
EXCLUDE_PAIRS = {"XXRPZUSD", "XRPUSD"}

# MM-only pairs (bot traded these). If set, only include these pairs.
# Set to None to include everything.
MM_PAIRS = {
    "RAINUSD", "OMGUSD", "FXSUSD", "HOUSEUSD", "SUPUSD",
    "KEEPUSD", "CAMPUSD",
}
# To include ALL account trades, set MM_PAIRS = None


def kraken_post(endpoint, data=None):
    """POST to Kraken private API via proxy."""
    url = f"{PROXY_URL}{endpoint}"
    body = ""
    if data:
        body = "&".join(f"{k}={v}" for k, v in data.items())

    req = urllib.request.Request(
        url,
        data=body.encode() if body else b"",
        headers={
            "Authorization": f"Bearer {PROXY_TOKEN}",
            "Content-Type": "application/x-www-form-urlencoded",
        },
        method="POST",
    )
    with urllib.request.urlopen(req) as resp:
        return json.loads(resp.read().decode())


def normalize_pair(kraken_pair):
    """Convert Kraken pair name to standard format: RAINUSD -> RAIN/USD."""
    # Common suffixes
    for suffix in ["USD", "EUR", "GBP", "BTC", "ETH"]:
        if kraken_pair.endswith(suffix):
            base = kraken_pair[: -len(suffix)]
            # Strip Kraken X/Z prefix if present
            if base.startswith("XX") or base.startswith("XZ"):
                base = base[2:]
            elif base.startswith("X") and len(base) > 3:
                base = base[1:]
            return f"{base}/{suffix}"
    return kraken_pair


def fetch_all_trades():
    """Fetch all trades from Kraken, handling pagination."""
    all_trades = []
    ofs = 0
    while True:
        print(f"  Fetching trades offset={ofs}...")
        result = kraken_post("/0/private/TradesHistory", {"ofs": str(ofs)})

        if result.get("error"):
            print(f"  API error: {result['error']}", file=sys.stderr)
            break

        trades = result.get("result", {}).get("trades", {})
        count = result.get("result", {}).get("count", 0)

        if not trades:
            break

        for txid, trade in trades.items():
            trade["txid"] = txid
            all_trades.append(trade)

        ofs += len(trades)
        print(f"  Got {len(trades)} trades (total so far: {len(all_trades)}/{count})")

        if len(all_trades) >= count:
            break

    return all_trades


class Position:
    def __init__(self):
        self.qty = Decimal("0")
        self.avg_cost = Decimal("0")

    def apply_buy(self, qty, price):
        new_qty = self.qty + qty
        if new_qty > 0:
            self.avg_cost = (self.qty * self.avg_cost + qty * price) / new_qty
        self.qty = new_qty

    def apply_sell(self, qty, price):
        pnl = qty * (price - self.avg_cost)
        self.qty -= qty
        if self.qty <= 0:
            self.qty = Decimal("0")
            self.avg_cost = Decimal("0")
        return pnl

    def is_empty(self):
        return self.qty <= 0

    def to_dict(self):
        return {"qty": str(self.qty), "avg_cost": str(self.avg_cost)}


def main():
    if not PROXY_TOKEN:
        print("Error: PROXY_TOKEN not set", file=sys.stderr)
        sys.exit(1)

    print("Fetching all trades from Kraken via proxy...")
    trades = fetch_all_trades()
    print(f"Total trades fetched: {len(trades)}")

    # Sort by timestamp
    trades.sort(key=lambda t: t["time"])

    # Filter trades
    filtered = []
    excluded_count = 0
    seen_pairs = set()
    for t in trades:
        pair = t["pair"]
        seen_pairs.add(pair)
        if pair in EXCLUDE_PAIRS:
            excluded_count += 1
            continue
        if MM_PAIRS is not None and pair not in MM_PAIRS:
            excluded_count += 1
            continue
        filtered.append(t)

    print(f"All pairs on account: {sorted(seen_pairs)}")
    if excluded_count:
        print(f"Excluded {excluded_count} non-MM trades")

    # Replay trades
    positions = {}  # symbol -> Position
    realized_pnl = Decimal("0")
    total_fees = Decimal("0")
    trade_count = 0
    recent_trades = []

    for t in filtered:
        symbol = normalize_pair(t["pair"])
        side = t["type"]  # "buy" or "sell"
        price = Decimal(t["price"])
        qty = Decimal(t["vol"])
        fee = Decimal(t["fee"])
        ts = t["time"]

        if symbol not in positions:
            positions[symbol] = Position()

        pos = positions[symbol]
        pnl = Decimal("0")

        if side == "buy":
            pos.apply_buy(qty, price)
        elif side == "sell":
            pnl = pos.apply_sell(qty, price) - fee
            realized_pnl += pnl
            if pos.is_empty():
                del positions[symbol]

        total_fees += fee
        trade_count += 1

        # Convert unix timestamp to ISO format
        dt = datetime.fromtimestamp(ts, tz=timezone.utc)
        ts_str = dt.strftime("%Y-%m-%dT%H:%M:%SZ")

        recent_trades.append(
            {
                "timestamp": ts_str,
                "symbol": symbol,
                "side": side,
                "price": str(price),
                "qty": str(qty),
                "fee": str(fee),
                "pnl": str(pnl),
            }
        )

    # Build output matching PnlTracker JSON format
    output = {
        "positions": {
            sym: pos.to_dict() for sym, pos in positions.items() if not pos.is_empty()
        },
        "realized_pnl": str(realized_pnl),
        "total_fees": str(total_fees),
        "trade_count": trade_count,
        "recent_trades": recent_trades,
    }

    # rust_decimal supports max 28 significant digits — truncate
    def truncate_decimal(s, max_sig=28):
        """Truncate a decimal string to max significant digits."""
        if not s or s == "0":
            return s
        neg = s.startswith("-")
        s_abs = s.lstrip("-")
        # Count significant digits (skip leading zeros and decimal point)
        digits = ""
        for c in s_abs:
            if c == ".":
                continue
            if not digits and c == "0":
                continue
            digits += c
        if len(digits) <= max_sig:
            return ("-" if neg else "") + s_abs
        # Need to truncate - find position in original string
        sig_count = 0
        result = []
        for c in s_abs:
            if c == ".":
                result.append(c)
                continue
            if sig_count == 0 and c == "0":
                result.append(c)
                continue
            sig_count += 1
            if sig_count <= max_sig:
                result.append(c)
            # Stop after max_sig significant digits
        return ("-" if neg else "") + "".join(result)

    def truncate_in_obj(obj):
        if isinstance(obj, dict):
            return {k: truncate_in_obj(v) for k, v in obj.items()}
        elif isinstance(obj, list):
            return [truncate_in_obj(v) for v in obj]
        elif isinstance(obj, str):
            try:
                Decimal(obj)  # verify it's a decimal string
                return truncate_decimal(obj)
            except Exception:
                return obj
        else:
            return obj

    output = truncate_in_obj(output)

    with open(OUTPUT_FILE, "w") as f:
        json.dump(output, f, indent=2)

    print(f"\nWritten {OUTPUT_FILE}")
    print(f"  Trades: {trade_count}")
    print(f"  Realized PnL: ${realized_pnl:.6f}")
    print(f"  Total Fees: ${total_fees:.6f}")
    print(f"  Open Positions: {len(output['positions'])}")
    for sym, pos in output["positions"].items():
        print(f"    {sym}: qty={pos['qty']}, avg_cost={pos['avg_cost']}")


if __name__ == "__main__":
    main()
