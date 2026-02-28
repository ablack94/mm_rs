# Market-Making Bot

## Project Structure

Cargo workspace with 11 crates under `crates/`:

| Crate | Type | Binary | Description |
|-------|------|--------|-------------|
| `trading-primitives` | library | — | Exchange-agnostic types (OrderBook, Position, Fill, etc.), traits (ExchangeClient, OrderManager), shared config (PairState, PairConfig) |
| `core` | library | — | Engine, exchange adapters, bot-specific types, config (`kraken_core`). Re-exports trading-primitives. |
| `bot` | binary | `kraken-mm` | Market-making executor (exchange-agnostic, connects via proxy + state store) |
| `state-store` | binary | `state-store` | CRUD service for pair configs/states, WS relay to bot (standalone, no `kraken-core` dep) |
| `pnl-analyzer` | binary | `pnl-analyzer` | Edge metrics, pair promotion/demotion, writes to state store |
| `mock-exchange` | binary | `mock-exchange` | Simulated exchange for testing (random walk + deterministic scenario mode) |
| `proxy-common` | library | — | Shared proxy types, auth middleware, WS relay framework, rate limiter |
| `proxy-kraken` | binary | `kraken-proxy` | Kraken signing proxy (holds keys, signs requests, relays WS) |
| `proxy-coinbase` | binary | `coinbase-proxy` | Coinbase proxy (signs requests, translates REST/WS protocol) |
| `recorder` | binary | `recorder` | Records live WS data to JSONL for replay |
| `scanner` | binary | `scanner` | Scans Kraken pairs for MM opportunities (standalone, no `kraken-core` dep) |

## Build & Run

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release
# Binary at: ./target/release/kraken-mm
```

- Use `python3` not `python` on this system
- pip: `python3 /tmp/get-pip.py --break-system-packages` then `~/.local/bin/pip3`

## Architecture

### Trait Boundaries
All exchange interaction goes through traits for testability:
- `ExchangeClient` — REST calls (balances, ticker, asset pairs, WS token)
- `OrderManager` — place/amend/cancel orders (WS)
- `DeadManSwitch` — cancel-all-after heartbeat
- `TradeLogger` — append trade fills to CSV
- `EventSource` — replay recorded events for testing
- `Clock` / `SystemClock` — wall-clock abstraction

### Engine Core (`crates/core/src/engine/core.rs`)
Event-driven loop processing `EngineEvent` variants:
- `BookSnapshot` / `BookUpdate` — order book data from public WS
- `Fill` — trade execution from private WS
- `OrderAcknowledged` / `OrderCancelled` / `OrderRejected` — order lifecycle
- `Tick` — periodic timer (10s default)

Produces `EngineCommand` variants:
- `PlaceOrder` / `AmendOrder` / `CancelOrders` / `CancelAll`
- `PersistState` / `LogTrade`
- `RefreshDms` / `DisableDms`

### Quoting Logic (`engine/quoter.rs`)
Per-pair quoter tracks bid/ask cl_ord_ids, last mid price. Requotes when mid moves beyond `requote_threshold_pct`. Cost floor on asks ensures every sell is profitable (avg_cost * (1 + min_profit_pct)).

### Inventory Management (`engine/inventory.rs`)
Caps per-pair exposure at `max_inventory_usd` and total at `max_total_exposure_usd`.

### Risk Controls (`risk/`)
- **Kill switch**: shuts down if cumulative PnL drops below `kill_switch_loss_usd`
- **Stop-loss**: liquidates position if price drops `stop_loss_pct` below avg_cost
- **Take-profit**: liquidates if price rises `take_profit_pct` above avg_cost
- **Dead man's switch**: exchange cancels all orders if heartbeat not refreshed
- **Rate limiter**: token bucket in proxy, delays when approaching limit

### Exchange Adapters
- `exchange/live.rs` — Live exchange connection via proxy (splits private WS into read/write loops)
- `exchange/rest.rs` — Direct Kraken REST client with HMAC-SHA512 signing (scanner/recorder only)
- `exchange/proxy_client.rs` — REST client that routes through signing proxy (bot uses this)
- `exchange/ws.rs` — WebSocket connection helpers (including `connect_with_token` for proxy auth)
- `exchange/replay.rs` — Replay recorded JSONL for testing
- `exchange/messages.rs` — WS message types and parser (Kraken WS v2 format as protocol)

### Proxy Architecture (Multi-Exchange)
The bot is exchange-agnostic — it talks a standard WS/REST protocol to a proxy. The proxy handles exchange-specific translation, signing, and rate limiting. Different proxy binaries for different exchanges, same bot binary.

```
Bot (generic) ──► Kraken Proxy (kraken-proxy)   ──► Kraken API
               ──► Coinbase Proxy (coinbase-proxy) ──► Coinbase API
```

Set `PROXY_URL` (required) and `PROXY_TOKEN` env vars. WS URLs are derived from `PROXY_URL`:
- Private WS: `ws://{proxy}/ws/private`
- Public WS: `ws://{proxy}/ws/public`
- REST: routes through `ProxyClient` to `{proxy}/0/...`
The bot never sees API keys (proxy holds them).

**Proxy crate structure:**
- `proxy-common` — shared auth middleware, WS relay framework, rate limiter
- `proxy-kraken` — Kraken HMAC-SHA512 signing, WS v2 relay, token injection
- `proxy-coinbase` — Coinbase HMAC-SHA256 signing, REST↔WS translation, pair format conversion (BTC-USD ↔ BTC/USD)

## Key Types
- `Symbol` → `trading-primitives/src/symbol.rs` (newtype for currency: "BTC", "USD")
- `Ticker` → `trading-primitives/src/symbol.rs` (trading pair: base/quote Symbol, e.g. "BTC/USD")
- `PairInfo` → `trading-primitives/src/pair.rs` (decimals, minimums, fees; field `pair: Ticker`)
- `Position` → `trading-primitives/src/position.rs` (qty, avg_cost, realized_pnl)
- `OrderRequest` → `trading-primitives/src/order.rs` (price, qty, side; field `pair: Ticker`)
- `Fill` → `trading-primitives/src/fill.rs` (field `pair: Ticker`)
- `BookLevel` / `Spread` → `trading-primitives/src/book.rs`

## Config Defaults
| Parameter | Default | Description |
|-----------|---------|-------------|
| `order_size_usd` | $100 | Per-side order size |
| `min_spread_bps` | 100 | Minimum spread to quote |
| `spread_capture_pct` | 50% | How much of spread to capture (rest is edge) |
| `maker_fee_pct` | 0.23% | Maker fee at current tier (exchange-specific) |
| `min_profit_pct` | 1% | Minimum profit margin on sells |
| `max_inventory_usd` | $200 | Max position per pair |
| `max_total_exposure_usd` | $2000 | Max total across all pairs |
| `kill_switch_loss_usd` | -$100 | Emergency shutdown threshold |
| `stop_loss_pct` | 3% | Stop-loss liquidation trigger |
| `take_profit_pct` | 10% | Take-profit liquidation trigger |
| `dms_timeout_secs` | 60 | Dead man's switch timeout |

## Testing
- Integration tests: `crates/core/tests/` (uses `../../test_data/recorded.jsonl`)
- Run: `cargo test --release`
- Tests use `EventSource` trait to replay recorded data against engine

## State Persistence
- State store service is the source of truth for pair configs and state
- `logs/trades.csv` — append-only trade log (local debug aid)

## Per-Pair Architecture (Feb 2026 refactor)

The engine no longer uses flat `disabled_pairs`/`sell_only_pairs`/`pending_liquidation`/`cooldown_until` collections. Each pair is a `ManagedPair`:

```rust
ManagedPair { symbol, state: PairState, config: PairConfig, quoter, pair_info, liq_retry_count }
```

**`PairState` enum:** `Disabled`, `WindDown` (sell-only, auto-disables on position=0), `Liquidating` (market sell, auto-disables on fill), `Active`

**`PairConfig`:** per-pair overrides (all `Option<Decimal>`): `order_size_usd`, `max_inventory_usd`, `min_spread_bps`, `spread_capture_pct`, `min_profit_pct`, `stop_loss_pct`, `take_profit_pct`. `None` = use `GlobalDefaults`.

**`ResolvedConfig`:** merge of pair config + global defaults, computed at quote time.

Key types in `crates/core/src/types/managed_pair.rs`.

## Service Architecture

```
Kraken WS ──► PnL Analyzer (edge metrics, promotes/demotes pairs)
                   │ REST
                   ▼
             State Store (CRUD pairs/config, WS relay, JSON persistence)
                   │ WS
                   ▼
             Bot (per-pair ManagedPair objects, dumb executor)
```

- **State Store** (port 3040): dumb CRUD + WS relay. API contract in `docs/STATE_STORE_API.md`
- **PnL Analyzer** (port 3031): watches Kraken fills via proxy WS, computes per-pair `Net PnL / Traded Volume`, adjusts pair configs via state store REST
- **Bot**: no REST API, requires state store. On startup: connects to state store WS, waits for initial snapshot (pair list + defaults), fetches pair_info from exchange, subscribes to book data, then runs. No CLI pair args or config files.

## Key Patterns
- All WS sends go through a single mpsc channel to a write loop (serialized access)
- `req_id → cl_ord_id` map resolves Kraken rejection responses (Kraken often omits cl_ord_id in errors)
- Per-pair state checked via `pair.state.allows_quoting()` / `allows_buys()` / `allows_sells()`
- Liquidation is two-phase: cancel pair orders → wait → market sell
- State store auto-transitions WindDown/Liquidating → Disabled when bot reports position=0

## Mock Exchange

The `mock-exchange` crate simulates a Kraken exchange for integration testing.

**Config:** All via env vars (`MOCK_PORT`, `MOCK_PAIRS`, `MOCK_SPREAD_PCT`, `MOCK_VOLATILITY`, `MOCK_FILL_PROBABILITY`, `MOCK_UPDATE_INTERVAL_MS`, `MOCK_SEED`, `MOCK_STARTING_USD`).

**Deterministic Scenario Mode:** Set `MOCK_SCENARIO_FILE` to a JSON file path. Pairs listed in the scenario follow predefined price trajectories (waypoints with linear/step interpolation); unlisted pairs continue random-walking. Design doc: `docs/DETERMINISTIC_MOCK_DESIGN.md`.

**Scenario files:** `test_data/scenarios/` — crash_recovery, gradual_uptrend, sideways_choppy, spread_widening, flash_spike, multi_pair_divergence.

**Example:**
```bash
MOCK_SCENARIO_FILE=test_data/scenarios/crash_recovery.json \
MOCK_SEED=42 MOCK_PAIRS="OMG/USD:0.50,CAMP/USD:0.004" \
MOCK_UPDATE_INTERVAL_MS=500 cargo run -p mock-exchange
```

## Known Behaviors
- US:NJ restricted pairs (SPICE, STEP, EPT) get auto-disabled on first rejection
- Dust positions below exchange minimums are removed from state (fixed Feb 2026)
- No-op amends (same price) are skipped to avoid Kraken rejections (fixed Feb 2026)
