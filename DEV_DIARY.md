# Dev Diary

## 2026-02-19 — Coinbase Integration Complete, Starting Trading Primitives

### Morning Session (completed before user left)
- Audited proxy & bot for Kraken coupling
- Created `proxy-common` crate (auth middleware, rate limiter, WS relay framework)
- Created `proxy-kraken` crate (Kraken signing, REST routing, WS relay) — replaces old `proxy` crate
- Created `proxy-coinbase` crate (Coinbase HMAC-SHA256, REST/WS translation, pair format conversion)
- Cleaned up bot-side Kraken assumptions (generic error messages, doc comments)
- All 136 tests pass, 3 release binaries produced

### Afternoon Session — Trading Primitives Crate

**Goal:** Extract exchange-agnostic trading types into `trading-primitives` crate that can be shared across core, proxies, state-store, and future exchange adapters.

**Analysis phase:**
- Audited all types in `kraken_core::types` — classified each as GENERIC, MIXED, or ENGINE-SPECIFIC
- Ran brainstorming agent that produced a prioritized work plan (confirmed approach)

**Types extracted to `trading-primitives` (all with tests):**
- `OrderSide`, `OrderRequest` — order primitives
- `OrderBook`, `LevelUpdate` — BTreeMap-backed order book with best_bid/ask/mid/spread/bps
- `Position` — qty/avg_cost tracking with apply_buy/apply_sell and realized P&L
- `Fill`, `TradeRecord` — execution fill and trade logging record
- `TickerData` — 24h ticker data
- `PairInfo` — pair metadata (decimals, minimums, fees, base_asset)
- `PairState`, `PairConfig`, `GlobalDefaults`, `ResolvedConfig` — pair lifecycle and config types

**Traits extracted:**
- `ExchangeClient` — REST operations (ws_token, pair_info, tickers, balances)
- `OrderManager` — order operations (place, amend, cancel, cancel_all)

**Core updated:**
- Added `trading-primitives` dependency
- All type modules now re-export from trading-primitives (backward compatible)
- Traits re-export from trading-primitives

**State-store unified:**
- Replaced duplicated PairState/PairConfig/GlobalDefaults (were using f64) with imports from trading-primitives (uses Decimal)
- Updated PatchPairConfig and PatchDefaultsRequest to use Decimal instead of f64
- Eliminates precision hazard from f64 round-trip in financial calculations
- JSON wire format unchanged (rust_decimal's serde-with-float serializes as JSON numbers)

**Verification:** `cargo build --release` and `cargo test --release` both pass — 141 tests, 0 failures.

**Types that intentionally stay in core (engine-specific):**
- `EngineEvent`, `EngineCommand`, `ApiAction`, `StateStoreAction` — event loop types
- `ManagedPair` — pair with quoter, state, retry counters (depends on `Quoter`)
- `Quoter`, `QuoteState` — quoting strategy
- `KillSwitch` — risk control
- `DeadManSwitch`, `TradeLogger`, `EventSource`, `Clock` traits — engine-specific

### Brainstorm Agent Ideas (for future work)
1. **Formalize proxy protocol** — move message types from `exchange/messages.rs` to `proxy-common` as shared structs. Both proxies would import canonical types instead of constructing JSON ad-hoc.
2. **ExchangeCapabilities** — let proxy advertise capabilities (e.g., DMS support). Engine skips DMS refresh for Coinbase instead of proxy silently acknowledging.
3. **Integration test: bot + mock-exchange + coinbase-proxy** — validate full Coinbase path end-to-end.
4. **Performance metrics / observability** — Prometheus metrics or structured metric events.
