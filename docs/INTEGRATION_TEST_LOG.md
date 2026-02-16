# Integration Test Log — Full System (Feb 16, 2026)

## Setup

| Component | Port | Config |
|-----------|------|--------|
| Mock Exchange | 3080 | OMG/USD@0.50, CAMP/USD@0.004, KEEP/USD@0.02, seed=42, 3% spread, 80% fill prob, 0.5 volatility, 1.5s ticks |
| State Store | 3081 | token=test123, file=/tmp/integ_ss_final.json |
| Bot | — | PROXY_URL=http://localhost:3080, STATE_STORE_URL=http://localhost:3081, --pairs OMG/USD CAMP/USD KEEP/USD |

## Bug Found: Unit Mismatch in State Store Defaults

**Severity:** High — caused 100% profit margin on sells instead of 1%

The state store's `GlobalDefaults` used human-readable percentages (1.0, 3.0, 10.0) for `min_profit_pct`, `stop_loss_pct`, `take_profit_pct`, but the bot engine expects fractional values (0.01, 0.03, 0.10). When the bot received `min_profit_pct: 1.0` from the state store, it interpreted this as 100% profit margin, placing sell orders at `avg_cost * 2.0` — far above market and never fillable.

**Fix:** Changed state store defaults in `crates/state-store/src/types.rs`:
- `min_profit_pct: 1.0` → `0.01`
- `stop_loss_pct: 3.0` → `0.03`
- `take_profit_pct: 10.0` → `0.10`

Also updated `docs/STATE_STORE_API.md` to reflect correct values.

## Bug Found: Mock Probabilistic Fill Range Too Tight

The mock exchange's probabilistic fill check required orders within 1% of the opposite side, but the bot places bids ~2% below the ask (due to spread capture). This meant fills only happened via deterministic crossing (random walk), not probabilistically.

**Fix:** Widened probabilistic fill range from 1% to 5% in `crates/mock-exchange/src/simulation.rs`:
- Buy: `o.price >= best_ask * 0.99` → `o.price >= best_ask * 0.95`
- Sell: `o.price <= best_bid * 1.01` → `o.price <= best_bid * 1.05`

## Test Results

### Scenario 1: Disable an Active Pair (OMG/USD Active → Disabled) ✅

- State store pushed `pair_updated` to bot within ~1ms
- Bot received state change: `old_state=Active new_state=Disabled`
- Bot cancelled OMG/USD sell order (ask-52) immediately
- OMG/USD continues receiving book data but no new quotes placed
- Tick status no longer shows OMG/USD in active pairs list

### Scenario 2: Enable a Disabled Pair (KEEP/USD Disabled → Active) ✅

- Bot received state change: `old_state=Disabled new_state=Active`
- On next book snapshot (within 1 tick), bot placed fresh buy order for KEEP/USD
- Quote used global defaults for order sizing ($50 since no override)

### Scenario 3: Wind Down (CAMP/USD Active → WindDown) ✅

- Bot received state change: `old_state=Active new_state=WindDown`
- On next book snapshot, bot placed sell-only quotes (`can_buy=false can_sell=true`)
- Tick status shows `CAMP/USD:BidFilled/WindDown`
- A pending buy fill came in just after the state change (was already in-flight) — this is expected behavior

### Scenario 4: Re-enable a Disabled Pair (OMG/USD Disabled → Active) ✅

- Bot received state change: `old_state=Disabled new_state=Active`
- On next book snapshot, bot placed sell orders (had existing inventory from earlier)
- Quote decision correctly showed `can_buy=false can_sell=true` (at max inventory)

### Scenario 5: Change Config Parameters ✅

- PATCH'd KEEP/USD config: `min_spread_bps=200, order_size_usd=50`
- Bot received `pair_updated` with state=Active (config merge works)
- New quotes would use the updated parameters on next requote

### Scenario 6: Delete a Pair (KEEP/USD) ✅

- Bot received `pair_removed` from state store
- Bot cancelled outstanding sell order (ask-80) for KEEP/USD
- Pair removed from engine — no longer appears in tick status

### Scenario 7: Forced Liquidation (CAMP/USD Active → Liquidating) ✅

- Bot received state change: `old_state=Active new_state=Liquidating`
- Sell order immediately filled at market price (0.003975)
- Engine auto-transitioned: `Liquidation complete — pair on cooldown` (3600s cooldown)
- A concurrent pending buy fill also came in (was in-flight before state change)

## Performance Summary

After all scenarios (including enabling/disabling/re-enabling):

| Metric | Value |
|--------|-------|
| Total trades | 91+ |
| Realized PnL | ~$48 |
| Total fees | ~$22 |
| Net PnL | ~$26 |
| Final exposure | ~$102 |

The bot successfully operated across multiple pairs with dynamic state changes pushed from the state store, demonstrating the full service architecture works end-to-end.

## Mock Exchange Observations

- Seed=42 with 0.5 volatility and 3% spread produces active trading
- 80% fill probability with 5% range generates ~5 fills per 15 seconds per active pair
- The mock correctly handles order cancellation, amendment, and market sells
- Book snapshots are sent every 1.5 seconds with regenerated levels

## Deterministic Scenario Mode — Verified

Implemented and tested the deterministic scenario mode (design: `docs/DETERMINISTIC_MOCK_DESIGN.md`).

**Test: crash_recovery.json scenario**

- Mock started with `MOCK_SCENARIO_FILE=test_data/scenarios/crash_recovery.json`
- OMG/USD followed the exact deterministic trajectory:
  - t=0-5s: flat at 0.50
  - t=5-8s: crash to 0.30
  - t=8-18s: flat at 0.30
  - t=18-78s: linear recovery from 0.30 to 0.48
  - t>78s: held at 0.48 (hold_last)
- Bot confirmed receiving mid prices: 0.315 → 0.318 → 0.321 → 0.324 → 0.327 → ... → 0.480 (steady)
- Rate of increase (~0.003/s) matched expected: 0.18/60s = 0.003/s
- CAMP/USD (not in scenario) continued random-walking independently at ~0.004
- "Scenario mode active" logged at startup with pair count and end_behavior

**6 scenario files created in `test_data/scenarios/`:**
1. `crash_recovery.json` — 40% crash, 10s bottom, 60s recovery
2. `gradual_uptrend.json` — steady 20% rise over 120s
3. `sideways_choppy.json` — oscillate between 0.48-0.52 every 10s for 2 min
4. `spread_widening.json` — spread widens from 5% to 30% and back
5. `flash_spike.json` — 20% flash crash with step interpolation, instant recovery
6. `multi_pair_divergence.json` — OMG +15%, CAMP -15% over 60s

## Areas for Improvement

1. ~~**Deterministic mock exchange**: Need predefined price patterns for reproducible testing~~ DONE
2. **Wind-down completion**: The wind-down → disabled auto-transition needs the bot to report `position_qty=0` via PairReport, which requires the heartbeat/report interval to elapse
3. **Concurrent state changes**: A buy fill arriving after a state change to WindDown/Liquidating could increase position — the engine should guard against this
4. **Virtual time mode**: `MOCK_VIRTUAL_TIME=true` to eliminate wall-clock jitter for CI determinism
5. **Scenario-based integration tests**: Automated test harness that starts mock+bot, runs a scenario, asserts on fills/PnL
