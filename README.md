# kraken_mm_rs

A cryptocurrency market-making bot written in Rust. Runs automated two-sided quoting strategies on Kraken and Coinbase, earning the spread on low-liquidity trading pairs.

This is actively in development and running with real money.

## Architecture

The system runs as four cooperating services:

```
Exchange APIs (Kraken / Coinbase)
        ^
        |
   Exchange Proxy         Signs requests, rate limits, relays WebSockets.
   (port 8080)            Exchange-specific — swap the proxy to swap exchanges.
        ^
        |
      Bot                 Exchange-agnostic market maker. Pure event-driven
   (kraken-mm)            engine with no I/O — fully deterministic and testable.
        ^
        |
   State Store            CRUD service for pair configs and states.
   (port 3040)            Pushes updates to bot over WebSocket.
        ^
        |
   PnL Analyzer           Watches fills, computes per-pair edge metrics,
   (port 3031)            auto-promotes/demotes pairs based on performance.
```

The bot never sees API keys — the proxy holds them and signs requests. Swapping exchanges means swapping the proxy binary, not the bot.

## Crate Structure

12 crates in a Cargo workspace:

| Crate | Description |
|-------|-------------|
| `trading-primitives` | Exchange-agnostic types and traits (`OrderBook`, `Position`, `Fill`, `ExchangeClient`) |
| `core` | Engine, quoting logic, exchange adapters, risk management |
| `bot` | Market-making executor binary |
| `proxy-kraken` | Kraken signing proxy (HMAC-SHA512) |
| `proxy-coinbase` | Coinbase signing proxy (HMAC-SHA256) with full protocol translation |
| `proxy-common` | Shared proxy auth, rate limiting, WebSocket relay framework |
| `state-store` | Pair config/state CRUD with WebSocket relay to bot |
| `pnl-analyzer` | Per-pair edge metrics, automatic pair promotion/demotion |
| `mock-exchange` | Simulated exchange for testing (random walk + deterministic scenarios) |
| `scanner` | Scans exchange pairs for market-making opportunities |
| `recorder` | Records live WebSocket data to JSONL for replay |
| `dashboard` | Web dashboard binary |

## Engine Design

The core engine is a pure function: `EngineEvent -> Vec<EngineCommand>`. No I/O, no side effects. All exchange communication happens in the outer dispatch loop. This makes the engine fully deterministic and replayable — the same sequence of events always produces the same commands.

Events: `BookSnapshot`, `BookUpdate`, `Fill`, `OrderAcknowledged`, `OrderCancelled`, `OrderRejected`, `Tick`

Commands: `PlaceOrder`, `AmendOrder`, `CancelOrders`, `CancelAll`, `PersistState`, `LogTrade`, `RefreshDms`

## Quoting Strategy

- Places bid and ask limit orders around the mid price, capturing a configurable percentage of the spread
- Inventory skew adjusts quotes based on current position to encourage mean-reversion
- Cost floor on asks ensures every sell is profitable (avg_cost * (1 + min_profit_pct))
- Post-only protection clamps prices to never cross the book
- Only requotes when mid price moves beyond a threshold

## Risk Management

Multiple independent layers:

- **Kill switch**: Emergency shutdown if cumulative PnL drops below threshold (-$100 default)
- **Position limits**: Per-pair ($200) and total portfolio ($2,000) exposure caps
- **Stop-loss**: Liquidates position if price drops 3% below average cost
- **Take-profit**: Liquidates if price rises 10% above average cost
- **Dead man's switch**: Exchange cancels all orders if heartbeat not refreshed within 60 seconds
- **Rate limiter**: Token bucket in proxy layer prevents API rate limit violations

## Per-Pair State Machine

Each trading pair runs as a `ManagedPair` with a state enum:

- **Active** — Full two-sided quoting
- **WindDown** — Sell-only, auto-disables when position reaches zero
- **Liquidating** — Market sell, auto-disables on fill
- **Disabled** — No activity

The PnL analyzer automatically transitions pairs between states based on rolling edge metrics.

## Testing

```bash
cargo test --release
```

- **Mock exchange**: Simulates an exchange with configurable spread, volatility, and fill probability
- **Deterministic scenarios**: Pre-built JSON scenarios (crash recovery, gradual uptrend, flash spike, etc.) with price waypoints and interpolation
- **Replay testing**: Records live WebSocket data to JSONL, replays against the engine
- **Integration tests**: Full engine tests using recorded and synthetic market data

## Build

```bash
cargo build --release
```

Binaries are written to `./target/release/`. See `run_bot.sh.example` for environment variables needed at runtime.

## Supporting Tools

- `dashboard.py` — Mobile-friendly web dashboard for monitoring portfolio, positions, PnL, and pair states
- `kraken_mcp.py` — MCP server for Claude Code integration (read-only queries + emergency operations)
- `pnl.py` / `bootstrap_pnl.py` — PnL calculation and bootstrapping utilities

## Development

This project was built through agentic development with Claude. Claude manages the development lifecycle and assists with operations via the MCP server.

## Disclaimer

This is a personal project. Use at your own risk. Cryptocurrency trading involves significant risk of loss. This software is provided as-is with no guarantees of profitability or correctness.

## License

MIT
