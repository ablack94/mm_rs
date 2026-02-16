# Kraken MM Bot — Operations Runbook

## Quick Start

### Prerequisites
- Signing proxy running at `10.255.255.254:8080`
- Rust toolchain: `export PATH="$HOME/.cargo/bin:$PATH"`
- Built binary: `cargo build --release`

### Start the Bot
```bash
export PROXY_MODE=true
export PROXY_URL="http://10.255.255.254:8080"
export PROXY_TOKEN="<token from .mcp.json>"
export BOT_API_TOKEN="mcp-bot-token-2026"
export BOT_API_PORT="3030"
export RUST_LOG=info
./target/release/kraken-mm 2>&1 | tee -a bot.log
```

Or use the launch script:
```bash
./run_bot.sh
```

### Start as Background Task (Claude Code)
```bash
# Bot process
sleep 3 && : > bot.log && \
  export PROXY_MODE=true PROXY_URL="http://10.255.255.254:8080" \
  PROXY_TOKEN="<token>" BOT_API_TOKEN="mcp-bot-token-2026" \
  BOT_API_PORT="3030" RUST_LOG=info && \
  ./target/release/kraken-mm 2>&1 | tee -a bot.log
```
Run with `run_in_background: true`.

### Alert Tail (Critical Events Only)
```bash
tail -f bot.log | grep --line-buffered -E "FILL|STOP-LOSS|LIQUIDAT|WS recv error|read loop ended|Private WS read loop"
```
Run as a second background task. The system notifies Claude only when these patterns appear — silent during normal quoting.

## MCP Server (Claude Code Integration)

Config at `/home/node/.claude/projects/-workarea/.mcp.json`:
```json
{
  "mcpServers": {
    "kraken": {
      "command": "python3",
      "args": ["/workarea/kraken_mm_rs/kraken_mcp.py"],
      "env": {
        "PROXY_URL": "http://10.255.255.254:8080",
        "PROXY_TOKEN": "<token>"
      }
    }
  }
}
```

### Available MCP Tools
| Tool | Description | Safe? |
|------|-------------|-------|
| `get_balances` | All asset balances | Read-only |
| `get_open_orders` | Current open orders | Read-only |
| `get_trade_history` | Recent fills | Read-only |
| `get_ticker` | Current price/spread/volume | Read-only |
| `get_asset_pairs` | Pair info (decimals, minimums) | Read-only |
| `get_ohlc` | OHLC candle data | Read-only |
| `health` | Proxy health check | Read-only |
| `market_sell` | Emergency market sell full balance | **DESTRUCTIVE** |
| `limit_sell` | Limit sell at best ask (or specified price) | **MODIFYING** |
| `cancel_order` | Cancel order by txid | **MODIFYING** |

## Monitoring

### Log Patterns
| Pattern | Meaning | Action |
|---------|---------|--------|
| `FILL` | Trade executed | Normal — check PnL impact |
| `STOP-LOSS TRIGGERED` | Price dropped below threshold | Liquidation starting |
| `LIQUIDATION PHASE` | Cancelling orders / market selling | Watch for completion |
| `Removing dust position` | Sub-minimum qty cleaned up | Normal after liquidation |
| `Pair restricted — disabling` | US:NJ or other regulatory block | Expected for SPICE/STEP/EPT |
| `DOWNTREND — sell-only` | 24h change below -5% | Pair won't buy, only sell |
| `WS recv error` / `read loop ended` | Connection dropped | Bot needs restart |
| `Kill switch activated` | PnL below -$100 | Bot shutting down, investigate |
| `No amendable parameters` | Amend sent with same price | Bug (fixed Feb 2026) |

### Key Metrics in Tick Status (every 10s)
```
Tick status open_orders=6 acked=6 trades=173 pnl=1030.71 fees=18.57 exposure_usd=580.23 pairs=[...]
```
- `open_orders` vs `acked`: should be equal (all orders confirmed by exchange)
- `pnl`: cumulative realized PnL
- `exposure_usd`: total position value across all pairs
- `pairs`: list with status (Quoting / SellOnly / Liquidating)

## Restart Procedure

1. Stop the running bot process (TaskStop or kill)
2. Rebuild if code changed: `cargo build --release`
3. Clear log if desired: `: > bot.log`
4. Start bot (see Quick Start above)
5. Start alert tail
6. Watch first 30s of logs for:
   - "Private WS connected" — connection OK
   - "Reconciling state positions" — state loaded
   - Pairs going to "Quoting" status
   - US:NJ rejections (expected, auto-disabled)

The bot persists state to `state.json` — positions, PnL, and open orders survive restarts. On startup it reconciles with Kraken balances.

## Common Issues

### "Set KRAKEN_API_KEY" on startup
Binary is stale — doesn't have proxy mode support. Rebuild: `cargo build --release`

### WS 401 Unauthorized
Proxy token mismatch or `connect_with_token` not being used. Verify `PROXY_TOKEN` matches the proxy's expected token.

### Dust position loop (FIXED)
After liquidation, sub-minimum dust would re-trigger stop-loss every tick. Fixed: engine now removes dust positions from state.

### No-op amend spam (FIXED)
When cost floor pins ask price, mid movements triggered amends with unchanged price. Fixed: engine now skips amends when new price equals current price.

### FXS cost floor pinning
If FXS position avg_cost is above current market ask, the ask gets pinned to cost floor. Only buy fills will occur until market price exceeds avg_cost * 1.01. This is by design — prevents selling at a loss.

## File Locations
| File | Purpose |
|------|---------|
| `state.json` | Bot state (positions, orders, PnL) |
| `scanned_pairs.json` | Scanner output (pair selection) |
| `bot.log` | Runtime logs (tee'd from stdout) |
| `logs/trades.csv` | Append-only trade log |
| `run_bot.sh` | Launch script with env vars |
| `kraken_mcp.py` | MCP server for Claude Code |
| `config.toml` | Config overrides (if present) |

## Emergency Procedures

### Kill All Orders Immediately
```bash
# Via MCP (preferred — doesn't require stopping bot):
# Use cancel_order tool for specific orders
# Or stop the bot — DMS will cancel all orders within 60s

# Via bot: just stop the process, DMS fires automatically
```

### Emergency Liquidation
```bash
# Via MCP:
# market_sell tool with pair name (e.g., "BTC/USD")
# Sells full balance at market immediately
```

### Bot Won't Start
1. Check proxy is reachable: `curl http://10.255.255.254:8080/health`
2. Check binary exists: `ls -la target/release/kraken-mm`
3. Check env vars are set (especially PROXY_MODE, PROXY_URL, PROXY_TOKEN)
4. Check state.json is valid JSON: `python3 -m json.tool state.json`
