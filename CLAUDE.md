# Kraken Market-Making Bot

## Project Structure

Cargo workspace with 6 crates under `crates/`:

| Crate | Type | Binary | Description |
|-------|------|--------|-------------|
| `core` | library | — | Shared engine, exchange adapters, types, traits, config (`kraken_core`) |
| `bot` | binary | `kraken-mm` | Main market-making bot |
| `proxy` | binary | `kraken-proxy` | Signing proxy (holds API keys, signs requests) |
| `pnl-tracker` | binary | `pnl-tracker` | Standalone PnL analysis tool |
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
- `StateStore` — persist/load bot state (JSON)
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
- **Dead man's switch**: Kraken cancels all orders if heartbeat not refreshed
- **Rate limiter**: tracks Kraken's rate counter, delays when approaching limit

### Exchange Adapters
- `exchange/live.rs` — Real Kraken connection (splits private WS into read/write loops)
- `exchange/rest.rs` — Direct REST client with HMAC-SHA512 signing
- `exchange/proxy_client.rs` — REST client that routes through signing proxy
- `exchange/ws.rs` — WebSocket connection helpers (including `connect_with_token` for proxy auth)
- `exchange/replay.rs` — Replay recorded JSONL for testing
- `exchange/messages.rs` — Kraken WS v2 message types and parser

### Proxy Mode
When `PROXY_MODE=true`, the bot:
- Uses `proxy_client.rs` instead of direct REST
- Connects WS with `Authorization: Bearer {token}` header
- Gets a placeholder WS token (proxy injects real one upstream)
- Never sees API keys (proxy holds them)

## Key Types
- `TickerData` → `types/ticker.rs`
- `PairInfo` → `types/pair.rs` (decimals, minimums, fees)
- `Position` → `types/position.rs` (qty, avg_cost, realized_pnl)
- `OrderState` → `types/order.rs` (price, qty, side, acked status)
- `Fill` → `types/fill.rs`
- `BookLevel` → `types/book.rs`

## Config Defaults
| Parameter | Default | Description |
|-----------|---------|-------------|
| `order_size_usd` | $100 | Per-side order size |
| `min_spread_bps` | 100 | Minimum spread to quote |
| `spread_capture_pct` | 50% | How much of spread to capture (rest is edge) |
| `maker_fee_pct` | 0.23% | Kraken maker fee at current tier |
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
- `state.json` — positions, open orders, realized PnL, trade count
- `logs/trades.csv` — append-only trade log

## API Server (`api/server.rs`)
REST API on `BOT_API_PORT` (default 3030) with `BOT_API_TOKEN` auth:
- Routes through `ExchangeClient` trait, not raw reqwest

## Key Patterns
- All WS sends go through a single mpsc channel to a write loop (serialized access)
- `req_id → cl_ord_id` map resolves Kraken rejection responses (Kraken often omits cl_ord_id in errors)
- Pair disabling: `disabled_pairs` HashSet, checked at quote time
- Sell-only mode: `sell_only_pairs` HashSet for downtrending pairs
- Liquidation is two-phase: cancel pair orders → wait → market sell

## Known Behaviors
- US:NJ restricted pairs (SPICE, STEP, EPT) get auto-disabled on first rejection
- CAMP/USD enters sell-only mode when 24h change exceeds `downtrend_threshold_pct`
- Dust positions below exchange minimums are removed from state (fixed Feb 2026)
- No-op amends (same price) are skipped to avoid Kraken rejections (fixed Feb 2026)
