# Best Practices — kraken-mm

## 1. Trait Boundaries

Every external dependency (network, filesystem, clock) must sit behind a trait:

- **`ExchangeClient`** — REST API calls (pair info, tickers, balances, WS tokens)
- **`OrderManager`** — order placement/amendment/cancellation via WS
- **`DeadManSwitch`** — DMS refresh/disable via WS
- **`StateStore`** — state persistence (JSON file, could be DB)
- **`TradeLogger`** — trade logging (CSV, could be DB)
- **`EventSource`** — event production (live WS, replay file)
- **`Clock`** — wall-clock time (`SystemClock` for prod, injectable for tests)

This makes it possible to swap backends (direct Kraken vs. proxy), run integration tests with replay data, and inject mock implementations for unit tests.

## 2. Engine Purity

The `Engine` struct is the core business logic and must have **zero I/O**:

- No network calls, no filesystem access, no `Utc::now()`
- Takes `EngineEvent`s, returns `Vec<EngineCommand>`
- All timestamps come from events (set by the event source)
- Fully deterministic: same event sequence → same command sequence
- Side effects happen in the command dispatcher (outside the engine)

This makes the engine directly testable without mocks or async runtime.

## 3. Error Handling

- Use `anyhow::Result` for application-level errors
- Never `unwrap()` on user data or exchange responses — use `?` or provide defaults
- Exchange API errors should be checked via `check_error()` before accessing response data
- Log errors with context (`tracing::error!` with structured fields)
- Let errors propagate up to the appropriate handler rather than swallowing them

## 4. Domain Types

- Trait methods use domain types (`PairInfo`, `TickerData`, `Fill`, `Position`), not raw JSON
- `serde` is an implementation detail of specific adapters (REST, JSON store)
- Domain types live in `src/types/`
- Exchange-specific serialization (e.g., `decimal_as_f64`) lives in `src/exchange/messages.rs`

## 5. Testing

- **Unit tests**: in-file `#[cfg(test)]` modules, test pure logic (engine, position math)
- **Integration tests**: via `ReplaySource` or `EventSource` trait with recorded data
- **Mock trait impls**: for testing the dispatcher/wiring without real exchange
- Engine tests use `handle_event()` directly — no async, no channels
- Test both happy path and edge cases (spread too narrow, kill switch, liquidation)

## 6. Security

- API keys are **never** logged, serialized to state, or exposed in API responses
- The `/api/config` endpoint redacts `api_key` and `api_secret`
- When running in proxy mode, the bot holds no keys at all — the proxy handles signing
- Bearer tokens for API and proxy authentication should be strong random strings
- Never commit `.env` files or key material to version control

## 7. Concurrency

- **Channels** (`mpsc`) for inter-task communication (events, commands)
- **`Arc<RwLock<T>>`** for shared state (bot state accessed by API server + engine)
- **Never hold locks across `.await`** — acquire, read/write, release, then await
- WS write serialized through a channel → single writer, no lock contention
- WS read loop and write loop run as separate tasks (split stream pattern)
- Background tasks (book feed, ticker, private WS) are tracked via `JoinHandle` for clean shutdown
