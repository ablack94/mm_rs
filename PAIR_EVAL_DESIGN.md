# Dynamic Pair Evaluator -- Design Document

## Problem Statement

The bot currently loads pairs from a static `scanned_pairs.json` file at startup.
Once loaded, every pair stays active for the entire session regardless of how
market conditions change.  In practice, low-liquidity pairs can swing between
"wide spread, decent volume" (profitable) and "wide spread, zero volume" (dead
money tying up risk budget) or "tight spread, high volume" (unprofitable after
fees).  The bot needs a runtime feedback loop that periodically re-evaluates
each pair and gracefully transitions them between states.

---

## 1. Architecture Overview

### Where It Fits

The pair evaluator is **not** a separate binary.  It lives inside the engine's
existing event loop as a new subsystem invoked on a timer tick.

```
                        +-----------------+
  EngineEvent::Tick --> |   Engine::      |
                        |   on_tick()     |
                        |                 |
                        |  [existing]     |
                        |  - stale orders |
                        |  - stop-loss    |
                        |  - DMS refresh  |
                        |                 |
                        |  [NEW]          |
                        |  - pair_eval()  |
                        +-----------------+
                              |
                              v
                        EngineCommand::FetchTickers  (new command)
                              |
                              v
                        Command dispatcher calls
                        exchange.get_tickers()
                              |
                              v
                        EngineEvent::TickerRefresh { tickers }  (new event)
                              |
                              v
                        Engine::on_ticker_refresh()
                        evaluates each pair, transitions state machine
```

The evaluation itself is split into two phases to keep the engine I/O-free:

1. **Phase 1 (on_tick)**: The engine checks if enough time has elapsed since the
   last evaluation.  If so, it emits `EngineCommand::FetchTickers`.
2. **Phase 2 (on_ticker_refresh)**: The command dispatcher fetches tickers via
   the existing `ExchangeClient::get_tickers()` method and sends the result
   back as `EngineEvent::TickerRefresh`.  The engine then runs the evaluation
   logic using the fresh ticker data combined with its internal metrics.

This two-phase pattern mirrors how the existing DMS refresh works: the engine
emits a command, the dispatcher performs I/O, and the result flows back as an
event.

### Data Flow

```
Engine (pure logic, no I/O)
  |
  |-- on_tick(): "Is it time to re-evaluate?"
  |     YES --> emit FetchTickers command
  |
  |-- on_ticker_refresh(tickers):
  |     for each active pair:
  |       1. Check volume floor
  |       2. Check spread viability
  |       3. Check internal profitability metrics
  |       4. Decide: keep Active, move to SellOnly, move to Disabled
  |     for each disabled pair:
  |       1. Check if conditions have improved
  |       2. Decide: keep Disabled, move to Cautious (re-enable)
```

---

## 2. Data Sources

### External (API calls)

| Data | Source | Frequency | Rate Limit Cost |
|------|--------|-----------|-----------------|
| 24h volume, spread, price change | `GET /0/public/Ticker?pair=X,Y,Z` | Every eval interval (10 min) | 1 call (public, batched) |

This is the **only** new API call.  The bot already calls `get_tickers()` at
startup for the downtrend filter.  Reusing the same method at runtime adds
exactly one public REST call per evaluation cycle.  Kraken's public endpoint
rate limit is generous (~1 req/sec sustained) so one call every 10 minutes is
negligible.

No OHLC or trade history calls are needed.  The engine already has all the
internal data it needs from fills and order book updates.

### Internal (already available in engine state)

| Metric | Source | Location |
|--------|--------|----------|
| Per-pair realized PnL | `BotState.realized_pnl` per fill | `on_fill()` in `core.rs` |
| Per-pair trade count | Counted from fills | `on_fill()` |
| Per-pair fill timestamps | `Fill.timestamp` | `on_fill()` |
| Current spread | `OrderBook.spread_bps()` | `books` HashMap |
| Current mid price | `OrderBook.mid_price()` | `books` HashMap |
| Position size | `BotState.positions` | `state.position(symbol)` |
| Maker fee | `PairInfo.maker_fee_pct` | `pair_info` HashMap |
| Quoter state | `Quoter.state` | `quoters` HashMap |

### New Internal Tracking (to be added to engine)

A new `PairMetrics` struct stored per-pair inside the engine:

```
PairMetrics {
    // Accumulated from fills
    realized_pnl: Decimal,          // sum of fill PnL for this pair
    trade_count: u64,               // number of fills for this pair
    last_fill_time: Option<DateTime<Utc>>,
    consecutive_losses: u32,        // reset on profitable fill

    // From ticker refresh
    volume_24h_usd: Decimal,        // last known 24h volume
    last_ticker_spread_pct: Decimal, // spread at last ticker fetch
    last_eval_time: Option<DateTime<Utc>>,

    // State machine
    pair_state: PairState,
    state_entered_at: DateTime<Utc>,
    disable_reason: Option<String>,
    cooldown_until: Option<DateTime<Utc>>,  // prevents flapping
}
```

This data is already implicitly available (fills flow through `on_fill()`, books
are in `self.books`).  The only new work is accumulating per-pair counters
instead of a single global `realized_pnl`.

---

## 3. Evaluation Criteria

All thresholds are configurable via the new `PairEvalConfig` section.  The
defaults below are calibrated for low-liquidity USD pairs on Kraken.

### 3.1 Volume Floor

**Rationale**: A pair with $50/day volume will never fill your orders frequently
enough to justify the risk budget and API overhead.

| Parameter | Default | Description |
|-----------|---------|-------------|
| `min_volume_24h_usd` | `500.0` | Disable pairs below this 24h USD volume |
| `warn_volume_24h_usd` | `2000.0` | Log a warning (informational, no action) |

**Action**: If `volume_24h_usd < min_volume_24h_usd` for two consecutive
evaluation cycles (to avoid disabling on a single bad read), transition to
SellOnly.

Why two cycles: Kraken's 24h volume can be lumpy.  A pair might show $300 volume
at 2 AM and $3,000 at 2 PM.  Two consecutive failures (20 minutes apart by
default) gives enough confidence that the pair is genuinely dead.

### 3.2 Spread Viability

**Rationale**: If the spread narrows below the fee breakeven, every fill loses
money.

| Parameter | Default | Description |
|-----------|---------|-------------|
| `min_profitable_spread_pct` | `2 * maker_fee_pct` | Spread must exceed 2x maker fee |
| `spread_check_source` | `"book"` | Use live book spread (already checked per-quote by the quoter) |

**Action**: The quoter already returns `None` when the spread is too narrow
(line 57-63 of `quoter.rs`), which cancels quotes.  The evaluator adds a
*sustained* check: if the book spread has been below the profitable threshold
for the last N evaluation cycles (default 3, i.e., 30 minutes), transition to
SellOnly.  This prevents the bot from keeping a pair active when the spread is
structurally narrowing (e.g., a competing market maker entered).

This is complementary to, not replacing, the existing per-quote spread check.
The per-quote check handles transient spread narrowing (cancels quotes
immediately).  The evaluator handles structural changes (transitions pair
state).

### 3.3 Profitability Tracking

**Rationale**: A pair might have wide spread and decent volume but still lose
money due to adverse selection (getting picked off by informed traders).

| Parameter | Default | Description |
|-----------|---------|-------------|
| `max_consecutive_losses` | `5` | Disable after N consecutive losing round-trips |
| `min_pnl_per_pair_usd` | `-10.0` | Disable if cumulative pair PnL drops below this |
| `profitability_eval_min_trades` | `3` | Don't evaluate profitability until at least N trades |

**Action**: After `profitability_eval_min_trades` fills on a pair:
- If `consecutive_losses >= max_consecutive_losses`, transition to SellOnly.
- If `realized_pnl < min_pnl_per_pair_usd`, transition to SellOnly.

These thresholds are deliberately conservative.  A single losing trade is
normal.  Five consecutive losses on a low-liquidity pair means something is
structurally wrong (likely adverse selection from informed flow).

### 3.4 Time Since Last Fill

**Rationale**: If a pair hasn't had a fill in hours, it's consuming risk budget
and book subscription resources for nothing.

| Parameter | Default | Description |
|-----------|---------|-------------|
| `max_idle_minutes` | `120` | Flag pair if no fill in this many minutes |

**Action**: If `now - last_fill_time > max_idle_minutes` AND the pair is
Active (has been quoting), log a warning.  This is informational only by
default -- it does not auto-disable because the pair may simply be waiting for a
fill on a legitimately wide spread.  The volume floor check is the primary
mechanism for detecting dead pairs.

If `max_idle_minutes` is set to 0, this check is disabled entirely.

---

## 4. State Machine: Pair Lifecycle

```
                   +----------+
         +---------| Scanning |<------ Initial startup
         |         +----------+        (or re-evaluation after cooldown)
         |              |
         |  Passes all  |
         |  criteria    |
         v              v
    +---------+    +-----------+
    | Cautious|    |  Active   |<----- Normal quoting (bid + ask)
    +---------+    +-----------+
         |              |
  After N fills         | Fails evaluation
  without loss          | (volume, spread, or profitability)
         |              v
         +-------> +-----------+
                   | SellOnly  |<----- Sell existing inventory, no new buys
                   +-----------+
                        |
              Position  | fully sold
              (or below | exchange minimums)
                        v
                   +-----------+
                   | Disabled  |<----- No quoting, no subscriptions*
                   +-----------+
                        |
              Cooldown  | expires AND
              ticker    | shows improvement
                        v
                   +----------+
                   | Scanning | (re-evaluation)
                   +----------+
```

* Note: Book subscriptions are managed at the WebSocket level.  In the initial
  implementation, disabled pairs keep their WS subscriptions (the engine simply
  ignores their book updates via the `disabled_pairs` check in `maybe_quote()`).
  Unsubscribing/resubscribing to WS channels mid-session is a future
  optimization -- not worth the complexity for v1.

### State Definitions

**Scanning**: Pair has been identified by the scanner but not yet validated at
runtime.  This is the initial state for all pairs at startup.  The first ticker
refresh transitions them to Active or Disabled.

**Active**: Normal two-sided quoting.  Bid and ask orders are placed per the
existing quoter logic.

**Cautious**: Re-enabled after being disabled.  Only buy-side orders are placed,
at 50% of normal `order_size_usd`.  After `cautious_fill_count` (default 3)
profitable fills, transitions to Active.  If any fill is a loss, transitions
back to SellOnly.

**SellOnly**: One-sided quoting.  Only ask orders are placed to unwind existing
inventory.  This reuses the existing `sell_only_pairs` HashSet in the engine.
No new buys.  Transitions to Disabled once position is empty (or below exchange
minimums).

**Disabled**: No quoting at all.  This reuses the existing `disabled_pairs`
HashSet.  A cooldown timer prevents re-evaluation for `disable_cooldown_minutes`
(default 60).  After cooldown, the next ticker refresh checks if conditions have
improved.

### Transition Rules (Summary)

| From | To | Trigger |
|------|----|---------|
| Scanning | Active | Passes volume, spread, and no-restriction checks |
| Scanning | Disabled | Fails volume or spread check |
| Active | SellOnly | Volume below floor (2 consecutive), spread below breakeven (3 consecutive), or profitability threshold breached |
| Active | SellOnly | API `Pause` command (existing, unchanged) |
| Active | Disabled | Order rejected with "restricted" (existing, unchanged) |
| SellOnly | Disabled | Position fully unwound (qty == 0 or below exchange minimums) |
| Disabled | Scanning | Cooldown expired |
| Scanning | Cautious | Previously disabled pair passes re-evaluation |
| Cautious | Active | N profitable fills without a loss |
| Cautious | SellOnly | Any losing fill |

### Anti-Flapping

To prevent a pair from oscillating between Active and Disabled:

- **Disable cooldown**: After transitioning to Disabled, the pair cannot be
  re-evaluated for `disable_cooldown_minutes` (default 60 minutes).
- **Strike counter**: Each time a pair transitions from Active to SellOnly, a
  strike counter increments.  After `max_strikes` (default 3) strikes in one
  session, the pair is permanently disabled for the remainder of the session.
  This prevents a pair from wasting the entire session cycling.
- **Hysteresis on volume**: Re-enabling requires volume to exceed
  `min_volume_24h_usd * 1.5` (50% above the disable threshold), not just the
  bare minimum.

---

## 5. Config Schema Additions

New section in `Config`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairEvalConfig {
    /// Enable/disable the dynamic pair evaluator entirely.
    /// When false, all pairs stay active for the session (current behavior).
    pub enabled: bool,

    /// How often to re-evaluate pairs, in seconds.
    pub eval_interval_secs: u64,

    /// Minimum 24h USD volume to keep a pair active.
    pub min_volume_24h_usd: Decimal,

    /// Number of consecutive eval cycles below volume floor before disabling.
    pub volume_fail_cycles: u32,

    /// Number of consecutive eval cycles with spread below breakeven before disabling.
    pub spread_fail_cycles: u32,

    /// Maximum consecutive losing round-trips before flagging.
    pub max_consecutive_losses: u32,

    /// Minimum cumulative PnL (USD) per pair before flagging.
    /// Negative value = allow losses up to this amount.
    pub min_pnl_per_pair_usd: Decimal,

    /// Minimum number of trades before profitability is evaluated.
    pub profitability_eval_min_trades: u64,

    /// Minutes with no fill before logging a warning (0 = disabled).
    pub max_idle_minutes: u64,

    /// Cooldown period (minutes) after disabling before re-evaluation.
    pub disable_cooldown_minutes: u64,

    /// Number of profitable fills required in Cautious state before promoting to Active.
    pub cautious_fill_count: u32,

    /// Order size multiplier in Cautious state (e.g., 0.5 = half size).
    pub cautious_size_multiplier: Decimal,

    /// Maximum times a pair can cycle Active -> SellOnly in one session.
    pub max_strikes: u32,
}
```

Default values:

```rust
impl Default for PairEvalConfig {
    fn default() -> Self {
        Self {
            enabled: false,  // opt-in, does not change existing behavior
            eval_interval_secs: 600,  // 10 minutes
            min_volume_24h_usd: dec!(500),
            volume_fail_cycles: 2,
            spread_fail_cycles: 3,
            max_consecutive_losses: 5,
            min_pnl_per_pair_usd: dec!(-10),
            profitability_eval_min_trades: 3,
            max_idle_minutes: 120,
            disable_cooldown_minutes: 60,
            cautious_fill_count: 3,
            cautious_size_multiplier: dec!(0.5),
            max_strikes: 3,
        }
    }
}
```

Added to `Config` as:

```rust
pub struct Config {
    pub exchange: ExchangeConfig,
    pub trading: TradingConfig,
    pub risk: RiskConfig,
    pub persistence: PersistenceConfig,
    pub pair_eval: PairEvalConfig,  // NEW
}
```

---

## 6. Implementation Plan

Ordered list of code changes.  Each step is independently testable.

### Step 1: Add PairMetrics and PairState to the Engine

**Files**:
- `/workarea/kraken_mm_rs/crates/core/src/engine/pair_eval.rs` (new)
- `/workarea/kraken_mm_rs/crates/core/src/engine/mod.rs` (add `pub mod pair_eval;`)

**What**: Define `PairState` enum, `PairMetrics` struct, and
`PairEvaluator` struct with the evaluation logic as pure functions.  The
evaluator takes `&PairMetrics`, `&TickerData`, and `&PairEvalConfig` and returns
a `PairDecision` (keep, transition to SellOnly, transition to Disabled, etc.).

**Tests**: Unit tests for each transition rule.  Feed synthetic metrics and
assert the correct decision.

### Step 2: Add PairEvalConfig to Config

**Files**:
- `/workarea/kraken_mm_rs/crates/core/src/config.rs`

**What**: Add `PairEvalConfig` struct and include it in `Config`.  Set
`enabled: false` as default so the feature is opt-in.

**Tests**: Verify `Config::default()` round-trips through serde correctly.

### Step 3: Add New Event and Command Variants

**Files**:
- `/workarea/kraken_mm_rs/crates/core/src/types/event.rs` -- add `TickerRefresh` variant
- `/workarea/kraken_mm_rs/crates/core/src/types/command.rs` -- add `FetchTickers` variant

**What**:

```rust
// event.rs
EngineEvent::TickerRefresh {
    tickers: HashMap<String, TickerData>,
    timestamp: DateTime<Utc>,
}

// command.rs
EngineCommand::FetchTickers
```

**Tests**: Compilation only (enum variants).

### Step 4: Wire Per-Pair Metrics Accumulation in the Engine

**Files**:
- `/workarea/kraken_mm_rs/crates/core/src/engine/core.rs`

**What**:
- Add `pair_metrics: HashMap<String, PairMetrics>` field to `Engine`.
- In `Engine::new()`, initialize `PairMetrics` for each pair in `pair_info` with
  state = `Scanning`.
- In `on_fill()`, update the per-pair `PairMetrics`: increment trade_count,
  update realized_pnl, update consecutive_losses, update last_fill_time.
- This step does NOT add evaluation logic -- just the data collection.

**Tests**: Existing engine tests continue to pass.  Add a test that verifies
`pair_metrics` are updated after a fill.

### Step 5: Implement the Evaluation Timer and FetchTickers Command

**Files**:
- `/workarea/kraken_mm_rs/crates/core/src/engine/core.rs` (in `on_tick()`)

**What**:
- Add `last_eval_time: Option<DateTime<Utc>>` field to `Engine`.
- In `on_tick()`, after the existing stale order check and DMS refresh:
  - If `config.pair_eval.enabled` AND enough time has elapsed since
    `last_eval_time`, emit `EngineCommand::FetchTickers`.
  - Update `last_eval_time`.

**Tests**: Verify FetchTickers is emitted after the configured interval.

### Step 6: Handle TickerRefresh Event -- Run Evaluations

**Files**:
- `/workarea/kraken_mm_rs/crates/core/src/engine/core.rs` (add `on_ticker_refresh()`)

**What**:
- Add `EngineEvent::TickerRefresh` to the match in `handle_event()`.
- Implement `on_ticker_refresh()`:
  - For each pair, update `pair_metrics` with fresh volume and spread data.
  - Call `PairEvaluator::evaluate()` for each pair.
  - Apply transitions:
    - Active -> SellOnly: `self.sell_only_pairs.insert(symbol)`.
    - SellOnly -> Disabled: if `state.position(symbol).is_empty()`,
      `self.disabled_pairs.insert(symbol)` and
      `self.sell_only_pairs.remove(symbol)`.
    - Disabled -> Scanning -> Cautious: if cooldown expired and ticker shows
      improvement, remove from `disabled_pairs`, add to a new
      `cautious_pairs: HashSet<String>`.
  - Log each transition at `INFO` level with the reason.

The Cautious state needs a new field: `cautious_pairs: HashSet<String>`.  In
`place_fresh_quotes()`, if the pair is in `cautious_pairs`:
- Only place buy orders (no asks) -- the opposite of SellOnly.
- Use `order_size_usd * cautious_size_multiplier` instead of full size.
- After `cautious_fill_count` profitable fills, remove from `cautious_pairs`.

**Tests**:
- Feed a `TickerRefresh` with low volume for a pair, verify it transitions to
  SellOnly.
- Feed a `TickerRefresh` with improved volume for a disabled pair past cooldown,
  verify it transitions to Cautious.
- Verify anti-flapping: after `max_strikes`, pair stays disabled.

### Step 7: Wire FetchTickers in the Command Dispatcher

**Files**:
- `/workarea/kraken_mm_rs/crates/bot/src/main.rs` (both `run_dry` and `run_live`)

**What**: Add a match arm for `EngineCommand::FetchTickers`:
- Call `exchange.get_tickers(&pair_info)`.
- Send `EngineEvent::TickerRefresh { tickers, timestamp }` back through
  `event_tx`.
- On failure, log a warning and skip (same pattern as the existing startup
  ticker fetch).

The command dispatcher needs access to `pair_info` and `event_tx` (both are
already in scope in `run_live` and `run_dry`).  For `run_dry`, the ticker data
would come from the real Kraken public API (no auth needed), which is the
correct behavior even in dry-run mode.

**Implementation detail**: The dispatcher should clone the `event_tx` for
sending back the response.  This is already the pattern used by the ticker
handle and book handle spawns.

**Tests**: Integration test with a mock exchange client that returns
predetermined ticker data.

### Step 8: SellOnly -> Disabled Auto-Transition on Empty Position

**Files**:
- `/workarea/kraken_mm_rs/crates/core/src/engine/core.rs` (in `on_fill()` or `on_tick()`)

**What**: After a sell fill fully unwinds a position for a pair in SellOnly
state (due to the evaluator, not liquidation), transition that pair to Disabled
state:
- Remove from `sell_only_pairs`.
- Add to `disabled_pairs`.
- Set `cooldown_until` in `PairMetrics`.
- Log the transition.

This check already partially exists: in `on_fill()`, when
`pos.is_empty()` after a sell, the pending_liquidation path handles it.  The new
code adds a parallel check for evaluator-driven SellOnly pairs.

**Tests**: Simulate a fill that empties a SellOnly pair's position, verify it
moves to Disabled.

### Step 9: Add Pair Evaluation Status to the REST API

**Files**:
- `/workarea/kraken_mm_rs/crates/core/src/api/server.rs`
- `/workarea/kraken_mm_rs/crates/core/src/api/shared.rs`

**What**: Add a `GET /pairs` endpoint that returns the current state of each
pair:
- Symbol, PairState, volume, last fill time, per-pair PnL, trade count, strikes.
- This is informational -- allows the operator to monitor the evaluator's
  decisions.

Optionally add `POST /pairs/{symbol}/enable` and `POST /pairs/{symbol}/disable`
for manual overrides (similar to the existing `Pause`/`Resume` API actions but
per-pair).

**Tests**: HTTP handler tests with mock state.

### Step 10: Logging and Observability

**Files**:
- `/workarea/kraken_mm_rs/crates/core/src/engine/core.rs` (in tick status log)

**What**: Extend the periodic tick status log (line 666-681 of `core.rs`) to
include pair evaluator summary:
- Count of Active, SellOnly, Disabled, Cautious pairs.
- Any pairs that transitioned since last tick.

This requires no new infrastructure -- just extending the existing
`tracing::info!` call.

---

## 7. Risk Considerations

### 7.1 Rate Limits

**Risk**: The ticker fetch adds one public REST call per evaluation cycle.

**Mitigation**: At the default 10-minute interval, this is 6 calls/hour -- well
within Kraken's public API limit.  The call is batched (all pairs in one
request, same as the existing startup call).  If the call fails, the evaluator
skips that cycle and tries again at the next interval.  No retry loop.

The evaluator does NOT add any private API calls (no new orders, cancels, or
balance queries beyond what the existing engine already does).  The only
additional order activity comes from SellOnly transitions, which use the
existing quoter path.

### 7.2 Race Conditions with Open Orders

**Risk**: When transitioning a pair from Active to SellOnly, the pair might have
open buy and sell orders.

**Mitigation**: The transition does NOT cancel existing orders.  It sets
`sell_only_pairs.insert(symbol)`, which prevents new buy orders from being
placed.  Existing buy orders will either:
- Fill naturally (the position is then unwound by the SellOnly quoting).
- Expire via the existing stale order check (default 300 seconds).
- Get cancelled on the next requote cycle when the quoter re-evaluates.

This is the same behavior as the existing downtrend filter and API `Pause`
command.  No special handling needed.

**Exception**: If the evaluator decides to Disable (not just SellOnly) a pair
that still has inventory, it MUST go through SellOnly first.  The state machine
enforces this: there is no direct Active -> Disabled transition (except for
"restricted" rejections, which are permanent and handled separately).

### 7.3 Concurrent Evaluation and Fill Processing

**Risk**: A fill arrives between FetchTickers command and TickerRefresh response,
changing the pair's metrics.

**Mitigation**: This is safe because both events flow through the same
single-threaded `Engine::handle_event()` loop.  The engine processes events
sequentially.  A fill that arrives while the ticker fetch is in-flight will be
processed before the `TickerRefresh` event, so the evaluator sees up-to-date
metrics.  This is the same serialization guarantee the existing engine relies on
for all event processing.

### 7.4 Startup Behavior

**Risk**: On startup, all pairs are in Scanning state.  If the initial ticker
fetch fails, pairs remain in Scanning (which behaves identically to Active for
quoting purposes, maintaining backward compatibility).

**Mitigation**: The first evaluation runs after `eval_interval_secs`.  If the
feature is enabled, pairs are effectively unfiltered for the first 10 minutes.
This is acceptable because:
- The existing startup downtrend filter still runs (independent of the
  evaluator).
- The existing per-quote spread check still runs.
- 10 minutes of unfiltered quoting is bounded risk.

If faster startup filtering is desired, the evaluator can be triggered
immediately after the initial book snapshots are received (set
`last_eval_time = None` in the constructor).

### 7.5 State Persistence

**Risk**: If the bot restarts, all pair metrics are lost.

**Mitigation**: For v1, pair metrics are ephemeral (session-scoped).  After a
restart, all pairs start in Scanning state and are re-evaluated from scratch
within one evaluation cycle.  This is acceptable because:
- The ticker data (volume, spread) is real-time and doesn't need history.
- Per-pair PnL is cumulative from the current session, which resets on restart.
  This is actually desirable -- a pair that was unprofitable yesterday might be
  profitable today.

If persistence is later desired, `PairMetrics` can be added to
`BotState` (it already serializes via serde).

### 7.6 Interaction with Existing Disable/SellOnly Mechanisms

The engine already has three ways to disable or restrict a pair:
1. **`disabled_pairs`**: Set on "restricted" order rejection.
2. **`sell_only_pairs`**: Set by the startup downtrend filter or API Pause.
3. **`pending_liquidation`**: Set by stop-loss/take-profit triggers.

The evaluator introduces a fourth mechanism.  These must not conflict:
- If a pair is in `pending_liquidation`, the evaluator skips it entirely (let
  the liquidation logic handle it).
- If a pair was disabled due to a "restricted" rejection, the evaluator never
  re-enables it (that's a permanent Kraken-side restriction).
- The `PairMetrics.disable_reason` field distinguishes between evaluator-driven
  disables (can be re-enabled) and restriction-driven disables (permanent).

### 7.7 Performance

The evaluator iterates over all pairs once per evaluation cycle.  With a typical
pair count of 5-20, this is trivially fast (microseconds).  No allocations are
needed beyond updating the `PairMetrics` struct in-place.

The `TickerRefresh` event carries a `HashMap<String, TickerData>` which is
already allocated by the exchange client.  Moving it into the engine is zero-copy
via the event channel.
