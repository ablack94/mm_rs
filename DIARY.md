# Trading Bot Diary

## 2026-02-18 — The WARD/USD Incident (-$140 day)

### What Happened

The bot lost approximately $140 trading primarily WARD/USD and secondarily ALLO/USD. This was the worst single-day loss since the bot went live.

### Timeline

**14:57** — Bot starts buying WARD/USD at ~$0.0275. First round: 7 rapid buys totaling ~$50 in under 10 seconds. No sells placed because the spread is too narrow for profitable limit sells.

**15:03** — First batch of sells goes through at $0.02779, small profit. Looks like normal operation.

**15:22–15:25** — Second accumulation cycle. Bot buys ~20 fills of WARD without any sells. Accumulated ~6500 WARD ($170+ exposure) despite `max_inventory_usd` being $200. Individual fills were small (~$6 each) but they stacked up because there was no gating on consecutive buys.

**15:30** — Stop-loss triggers. Bot does a market sell of 6495 WARD at $0.02642 (avg cost was ~$0.0275). Single fill: **-$7.55 realized**. This ate through the thin WARD order book.

**16:30–16:35** — Bot auto-re-enables after cooldown, immediately re-enters WARD. Another ~20 consecutive buys without any sell. Same pattern.

**17:19** — Second catastrophic liquidation. Market sell dumps 6000+ WARD in 8 fills ranging $0.02408 down to $0.0239. Realized: **-$14.75 cumulative**. The thin book meant each sell ate deeper into lower bids.

**18:19–18:21** — Bot re-enables *again*. Third accumulation cycle: 28 consecutive buys in 2 minutes, accumulating ~7300 WARD.

**18:39** — Third liquidation at $0.02303 for 7363 units. Realized: **-$25.05 cumulative**.

**19:39–19:50** — Fourth cycle. Bot re-enables, buys 40+ times consecutively. Accumulated thousands more WARD.

**19:42–19:50** — Frantic buy/sell cycling as the bot tries to trade its way out. Multiple liquidation rounds.

**20:14** — Fifth accumulation: 1792 WARD bought at $0.02408.

**23:55** — Final catastrophic exit. WARD has fallen to $0.0178. Market sells 6527 WARD at $0.01783-$0.01786. This is 35% below the ~$0.0275 entry price. Single fill of 6279 WARD realized **-$39.40**. WARD cumulative: **-$61.30**.

**23:56** — ALLO/USD final exit at $0.0917-$0.0929 (entered at ~$0.098). Two sells realize **-$10.92 combined**. Day ends at **-$72.23** per trade logger.

The actual loss was closer to **~$140** because the PnL analyzer's WebSocket disconnected mid-day and stopped tracking fills. The dashboard showed only -$20 while the real number was 7x worse.

### WARD/USD Stats for the Day
- **202 total fills** (majority were buys)
- **5+ liquidation cycles** — bot kept re-entering after each stop-loss
- Longest buy streak: **40+ consecutive buys** without a single sell
- Price fell from $0.0275 to $0.0178 over the day (-35%)
- Book was extremely thin — market sells of even $50 moved price significantly

### Root Causes

1. **No buy gating**: Bot placed 40+ consecutive buys without ever selling. Each individual buy was within size limits, but cumulatively they created massive one-sided exposure. The `max_inventory_usd` cap was hit repeatedly but the bot kept trying to buy on every book update.

2. **No quote throttling**: On thin books, every book update triggered a requote. WARD's book changed constantly, generating hundreds of quote operations per minute. This exhausted Kraken's rate limit counter (917 rate limit hits logged).

3. **Auto re-enable after cooldown**: After each stop-loss liquidation, the bot waited out the 1-hour cooldown, then automatically re-enabled WARD and re-entered the same losing position. This happened 5+ times, compounding losses each cycle.

4. **Market sell on thin books**: Stop-loss triggered `Liquidating` state, which does a market sell. On WARD's thin book, a market sell of $170 moved the price down 10-35%. The slippage on liquidation was often worse than the stop-loss threshold itself.

5. **PnL analyzer blind spot**: The PnL analyzer WebSocket disconnected and didn't process the execution snapshot on reconnect. It missed most of the day's fills, so the dashboard showed -$20 instead of the real -$140. No staleness detection existed to flag this.

### Fixes Implemented (2026-02-19)

All changes in a single session, all tests passing (80 unit + 9 integration + 30 mock-exchange).

| # | Fix | Effect |
|---|-----|--------|
| 1 | **Buy gating** — `buys_without_sell` counter per pair, configurable `max_buys_before_sell` (default: 2) | Bot must have a sell fill before buying more. Would have limited each WARD cycle to 2 buys instead of 40+ |
| 2 | **Quote throttling** — `min_quote_interval_secs` (default: 5s) on book-update triggers only | Max 12 quote ops/min/pair from book updates. Fill and tick triggers bypass throttle. Would have prevented rate limit exhaustion |
| 3 | **No auto re-enable** — cooldown expiry keeps pair Disabled | Bot stops re-entering losing pairs. Manual or analyzer re-enable required. Would have stopped the 5-cycle compound loss |
| 4 | **WindDown for stop-loss** — `use_winddown_for_stoploss` (default: true) with escalation timer | Micro-caps exit via limit sells (WindDown) instead of market sells. Escalates to Liquidating after 4 hours if position won't clear. Would have avoided $39 single-fill slippage |
| 5 | **PnL snapshot processing** — process unseen fills from execution snapshot on WS reconnect | Recovers fills missed during disconnection via `seen_order_ids` dedup |
| 6 | **Staleness detection** — `last_fill_time` in /pnl response, banner in dashboard, check in monitor.sh | Dashboard shows warning when PnL data is >5 minutes stale while bot is connected |

### Config Additions

```
# Per-pair (PairConfig, all optional — None = use global default)
max_buys_before_sell: Option<u32>      # e.g. 1 for micro-caps, 10 for BTC
use_winddown_for_stoploss: Option<bool> # false for liquid assets

# Global defaults
max_buys_before_sell: 2
use_winddown_for_stoploss: true

# Trading config
min_quote_interval_secs: 5  # env: MIN_QUOTE_INTERVAL_SECS

# Risk config
winddown_escalation_hours: 4  # env: WINDDOWN_ESCALATION_HOURS
```

### Lessons

- **Single-fill size limits are insufficient** — need to track cumulative directional exposure (consecutive buys) not just per-order size
- **Thin books need different exit strategies** — market sells that work fine on BTC are catastrophic on micro-caps
- **Auto-recovery is dangerous** — automatic re-enable after liquidation compounds losses when the underlying cause (bad pair) hasn't changed
- **Monitoring must detect its own blindness** — the PnL dashboard was useless precisely when it mattered most because it didn't know its own data was stale
- **Rate limits are a canary** — 917 rate limit hits should have triggered an automatic pause, not just a warning
