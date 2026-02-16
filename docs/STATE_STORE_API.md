# State Store API Contract

The state store is a dumb CRUD service for pair configurations and states.
It has no business logic. It persists pair configs, serves them via REST,
and pushes changes to connected bots via WebSocket.

## Architecture

```
PnL Analyzer ──► State Store ◄──► Bot (executor)
   (REST writer)    (CRUD + WS relay)    (WS consumer)
You (curl) ────►
```

- **PnL Analyzer** writes pair state/config changes via REST
- **Bot** connects via WS, receives pair configs, operates accordingly
- **Humans** can curl the REST API to inspect or override anything
- **State Store does NOT track fills, PnL, or market data**

## Data Model

### Pair

The core resource. Each pair has a state and a config.

```rust
/// The persistent pair record
struct PairRecord {
    symbol: String,               // e.g. "OMG/USD"
    state: PairState,
    config: PairConfig,
    disabled_reason: Option<String>,
    auto_enable_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}
```

### PairState

```rust
enum PairState {
    Disabled,      // No quoting, no orders. Inert.
    WindDown,      // Sell-only (limit sells). Transitions to Disabled when position=0.
                   // (Bot reports position=0 → state store auto-transitions to Disabled)
    Liquidating,   // Market sell in-flight. Transitions to Disabled on completion.
                   // (Bot reports liquidation complete → state store auto-transitions to Disabled)
    Active,        // Normal market-making with config parameters.
}
```

Note: "Cautious" vs "Aggressive" is expressed through config parameters (wider/narrower
spread, smaller/larger order size, lower/higher max inventory), not as separate states.
The PnL analyzer adjusts config values to achieve the desired aggressiveness.

### PairConfig

Per-pair trading parameters. Each field is optional — `null` means "use global default."

```rust
struct PairConfig {
    order_size_usd: Option<Decimal>,       // Per-side order size
    max_inventory_usd: Option<Decimal>,    // Max position value for this pair
    min_spread_bps: Option<Decimal>,       // Minimum spread to quote
    spread_capture_pct: Option<Decimal>,   // Fraction of spread to capture
    min_profit_pct: Option<Decimal>,       // Min margin on sells above avg_cost
    stop_loss_pct: Option<Decimal>,        // Auto-liquidate if price drops this %
    take_profit_pct: Option<Decimal>,      // Auto-liquidate if price rises this %
}
```

### GlobalDefaults

Fallback values used when a pair's config field is `null`.

```rust
struct GlobalDefaults {
    order_size_usd: Decimal,         // default: 50.0
    max_inventory_usd: Decimal,      // default: 200.0
    min_spread_bps: Decimal,         // default: 100.0
    spread_capture_pct: Decimal,     // default: 0.50
    min_profit_pct: Decimal,         // default: 0.01 (1%)
    stop_loss_pct: Decimal,          // default: 0.03 (3%)
    take_profit_pct: Decimal,        // default: 0.10 (10%)
}
```

## REST API

Base URL: `http://localhost:{STATE_STORE_PORT}` (default 3040)
Auth: `Authorization: Bearer {STATE_STORE_TOKEN}`

### Pairs

#### `GET /pairs`

List all pairs.

**Query params:**
- `state` (optional): filter by state (`active`, `disabled`, `wind_down`, `liquidating`)

**Response 200:**
```json
{
  "pairs": [
    {
      "symbol": "OMG/USD",
      "state": "active",
      "config": {
        "order_size_usd": 75.0,
        "max_inventory_usd": 500.0,
        "min_spread_bps": null,
        "spread_capture_pct": null,
        "min_profit_pct": null,
        "stop_loss_pct": null,
        "take_profit_pct": null
      },
      "disabled_reason": null,
      "auto_enable_at": null,
      "created_at": "2026-02-16T10:00:00Z",
      "updated_at": "2026-02-16T12:30:00Z"
    }
  ]
}
```

#### `GET /pairs/{symbol}`

Get a single pair. Symbol is URL-encoded (e.g. `OMG%2FUSD` or use dash: `OMG-USD`).

**Response 200:** Single pair object (same shape as above).
**Response 404:** `{ "error": "pair not found" }`

#### `PUT /pairs/{symbol}`

Create or fully replace a pair. Used to add new pairs or overwrite config.

**Request body:**
```json
{
  "state": "active",
  "config": {
    "order_size_usd": 75.0,
    "max_inventory_usd": 500.0
  }
}
```

- `state` defaults to `disabled` if omitted on create.
- `config` fields default to `null` (use global) if omitted.

**Response 200:** Updated pair object.
**Response 201:** Created pair object (new pair).

#### `PATCH /pairs/{symbol}`

Partial update. Merge-patches config fields and/or state.

**Request body (any fields optional):**
```json
{
  "state": "wind_down",
  "config": {
    "max_inventory_usd": 100.0
  },
  "disabled_reason": "edge dropped below threshold",
  "auto_enable_at": "2026-02-16T14:00:00Z"
}
```

- Only provided config fields are updated; others are left unchanged.
- Setting a config field to `null` explicitly reverts it to global default.

**Response 200:** Updated pair object.
**Response 404:** `{ "error": "pair not found" }`

#### `DELETE /pairs/{symbol}`

Remove a pair entirely. The state store pushes a `PairRemoved` message to the bot.
The bot should cancel all orders and stop quoting. Position handling is up to the caller
(typically: set state to `liquidating` first, wait for completion, then delete).

**Response 204:** No content.
**Response 404:** `{ "error": "pair not found" }`

### Global Defaults

#### `GET /defaults`

**Response 200:**
```json
{
  "order_size_usd": 50.0,
  "max_inventory_usd": 200.0,
  "min_spread_bps": 100.0,
  "spread_capture_pct": 0.50,
  "min_profit_pct": 0.01,
  "stop_loss_pct": 0.03,
  "take_profit_pct": 0.10
}
```

#### `PATCH /defaults`

Partial update of global defaults.

**Request body:**
```json
{
  "order_size_usd": 75.0
}
```

**Response 200:** Updated defaults object.

### Health

#### `GET /health`

**Response 200:**
```json
{
  "status": "ok",
  "bot_connected": true,
  "pairs_count": 12,
  "uptime_secs": 3600
}
```

## WebSocket Protocol

Endpoint: `ws://localhost:{STATE_STORE_PORT}/ws`
Auth: query param `?token={STATE_STORE_TOKEN}` or `Authorization` header.

### Connection Lifecycle

1. Bot connects to `/ws`
2. State store sends `Snapshot` with all pairs + defaults
3. On any REST mutation, state store pushes incremental update to bot
4. Bot sends periodic `Heartbeat` messages
5. Bot may send `PairReport` with current position/status (informational, not persisted)

### Messages: State Store → Bot

#### Snapshot (sent on connect)
```json
{
  "type": "snapshot",
  "pairs": [ /* full pair objects */ ],
  "defaults": { /* global defaults */ }
}
```

#### PairUpdated (on any pair create/update)
```json
{
  "type": "pair_updated",
  "pair": { /* full pair object */ }
}
```

#### PairRemoved (on pair delete)
```json
{
  "type": "pair_removed",
  "symbol": "OMG/USD"
}
```

#### DefaultsUpdated (on defaults change)
```json
{
  "type": "defaults_updated",
  "defaults": { /* full defaults object */ }
}
```

### Messages: Bot → State Store

#### Heartbeat
```json
{
  "type": "heartbeat",
  "timestamp": "2026-02-16T12:00:00Z",
  "active_pairs": 8,
  "total_exposure_usd": 1234.56
}
```

#### PairReport (informational, periodic — state store can use to detect wind_down/liquidation completion)
```json
{
  "type": "pair_report",
  "symbol": "OMG/USD",
  "position_qty": "125.5",
  "position_avg_cost": "0.85",
  "exposure_usd": "106.67",
  "quoter_state": "quoting",
  "has_open_orders": true
}
```

The state store uses `pair_report` with `position_qty == 0` to auto-transition:
- `WindDown` → `Disabled`
- `Liquidating` → `Disabled`

This is the ONLY "smart" behavior in the state store — a simple auto-transition
when the bot reports that a pair's position is fully closed.

## Persistence

The state store persists to a JSON file (`state_store.json`) on every mutation.
On startup, it loads from this file. Structure:

```json
{
  "defaults": { /* GlobalDefaults */ },
  "pairs": {
    "OMG/USD": { /* PairRecord */ },
    "CAMP/USD": { /* PairRecord */ }
  },
  "version": 1
}
```

Future: could move to SQLite if needed, but JSON file is fine for <100 pairs.

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `STATE_STORE_PORT` | 3040 | REST + WS listen port |
| `STATE_STORE_TOKEN` | (required) | Bearer token for auth |
| `STATE_STORE_FILE` | `state_store.json` | Persistence file path |

## Symbol Encoding in URLs

Pairs contain `/` which is problematic in URLs. Two options supported:
- URL-encode: `/pairs/OMG%2FUSD`
- Dash convention: `/pairs/OMG-USD` (state store normalizes to `OMG/USD` internally)
