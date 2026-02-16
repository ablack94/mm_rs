# Dashboard Rewrite Status — Feb 2026

## Status: COMPLETE, ready for smoke test

## What was done

Rewrote `dashboard.py` to pull from **state store** + **PnL analyzer** instead of the non-existent bot REST API.

### Changes made to `dashboard.py`:

1. **Config**: Removed `BOT_API_URL`, `BOT_API_TOKEN`, `STATE_JSON`. Added `STATE_STORE_URL` (default `:3040`), `STATE_STORE_TOKEN`, `PNL_API_TOKEN`. Removed unused `pathlib.Path` import.

2. **Data helpers**: Removed `_bot_get()`, `_read_state_file()`. Added `_state_store_get()` with Bearer auth. Updated `_pnl_get()` with optional Bearer auth. Added `_safe_float()` for decimal string parsing.

3. **`build_dashboard_data()`** — fully rewritten:
   - Step 1: State store `/health` → `bot_connected`
   - Step 2: State store `/pairs` → pair states, configs, disabled_reason
   - Step 3: State store `/defaults` → global trading params
   - Step 4: PnL analyzer `/pnl` → realized/unrealized PnL, fees, overall edge
   - Step 5: PnL analyzer `/pairs` → per-pair edge (1h/6h/24h), positions
   - Step 6: Proxy ticker → live prices
   - Step 7: Merge into unified pair list + derive positions (qty > 0)

4. **HTML template**:
   - Replaced "Open Orders" card → "Overall Edge" card
   - Added "Global Defaults" grid section
   - Pair Status table → 7 columns (Pair, State, Edge 1h/6h/24h, Position, Config)
   - Added `wind_down`, `liquidating` badge styles
   - Config column: "defaults" or "custom" with hover tooltip
   - Disabled reason shown inline next to state badge
   - **Removed**: Open Orders table, Recent Trades table
   - Status dot: green (bot connected), yellow (bot disconnected), red (state store down)

5. **Startup banner**: Updated to show State Store / PnL Analyzer / Proxy URLs.

### Syntax verified
`python3 -c "import py_compile; py_compile.compile('dashboard.py', doraise=True)"` — passed.

## What's next

1. **Smoke test**: Start dashboard with correct env vars and verify it renders data from live services
2. The state store was confirmed running on port 3040 (PID 24955) with a bot connected (PID 25001, pairs: OMG/USD, CAMP/USD)
3. Multiple zombie kraken-mm processes exist — harmless but noisy in `ps`
4. PnL analyzer status was not checked yet — need to verify it's running or start it
5. No PnL analyzer was visible in the process list — may need to start one

## Env vars for smoke test
```bash
export STATE_STORE_URL=http://localhost:3040
export STATE_STORE_TOKEN=test-token-123   # or whatever the running instance uses
export PNL_API_URL=http://localhost:3031
export PNL_API_TOKEN=
export PROXY_URL=http://10.255.255.254:8080
python3 /workarea/kraken_mm_rs/dashboard.py
```

## Running services snapshot (at time of interruption)
- `state-store` PID 24955 on port 3040
- `kraken-mm` PID 25001 (OMG/USD, CAMP/USD)
- Multiple older state-store instances on ports 3061, etc. (from integration tests)
- PnL analyzer: NOT confirmed running
