#!/usr/bin/env python3
"""P&L analysis for kraken-mm bot. Reads trades.csv + state.json."""

import csv
import json
import sys
from collections import defaultdict
from decimal import Decimal, ROUND_HALF_UP

TRADE_LOG = "logs/trades.csv"
STATE_FILE = "state.json"

D = Decimal

def load_trades(path=TRADE_LOG):
    trades = []
    with open(path) as f:
        reader = csv.DictReader(f)
        for row in reader:
            # Skip empty-symbol rows (old bug)
            if not row["symbol"].strip():
                continue
            trades.append({
                "timestamp": row["timestamp"],
                "symbol": row["symbol"],
                "side": row["side"],
                "price": D(row["price"]),
                "qty": D(row["qty"]),
                "value_usd": D(row["value_usd"]),
                "fee": D(row["fee"]),
                "pnl": D(row["pnl"]),
            })
    return trades

def load_state(path=STATE_FILE):
    with open(path) as f:
        return json.load(f)

def analyze(trades, state, exclude_symbols=None):
    exclude = set(exclude_symbols or [])

    # Per-pair stats
    pairs = defaultdict(lambda: {
        "buys": 0, "sells": 0,
        "buy_vol": D(0), "sell_vol": D(0),
        "fees": D(0), "realized_pnl": D(0),
    })

    total_vol = D(0)
    total_fees = D(0)
    total_realized = D(0)
    total_trades = 0

    for t in trades:
        sym = t["symbol"]
        if sym in exclude:
            continue
        p = pairs[sym]
        total_trades += 1
        total_fees += t["fee"]

        if t["side"] == "buy":
            p["buys"] += 1
            p["buy_vol"] += t["value_usd"]
        else:
            p["sells"] += 1
            p["sell_vol"] += t["value_usd"]
            p["realized_pnl"] += t["pnl"]
            total_realized += t["pnl"]

        p["fees"] += t["fee"]
        total_vol += t["value_usd"]

    # Current positions (unrealized)
    positions = state.get("positions", {})
    # We don't have live prices, but we can estimate from last trade prices
    last_prices = {}
    for t in trades:
        if t["symbol"] not in exclude:
            last_prices[t["symbol"]] = t["price"]

    total_unrealized = D(0)
    total_position_value = D(0)

    print("=" * 80)
    print("KRAKEN MM BOT — P&L REPORT")
    print("=" * 80)

    if exclude:
        print(f"(Excluding: {', '.join(sorted(exclude))})")
    print()

    # Per-pair breakdown
    print(f"{'Pair':<12} {'Buys':>5} {'Sells':>5} {'BuyVol':>10} {'SellVol':>10} {'Fees':>8} {'Realized':>10} {'Position':>10}")
    print("-" * 80)

    for sym in sorted(pairs.keys()):
        p = pairs[sym]
        pos = positions.get(sym, {})
        pos_qty = D(pos.get("qty", "0"))
        pos_avg = D(pos.get("avg_cost", "0"))
        pos_value = pos_qty * pos_avg

        # Estimate unrealized P&L (using last trade price vs avg cost)
        if sym in last_prices and not pos_qty.is_zero():
            unrealized = pos_qty * (last_prices[sym] - pos_avg)
            total_unrealized += unrealized
            total_position_value += pos_qty * last_prices[sym]
        else:
            unrealized = D(0)
            total_position_value += pos_value

        print(f"{sym:<12} {p['buys']:>5} {p['sells']:>5} "
              f"${p['buy_vol'].quantize(D('0.01')):>9} "
              f"${p['sell_vol'].quantize(D('0.01')):>9} "
              f"${p['fees'].quantize(D('0.01')):>7} "
              f"${p['realized_pnl'].quantize(D('0.01')):>9} "
              f"${pos_value.quantize(D('0.01')):>9}")

    print("-" * 80)
    print(f"{'TOTAL':<12} {total_trades:>11} "
          f"${(sum(p['buy_vol'] for p in pairs.values())).quantize(D('0.01')):>9} "
          f"${(sum(p['sell_vol'] for p in pairs.values())).quantize(D('0.01')):>9} "
          f"${total_fees.quantize(D('0.01')):>7} "
          f"${total_realized.quantize(D('0.01')):>9} "
          f"${total_position_value.quantize(D('0.01')):>9}")

    print()
    print("SUMMARY")
    print("-" * 40)
    print(f"  Total volume:         ${total_vol.quantize(D('0.01'))}")
    print(f"  Total trades:         {total_trades}")
    print(f"  Total fees:           ${total_fees.quantize(D('0.01'))}")
    print(f"  Realized P&L:         ${total_realized.quantize(D('0.01'))}")
    print(f"  Unrealized P&L (est): ${total_unrealized.quantize(D('0.01'))}")
    print(f"  Net P&L (est):        ${(total_realized + total_unrealized).quantize(D('0.01'))}")
    print(f"  Position value:       ${total_position_value.quantize(D('0.01'))}")

    # Profitability metrics
    if total_vol > 0:
        fee_pct = (total_fees / total_vol * 100).quantize(D('0.001'))
        pnl_pct = (total_realized / total_vol * 100).quantize(D('0.001'))
        print(f"  Fee % of volume:      {fee_pct}%")
        print(f"  Realized P&L/volume:  {pnl_pct}%")

    # Round-trip analysis: pair completed buy+sell cycles
    print()
    print("ROUND-TRIP ANALYSIS")
    print("-" * 40)
    completed = 0
    profitable = 0
    for sym, p in sorted(pairs.items()):
        rt = min(p["buys"], p["sells"])
        if rt > 0:
            won = "+" if p["realized_pnl"] > 0 else ""
            print(f"  {sym:<12} {rt:>3} round-trips  P&L: {won}${p['realized_pnl'].quantize(D('0.01'))}")
            completed += rt
            if p["realized_pnl"] > 0:
                profitable += 1

    if completed > 0:
        print(f"  Total round-trips: {completed}")
        print(f"  Profitable pairs:  {profitable}/{len([s for s,p in pairs.items() if min(p['buys'],p['sells'])>0])}")
    else:
        print("  No completed round-trips yet")

    print()

    # Open orders from state
    open_orders = state.get("open_orders", {})
    if open_orders:
        print(f"OPEN ORDERS ({len(open_orders)})")
        print("-" * 40)
        for oid, o in sorted(open_orders.items()):
            acked = "ACK" if o.get("acked") else "PND"
            print(f"  [{acked}] {o['side']:>4} {o['symbol']:<12} "
                  f"price={o['price']} qty={D(o['qty']).quantize(D('0.01'))}")
        print()

def main():
    exclude = set()
    # Parse --exclude flag
    args = sys.argv[1:]
    i = 0
    while i < len(args):
        if args[i] == "--exclude":
            i += 1
            while i < len(args) and not args[i].startswith("--"):
                exclude.add(args[i])
                i += 1
        elif args[i] == "--mm-only":
            # Common shortcut: exclude pre-existing XRP trades
            exclude.add("XRP/USD")
            i += 1
        else:
            i += 1

    trades = load_trades()
    state = load_state()
    analyze(trades, state, exclude)

if __name__ == "__main__":
    main()
