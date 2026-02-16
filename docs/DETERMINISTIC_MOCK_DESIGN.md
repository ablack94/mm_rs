# Deterministic Mock Exchange Design

## 1. Overview

The current mock exchange at `crates/mock-exchange/` simulates price movement
using a random walk driven by `volatility_pct` in `orderbook.rs:random_walk()`.
While seedable via `MOCK_SEED`, the random walk produces stochastic paths that
make it impossible to test specific market conditions (crashes, spread widening,
flash spikes) in a repeatable, inspectable way.

This design adds a **deterministic scenario mode** that replays predefined price
trajectories from a JSON file. The mode is activated by a single env var
(`MOCK_SCENARIO_FILE`). When set, the mock ignores `MOCK_VOLATILITY` for
scenario-driven pairs and instead drives prices from the scenario file's
waypoint sequences.

### Current Architecture Summary

- `config.rs` -- `MockConfig` parsed from env vars (`MOCK_PORT`, `MOCK_PAIRS`,
  `MOCK_SPREAD_PCT`, `MOCK_VOLATILITY`, `MOCK_FILL_PROBABILITY`,
  `MOCK_UPDATE_INTERVAL_MS`, `MOCK_SEED`, `MOCK_STARTING_USD`)
- `orderbook.rs` -- `OrderBook` with `BTreeMap` bids/asks, `random_walk()`
  modifies `target_mid` then calls `regenerate_levels()`
- `simulation.rs` -- `run_simulation()` ticks at `update_interval_ms`, calls
  `do_book_tick()` which random-walks each book, broadcasts snapshots, checks
  resting orders for fills (deterministic crossing + probabilistic near-spread)
- `state.rs` -- `ExchangeState` holds books, orders, balances, config
- `main.rs` -- Axum server with REST + WS endpoints, spawns simulation loop

---

## 2. Scenario File Format

### 2.1 Top-Level Structure

```json
{
  "name": "crash_recovery",
  "description": "Sharp crash to -40%, then slow recovery over 60s",
  "defaults": {
    "spread_pct": 5.0,
    "fill_probability": 0.3,
    "interpolation": "linear"
  },
  "pairs": {
    "OMG/USD": {
      "starting_price": 0.50,
      "waypoints": [
        { "t_ms": 0,     "mid": 0.50 },
        { "t_ms": 5000,  "mid": 0.50 },
        { "t_ms": 8000,  "mid": 0.30, "spread_pct": 12.0, "fill_probability": 0.05 },
        { "t_ms": 20000, "mid": 0.32 },
        { "t_ms": 60000, "mid": 0.48, "spread_pct": 5.0, "fill_probability": 0.3 }
      ]
    },
    "CAMP/USD": {
      "starting_price": 0.004,
      "waypoints": [
        { "t_ms": 0,     "mid": 0.004 },
        { "t_ms": 60000, "mid": 0.004 }
      ]
    }
  },
  "end_behavior": "hold_last"
}
```

### 2.2 Field Definitions

**Top level:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | yes | Human-readable scenario name, logged at startup |
| `description` | string | no | What this scenario tests |
| `defaults` | object | no | Default `spread_pct`, `fill_probability`, `interpolation` applied to all segments unless overridden per-waypoint |
| `pairs` | object | yes | Map of wsname to pair scenario. Pairs listed here use deterministic pricing; pairs in `MOCK_PAIRS` but absent from this map continue using random walk |
| `end_behavior` | string | no | What happens after the last waypoint: `"hold_last"` (default) freezes at final price, `"loop"` restarts from the first waypoint, `"stop"` halts book updates for that pair |

**Per-pair:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `starting_price` | f64 | yes | Must match the first waypoint's `mid`. Used to initialize the `OrderBook`. Overrides the value from `MOCK_PAIRS` for this symbol. |
| `waypoints` | array | yes | Ordered list of (time_offset, mid_price) points. At least 2 entries required. First entry must have `t_ms: 0`. |

**Per-waypoint:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `t_ms` | u64 | yes | Milliseconds since scenario start. Must be strictly monotonically increasing. |
| `mid` | f64 | yes | Target mid price at this point in time. |
| `spread_pct` | f64 | no | Override spread for the segment **starting** at this waypoint. Inherits from `defaults.spread_pct` or global `MOCK_SPREAD_PCT` if absent. |
| `fill_probability` | f64 | no | Override fill probability for the segment starting at this waypoint. Inherits similarly. |
| `interpolation` | string | no | `"linear"` (default) or `"step"`. Controls how mid is computed between this waypoint and the next. |

### 2.3 Interpolation Behavior

Given two consecutive waypoints `W[i]` and `W[i+1]`, at simulation time `t`
where `W[i].t_ms <= t < W[i+1].t_ms`:

- **Linear** (default):
  `mid(t) = W[i].mid + (W[i+1].mid - W[i].mid) * (t - W[i].t_ms) / (W[i+1].t_ms - W[i].t_ms)`
- **Step**:
  `mid(t) = W[i].mid` (holds flat, then jumps instantaneously at `W[i+1].t_ms`)

Spread and fill probability for a segment are determined by `W[i]`'s overrides
(the starting waypoint of the segment). They apply for the entire interval
`[W[i].t_ms, W[i+1].t_ms)`.

### 2.4 Validation Rules (enforced at load time)

1. Each pair must have at least 2 waypoints.
2. The first waypoint must have `t_ms: 0`.
3. `t_ms` values must be strictly monotonically increasing.
4. `starting_price` must equal the first waypoint's `mid`.
5. All `mid` values must be positive.
6. `spread_pct` must be positive when specified.
7. `fill_probability` must be in `[0.0, 1.0]` when specified.

---

## 3. Code Changes

### 3.1 New File: `crates/mock-exchange/src/scenario.rs`

This module owns all scenario-related types and logic.

**Types:**

```rust
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct ScenarioFile {
    pub name: String,
    pub description: Option<String>,
    pub defaults: Option<ScenarioDefaults>,
    pub pairs: HashMap<String, PairScenario>,
    pub end_behavior: Option<EndBehavior>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ScenarioDefaults {
    pub spread_pct: Option<f64>,
    pub fill_probability: Option<f64>,
    pub interpolation: Option<Interpolation>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PairScenario {
    pub starting_price: f64,
    pub waypoints: Vec<Waypoint>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Waypoint {
    pub t_ms: u64,
    pub mid: f64,
    pub spread_pct: Option<f64>,
    pub fill_probability: Option<f64>,
    pub interpolation: Option<Interpolation>,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Interpolation {
    #[default]
    Linear,
    Step,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EndBehavior {
    #[default]
    HoldLast,
    Loop,
    Stop,
}
```

**Return type for sampling:**

```rust
pub struct ScenarioSample {
    pub mid: f64,
    pub spread_pct: Option<f64>,
    pub fill_probability: Option<f64>,
    pub finished: bool,
}
```

**Key methods:**

```rust
impl ScenarioFile {
    pub fn load(path: &str) -> Result<Self, anyhow::Error> {
        let contents = std::fs::read_to_string(path)?;
        let scenario: ScenarioFile = serde_json::from_str(&contents)?;
        scenario.validate()?;
        Ok(scenario)
    }

    fn validate(&self) -> Result<(), anyhow::Error> {
        for (symbol, ps) in &self.pairs {
            anyhow::ensure!(ps.waypoints.len() >= 2,
                "{symbol}: need at least 2 waypoints");
            anyhow::ensure!(ps.waypoints[0].t_ms == 0,
                "{symbol}: first waypoint must have t_ms=0");
            for w in ps.waypoints.windows(2) {
                anyhow::ensure!(w[1].t_ms > w[0].t_ms,
                    "{symbol}: waypoints must be strictly monotonically increasing");
            }
            anyhow::ensure!(
                (ps.starting_price - ps.waypoints[0].mid).abs() < 1e-10,
                "{symbol}: starting_price must match first waypoint mid"
            );
        }
        Ok(())
    }
}

impl PairScenario {
    /// Given elapsed milliseconds since scenario start, compute the
    /// current mid price and any per-segment overrides.
    pub fn sample_at(
        &self,
        elapsed_ms: u64,
        defaults: &ScenarioDefaults,
        end_behavior: EndBehavior,
    ) -> ScenarioSample {
        let wps = &self.waypoints;
        let last = &wps[wps.len() - 1];

        // Past the end of the scenario
        if elapsed_ms >= last.t_ms {
            match end_behavior {
                EndBehavior::HoldLast => {
                    return ScenarioSample {
                        mid: last.mid,
                        spread_pct: last.spread_pct.or(defaults.spread_pct),
                        fill_probability: last.fill_probability
                            .or(defaults.fill_probability),
                        finished: false,
                    };
                }
                EndBehavior::Stop => {
                    return ScenarioSample {
                        mid: last.mid,
                        spread_pct: last.spread_pct.or(defaults.spread_pct),
                        fill_probability: last.fill_probability
                            .or(defaults.fill_probability),
                        finished: true,
                    };
                }
                EndBehavior::Loop => {
                    let total_duration = last.t_ms;
                    let looped_ms = elapsed_ms % total_duration;
                    return self.sample_at(looped_ms, defaults, EndBehavior::HoldLast);
                }
            }
        }

        // Find the active segment: W[i] and W[i+1] such that
        // W[i].t_ms <= elapsed_ms < W[i+1].t_ms
        let mut idx = 0;
        for i in 0..wps.len() - 1 {
            if elapsed_ms >= wps[i].t_ms && elapsed_ms < wps[i + 1].t_ms {
                idx = i;
                break;
            }
        }

        let w0 = &wps[idx];
        let w1 = &wps[idx + 1];

        let interp = w0.interpolation
            .or(defaults.interpolation)
            .unwrap_or_default();

        let mid = match interp {
            Interpolation::Linear => {
                let frac = (elapsed_ms - w0.t_ms) as f64
                    / (w1.t_ms - w0.t_ms) as f64;
                w0.mid + (w1.mid - w0.mid) * frac
            }
            Interpolation::Step => w0.mid,
        };

        ScenarioSample {
            mid,
            spread_pct: w0.spread_pct.or(defaults.spread_pct),
            fill_probability: w0.fill_probability.or(defaults.fill_probability),
            finished: false,
        }
    }
}
```

### 3.2 Changes to `config.rs`

Add one field to `MockConfig`:

```rust
pub struct MockConfig {
    // ... existing fields unchanged ...
    pub scenario: Option<ScenarioFile>,  // NEW
}
```

In `MockConfig::from_env()`, after the existing parsing, add:

```rust
let scenario = std::env::var("MOCK_SCENARIO_FILE")
    .ok()
    .map(|path| ScenarioFile::load(&path)
        .expect("Failed to load scenario file"));

// Merge scenario pairs into the pairs list
if let Some(ref sc) = scenario {
    for (symbol, ps) in &sc.pairs {
        if let Some(existing) = pairs.iter_mut().find(|p| p.symbol == *symbol) {
            existing.starting_price = ps.starting_price;
        } else {
            pairs.push(PairConfig {
                symbol: symbol.clone(),
                starting_price: ps.starting_price,
            });
        }
    }
}
```

And include `scenario` in the returned `MockConfig` struct.

### 3.3 Changes to `orderbook.rs`

Add one method to `OrderBook`. The existing `random_walk` method is unchanged.

```rust
impl OrderBook {
    // ... existing methods unchanged ...

    /// Set the mid price to a specific value and regenerate levels.
    /// Used by the deterministic scenario driver.
    pub fn set_mid(&mut self, mid: f64, spread_pct_override: Option<f64>) {
        self.target_mid = mid.max(0.0000001);
        if let Some(sp) = spread_pct_override {
            self.spread_pct = sp;
        }
        self.regenerate_levels();
    }
}
```

This mirrors `random_walk` but replaces the stochastic delta with a direct
assignment. The `regenerate_levels()` call produces a fresh book from the new
`target_mid`, exactly as the random walk does.

### 3.4 Changes to `simulation.rs`

This is the primary integration point. Two functions change.

**`run_simulation`** -- add scenario state and pass it to `do_book_tick`:

```rust
pub async fn run_simulation(
    state: Arc<RwLock<ExchangeState>>,
    book_tx: broadcast::Sender<String>,
    exec_tx: broadcast::Sender<String>,
) {
    let (interval_ms, volatility, fill_prob, seed, scenario) = {
        let st = state.read().await;
        (
            st.config.update_interval_ms,
            st.config.volatility,
            st.config.fill_probability,
            st.config.seed,
            st.config.scenario.clone(),  // NEW
        )
    };

    let mut rng: StdRng = match seed {
        Some(s) => StdRng::seed_from_u64(s),
        None => StdRng::from_entropy(),
    };

    let scenario_start = std::time::Instant::now();  // NEW

    let mut tick_interval = tokio::time::interval(Duration::from_millis(interval_ms));
    let mut heartbeat_interval = tokio::time::interval(Duration::from_secs(5));

    loop {
        tokio::select! {
            _ = tick_interval.tick() => {
                let elapsed_ms = scenario_start.elapsed().as_millis() as u64;  // NEW
                do_book_tick(
                    &state, &book_tx, &exec_tx,
                    volatility, fill_prob, &mut rng,
                    scenario.as_ref(), elapsed_ms,  // NEW
                ).await;
            }
            _ = heartbeat_interval.tick() => {
                let hb = serde_json::to_string(&messages::heartbeat()).unwrap();
                let _ = book_tx.send(hb.clone());
                let _ = exec_tx.send(hb);
            }
        }
    }
}
```

**`do_book_tick`** -- branch between random walk and scenario for each pair:

The current lines 63-67:
```rust
if let Some(book) = st.books.get_mut(symbol) {
    book.random_walk(volatility, rng);
```

Become:
```rust
if let Some(book) = st.books.get_mut(symbol) {
    let effective_fill_prob = if let Some(sc) = scenario {
        if let Some(pair_scenario) = sc.pairs.get(symbol) {
            // Deterministic mode for this pair
            let defaults = sc.defaults.clone().unwrap_or_default();
            let end = sc.end_behavior.unwrap_or_default();
            let sample = pair_scenario.sample_at(elapsed_ms, &defaults, end);

            if !sample.finished {
                book.set_mid(sample.mid, sample.spread_pct);
            }
            // Use per-segment fill probability, fall back to global
            sample.fill_probability.unwrap_or(fill_probability)
        } else {
            // Pair not in scenario -- random walk
            book.random_walk(volatility, rng);
            fill_probability
        }
    } else {
        // No scenario loaded -- random walk (original behavior)
        book.random_walk(volatility, rng);
        fill_probability
    };
```

And replace `fill_probability` with `effective_fill_prob` in the probabilistic
fill check at line 141:

```rust
.filter(|_| rng.gen::<f64>() < effective_fill_prob)
```

The rest of `do_book_tick` (book snapshot broadcast, deterministic fill
checking, balance updates, exec reports) remains completely unchanged.

### 3.5 Changes to `main.rs`

Add one line to the module declarations:

```rust
mod scenario;  // NEW -- add alongside existing mod declarations
```

No other changes. The scenario is loaded inside `MockConfig::from_env()` which
is already called in `main()`.

### 3.6 Cargo.toml

No new dependencies. `serde` (with `derive`), `serde_json`, and `anyhow` are
already present.

### 3.7 Summary of Changed Files

| File | Change |
|------|--------|
| `src/scenario.rs` | **NEW** -- types, loading, validation, sampling logic |
| `src/main.rs` | Add `mod scenario;` declaration |
| `src/config.rs` | Add `scenario: Option<ScenarioFile>` field, load from env, merge pairs |
| `src/orderbook.rs` | Add `set_mid()` method (3 lines + doc comment) |
| `src/simulation.rs` | Branch random_walk vs set_mid in `do_book_tick`, pass elapsed_ms from `run_simulation` |

---

## 4. Integration with Existing Simulation Loop

The design preserves the existing tick-based architecture. The simulation loop's
structure does not change -- it still ticks at `MOCK_UPDATE_INTERVAL_MS`
intervals, still calls `do_book_tick`, still checks fills, still broadcasts book
snapshots. The only difference is **how the mid price is determined** at each tick.

```
                     +---------------------------+
                     |     run_simulation loop    |
                     |  tick every N ms           |
                     +-------------+-------------+
                                   |
                           do_book_tick()
                                   |
                     +-------------+-------------+
                     |                           |
              scenario.is_some()?         No scenario
                     |                           |
            +--------+--------+          random_walk()
            |                 |
      pair in scenario?   pair NOT in
            |              scenario
      sample_at(elapsed)       |
      book.set_mid()     random_walk()
            |
    +-------+-------+
    |               |
 broadcast      check fills
 book snapshot  (unchanged logic,
                 uses effective_fill_prob
                 from scenario segment)
```

**Mixed mode.** If `MOCK_PAIRS=OMG/USD:0.50,CAMP/USD:0.004` and the scenario
only defines `OMG/USD`, then OMG/USD follows the deterministic trajectory while
CAMP/USD continues random walking. Both are served from the same simulation
loop and share the same tick cadence.

**Fill determinism.** With a fixed `MOCK_SEED` and a deterministic scenario,
fill outcomes are fully deterministic because:

1. The RNG is seeded identically each run.
2. The price path is identical each run.
3. The number of `rng.gen::<f64>()` calls per tick is determined by the number
   of resting orders, which is determined by the deterministic price path.

The only source of non-determinism is wall-clock timing jitter in the tick
interval, which can cause a slightly different `elapsed_ms` at each tick. For
CI/test use, this is negligible since `sample_at` produces smoothly
interpolated values. See Section 8 for a future virtual-time enhancement that
eliminates this entirely.

**Book-level quantities.** The `regenerate_levels()` method (called by both
`random_walk` and `set_mid`) uses `rand::thread_rng()` for level quantities.
This is intentional: level quantities are cosmetic (the bot's quoting logic
depends only on best bid/ask prices, not depth quantities). If quantity
determinism is needed, `regenerate_levels` could be changed to accept the
seeded RNG, but this is out of scope.

---

## 5. Example Scenarios

All scenario files are stored in `test_data/scenarios/`.

### 5.1 `crash_recovery.json` -- Sharp Crash and Recovery

**Tests:** Stop-loss trigger, inventory behavior during crash, requoting after
rapid price movement, spread widening during stress.

```json
{
  "name": "crash_recovery",
  "description": "40% crash over 3s, sideways 10s, slow 60s recovery to 96% of start",
  "defaults": {
    "spread_pct": 5.0,
    "fill_probability": 0.3
  },
  "pairs": {
    "OMG/USD": {
      "starting_price": 0.50,
      "waypoints": [
        { "t_ms": 0,     "mid": 0.50 },
        { "t_ms": 5000,  "mid": 0.50 },
        { "t_ms": 8000,  "mid": 0.30, "spread_pct": 15.0, "fill_probability": 0.05 },
        { "t_ms": 18000, "mid": 0.30, "spread_pct": 10.0 },
        { "t_ms": 78000, "mid": 0.48, "spread_pct": 5.0, "fill_probability": 0.3 }
      ]
    }
  },
  "end_behavior": "hold_last"
}
```

**Price trajectory:**
- t=0-5s: Flat at 0.50 (normal market)
- t=5-8s: Linear crash from 0.50 to 0.30 (-40% in 3s)
- t=8-18s: Flat at 0.30 (bottom, wide spread, low fills)
- t=18-78s: Linear recovery from 0.30 to 0.48

**Expected bot behavior:** During the crash (t=5s-8s), mid drops from 0.50 to
0.30. Spread widens to 15% and fill probability drops to 5%, simulating thin
liquidity. If the bot holds inventory bought at 0.50, the 3% stop-loss should
trigger when price hits ~0.485. After the crash, spread narrows gradually and
fills resume. Resting buy orders from before the crash may get filled as price
plunges through them.

### 5.2 `gradual_uptrend.json` -- Steady Climb

**Tests:** Take-profit trigger, cost-floor ask pricing, inventory accumulation.

```json
{
  "name": "gradual_uptrend",
  "description": "Steady 20% rise over 120s",
  "defaults": {
    "spread_pct": 5.0,
    "fill_probability": 0.5
  },
  "pairs": {
    "OMG/USD": {
      "starting_price": 0.50,
      "waypoints": [
        { "t_ms": 0,      "mid": 0.50 },
        { "t_ms": 120000, "mid": 0.60 }
      ]
    }
  },
  "end_behavior": "hold_last"
}
```

**Price trajectory:** Smooth linear rise from 0.50 to 0.60 over 2 minutes
(~0.083%/s).

**Expected bot behavior:** Fill probability is high (50%), so both bids and asks
should fill regularly. As price rises, bids fill (accumulating inventory) and
the cost-floor logic keeps asks profitable. Take-profit at 10% should trigger
if inventory was accumulated below 0.545 and price reaches 0.60.

### 5.3 `sideways_choppy.json` -- Range-Bound Oscillation

**Tests:** Steady-state market making, bid-ask fill balance, P&L accumulation
from spread capture. This is the ideal MM environment.

```json
{
  "name": "sideways_choppy",
  "description": "Oscillate between 0.48 and 0.52 every 10s for 2 minutes",
  "defaults": {
    "spread_pct": 5.0,
    "fill_probability": 0.4
  },
  "pairs": {
    "OMG/USD": {
      "starting_price": 0.50,
      "waypoints": [
        { "t_ms": 0,      "mid": 0.50 },
        { "t_ms": 10000,  "mid": 0.52 },
        { "t_ms": 20000,  "mid": 0.48 },
        { "t_ms": 30000,  "mid": 0.52 },
        { "t_ms": 40000,  "mid": 0.48 },
        { "t_ms": 50000,  "mid": 0.52 },
        { "t_ms": 60000,  "mid": 0.48 },
        { "t_ms": 70000,  "mid": 0.52 },
        { "t_ms": 80000,  "mid": 0.48 },
        { "t_ms": 90000,  "mid": 0.52 },
        { "t_ms": 100000, "mid": 0.48 },
        { "t_ms": 110000, "mid": 0.52 },
        { "t_ms": 120000, "mid": 0.50 }
      ]
    }
  },
  "end_behavior": "hold_last"
}
```

**Price trajectory:** Zig-zag between 0.48 and 0.52 with 10s half-periods.

**Expected bot behavior:** Bids placed during dips fill, asks placed during
peaks fill. Over 120s the bot should accumulate positive realized P&L from
spread capture. No stop-loss or take-profit should trigger since the price
never deviates more than 4% from start.

### 5.4 `spread_widening.json` -- Liquidity Crisis

**Tests:** Bot behavior when spread exceeds quoting thresholds, fill probability
impact on inventory skew.

```json
{
  "name": "spread_widening",
  "description": "Spread widens from 5% to 30% and back, simulating liquidity crisis",
  "defaults": {
    "fill_probability": 0.3
  },
  "pairs": {
    "OMG/USD": {
      "starting_price": 0.50,
      "waypoints": [
        { "t_ms": 0,     "mid": 0.50, "spread_pct": 5.0 },
        { "t_ms": 10000, "mid": 0.50, "spread_pct": 5.0 },
        { "t_ms": 15000, "mid": 0.49, "spread_pct": 15.0, "fill_probability": 0.1 },
        { "t_ms": 25000, "mid": 0.48, "spread_pct": 30.0, "fill_probability": 0.02 },
        { "t_ms": 45000, "mid": 0.49, "spread_pct": 10.0, "fill_probability": 0.15 },
        { "t_ms": 60000, "mid": 0.50, "spread_pct": 5.0,  "fill_probability": 0.3 }
      ]
    }
  },
  "end_behavior": "hold_last"
}
```

**Price trajectory:** Slight drift down during the wide-spread period, then
recovery. The interesting dynamics are in spread and fill probability.

**Expected bot behavior:** When spread reaches 30% (3000 bps), the bot's
`min_spread_bps` filter (default 100) still allows quoting since the spread is
very wide. The bot's `spread_capture_pct` of 50% means it quotes 15% inside
the spread on each side. The key test is whether fill probability dropping to 2%
causes one-sided inventory accumulation (bids might fill but asks never do at
the wider spread).

### 5.5 `flash_spike.json` -- Flash Crash and Instant Recovery

**Tests:** Order handling during extreme single-tick volatility, resting order
fills on sudden price drops, post-only rejection behavior.

```json
{
  "name": "flash_spike",
  "description": "20% flash crash in one tick, then instant recovery",
  "defaults": {
    "spread_pct": 5.0,
    "fill_probability": 0.3
  },
  "pairs": {
    "OMG/USD": {
      "starting_price": 0.50,
      "waypoints": [
        { "t_ms": 0,     "mid": 0.50 },
        { "t_ms": 10000, "mid": 0.50 },
        { "t_ms": 10001, "mid": 0.40, "interpolation": "step", "spread_pct": 20.0 },
        { "t_ms": 12000, "mid": 0.40, "spread_pct": 20.0 },
        { "t_ms": 12001, "mid": 0.49, "interpolation": "step", "spread_pct": 8.0 },
        { "t_ms": 20000, "mid": 0.50, "spread_pct": 5.0 }
      ]
    }
  },
  "end_behavior": "hold_last"
}
```

**Price trajectory:** Flat at 0.50 for 10s, then instantaneous drop to 0.40
(step interpolation with 1ms gap), hold for 2s, then instantaneous recovery to
0.49, then gradual return to 0.50.

**Expected bot behavior:** The flash crash at t=10s and recovery at t=12s each
occur within one simulation tick (default 2000ms interval). Resting buy orders
at 0.49 get deterministically filled when the best ask drops to ~0.40. This
tests the fill logic in `do_book_tick` lines 94-112 where `best_ask <= o.price`
triggers a fill.

### 5.6 `multi_pair_divergence.json` -- Pairs Moving Opposite Directions

**Tests:** Per-pair inventory limits, total exposure cap, independent quoting.

```json
{
  "name": "multi_pair_divergence",
  "description": "OMG rises 15% while CAMP drops 15% over 60s",
  "defaults": {
    "spread_pct": 5.0,
    "fill_probability": 0.4
  },
  "pairs": {
    "OMG/USD": {
      "starting_price": 0.50,
      "waypoints": [
        { "t_ms": 0,     "mid": 0.50 },
        { "t_ms": 60000, "mid": 0.575 }
      ]
    },
    "CAMP/USD": {
      "starting_price": 0.004,
      "waypoints": [
        { "t_ms": 0,     "mid": 0.004 },
        { "t_ms": 60000, "mid": 0.0034 }
      ]
    }
  },
  "end_behavior": "hold_last"
}
```

**Expected bot behavior:** OMG inventory gains value (bids fill on the way up),
potentially triggering take-profit. CAMP inventory loses value as price drops,
potentially triggering stop-loss. This tests per-pair independence of risk
controls and whether `max_total_exposure_usd` is enforced across both positions.

---

## 6. Environment Variable Summary

| Variable | Current | With This Change |
|----------|---------|------------------|
| `MOCK_SCENARIO_FILE` | N/A | **NEW** -- path to scenario JSON file. When set, activates deterministic mode for pairs listed in the file. |
| `MOCK_PORT` | 3050 | Unchanged |
| `MOCK_PAIRS` | `OMG/USD:0.50,...` | Unchanged. Scenario pairs override starting_price. Non-scenario pairs still random-walk. |
| `MOCK_SPREAD_PCT` | 5.0 | Unchanged. Used as fallback when scenario does not specify per-segment spread. Still used for random-walk pairs. |
| `MOCK_VOLATILITY` | 0.3 | Unchanged. Ignored for scenario-driven pairs. Still used for random-walk pairs. |
| `MOCK_FILL_PROBABILITY` | 0.3 | Unchanged. Used as fallback when scenario does not specify per-segment fill_probability. Still used for random-walk pairs. |
| `MOCK_UPDATE_INTERVAL_MS` | 2000 | Unchanged. Controls tick rate for both modes. |
| `MOCK_SEED` | None | Unchanged. Seeds RNG for probabilistic fills in both modes. |
| `MOCK_STARTING_USD` | 10000 | Unchanged |

**Example invocation:**

```bash
MOCK_SCENARIO_FILE=test_data/scenarios/crash_recovery.json \
MOCK_SEED=42 \
MOCK_PAIRS="OMG/USD:0.50,CAMP/USD:0.004" \
MOCK_UPDATE_INTERVAL_MS=500 \
cargo run -p mock-exchange
```

---

## 7. Testing Strategy

### 7.1 Unit Tests in `scenario.rs`

| Test | What it verifies |
|------|------------------|
| `test_load_valid_scenario` | Minimal valid file parses with correct field values |
| `test_validate_rejects_fewer_than_2_waypoints` | Error on single-waypoint pair |
| `test_validate_rejects_nonzero_first_t_ms` | Error when first waypoint has `t_ms != 0` |
| `test_validate_rejects_non_monotonic` | Error when `t_ms` values are not strictly increasing |
| `test_validate_rejects_starting_price_mismatch` | Error when `starting_price` differs from first waypoint `mid` |
| `test_sample_at_exact_waypoint` | Returns exact `mid` at waypoint timestamps |
| `test_sample_at_linear_midpoint` | Returns linearly interpolated `mid` between two linear waypoints |
| `test_sample_at_step_holds_previous` | Returns `W[i].mid` (not interpolated) for step segments |
| `test_sample_at_hold_last` | Returns last waypoint `mid` after scenario ends with `hold_last` |
| `test_sample_at_loop` | Time wraps around and replays from beginning |
| `test_sample_at_stop` | `finished: true` returned after last waypoint with `stop` |
| `test_spread_override_per_segment` | Per-waypoint `spread_pct` returned for the correct time window |
| `test_fill_probability_override` | Per-waypoint `fill_probability` returned correctly |
| `test_defaults_used_as_fallback` | When waypoint has no overrides, `defaults` values are returned |

### 7.2 Unit Test in `orderbook.rs`

| Test | What it verifies |
|------|------------------|
| `test_set_mid_updates_book` | `set_mid(0.60, None)` on a book initialized at 0.50 produces `mid_price()` near 0.60, best bid < best ask |
| `test_set_mid_with_spread_override` | `set_mid(0.50, Some(10.0))` produces wider spread than `set_mid(0.50, Some(2.0))` |

### 7.3 Integration Test

Create `test_data/scenarios/crash_recovery.json` and write an integration test
that:

1. Starts the mock exchange with `MOCK_SCENARIO_FILE` set
2. Connects a WebSocket client
3. Subscribes to book updates for `OMG/USD`
4. Collects book snapshots for 10+ seconds
5. Extracts mid prices from each snapshot
6. Verifies mid prices follow the expected trajectory within tolerance
7. Runs a second time with the same `MOCK_SEED` and verifies fill outcomes
   are identical (determinism)

### 7.4 Scenario File Storage

```
test_data/
    recorded.jsonl              (existing)
    scenarios/                  (new directory)
        crash_recovery.json
        gradual_uptrend.json
        sideways_choppy.json
        spread_widening.json
        flash_spike.json
        multi_pair_divergence.json
```

---

## 8. Future Enhancements (Out of Scope)

These are not part of the initial implementation but documented for reference.

1. **Virtual time mode** (`MOCK_VIRTUAL_TIME=true`): Instead of reading the
   wall clock, increment `elapsed_ms` by exactly `update_interval_ms` each
   tick. Eliminates timing jitter for perfect CI reproducibility.

2. **Scenario chaining**: Run multiple scenarios in sequence (e.g., uptrend
   followed by crash) by concatenating waypoint lists with time offsets.

3. **Event injection**: Allow the scenario file to inject exchange-level events
   at specific timestamps: order rejections, WebSocket disconnects, rate limit
   errors, balance changes.

4. **TOML format**: Support TOML as an alternative to JSON for scenario files.
   TOML is more readable for hand-editing complex multi-pair scenarios.

5. **Recorded data conversion**: A utility to convert `recorded.jsonl` into a
   scenario file by extracting the mid price at each book snapshot timestamp.
   This would allow replaying real market conditions through the deterministic
   mock.

6. **Accelerated playback**: A `MOCK_TIME_SCALE=10` env var that runs the
   scenario at 10x speed (divide `t_ms` by the scale factor), useful for
   running long scenarios quickly in CI.
