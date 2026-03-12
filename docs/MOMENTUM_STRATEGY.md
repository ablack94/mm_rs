# Momentum / Trend-Following Strategy — Design Notes

## Core Idea
Identify clear short-term trends on hyper-liquid pairs (BTC/USDC, ETH/USDC) and take large directional positions ($50k-$100k) to ride the wave. Primary goal: profit. Secondary goal: farm volume for fee tier improvement.

## Two Modes

### Long Mode (uptrend)
- Detect momentum via order flow imbalance + book pressure + volume surge
- Buy large position at best bid (maker order)
- Tight TP (+0.3%) and SL (-0.15%)
- Exit via limit sell at TP or market sell at SL

### Short Mode (downtrend)
- Borrow ETH against 35 ETH collateral (on Coinbase or DeFi)
- Detect downward momentum (same signals, inverted)
- Sell large position → buy back lower
- Same tight brackets, inverted
- "Easy money on tumble days" — flash crashes and cascading liquidations create predictable momentum

## Fee Math at Current Tier (0.09375% maker / 0.125% taker)

| Trade Size | Maker RT Cost | Taker RT Cost | Breakeven Move |
|-----------|--------------|--------------|----------------|
| $10,000   | $18.75       | $25.00       | 0.019% / 0.025% |
| $50,000   | $93.75       | $125.00      | 0.019% / 0.025% |
| $100,000  | $187.50      | $250.00      | 0.019% / 0.025% |

Breakeven is the same percentage regardless of size. The question is purely about edge quality.

## Entry Signals (Better Than EMA Crossover)

### 1. Order Flow Imbalance (OFI)
- Track buy vs sell volume over rolling 30s/60s/5min windows
- Signal: buy volume > 3x sell volume (or vice versa for shorts)
- This is the strongest short-term predictor — institutional flow shows up here first

### 2. Book Pressure Ratio
- Compute: sum(bid_depth_top_5) / sum(ask_depth_top_5)
- Ratio > 2.0 = strong buy pressure (bids stacking, asks thin)
- Ratio < 0.5 = strong sell pressure
- Changes in this ratio over time matter more than absolute level

### 3. VWAP Breakout
- Track volume-weighted average price over 5min/15min
- Price crossing above VWAP with increasing volume = confirmed uptrend
- Price breaking below VWAP with volume = confirmed downtrend

### 4. Volatility Regime
- Only trade when realized vol is in the "sweet spot" — enough movement to profit, not so much that stops get blown by noise
- Bollinger band width or ATR as filter

### Composite Signal
Require 2+ of the above to align before entering. Reduces false signals dramatically.

## Risk Management

- Max position: $100k (or configurable)
- Stop loss: 0.15% ($150 on $100k) — tight, accept frequent small losses
- Take profit: 0.3% ($300 on $100k) — 2:1 R:R target
- Max daily loss: $500 — kill switch, walk away
- No holding through major news events (FOMC, CPI releases)
- Position timeout: exit after 5 min regardless (momentum trades shouldn't take long)

## Expected Performance

With a 55% win rate and 2:1 R:R (maker fees):
- Win: +$300 - $94 = +$206
- Loss: -$150 - $94 = -$244
- EV per trade: 0.55 * $206 - 0.45 * $244 = +$3.40

With 60% win rate:
- EV per trade: 0.60 * $206 - 0.40 * $244 = +$26.00
- 10 trades/day = +$260/day

The edge is thin at 55% but real at 60%. The signal quality determines everything.

## Short Selling via ETH Collateral

### Setup
- 35 ETH as collateral (~$70k at current prices)
- Borrow ETH against it (Aave, Compound, or Coinbase margin if available)
- Sell borrowed ETH → buy back cheaper → return loan + keep difference

### When to Short
- Market-wide risk-off days (correlates with equity selloffs)
- After failed breakout attempts (double top patterns)
- Cascading liquidation events (watch DeFi liquidation dashboards)
- Negative funding rates on perps (signals crowded longs)

### Risk
- Liquidation risk on collateral if ETH pumps while short
- Borrow rate eats into profits on longer holds
- Best for intraday only — don't hold shorts overnight unless conviction is very high

## Volume Farming Angle

Even if strategy is breakeven on P&L, the volume generated improves Coinbase fee tier:
- $100k * 10 trades/day = $1M daily volume
- Monthly: $30M → likely qualifies for significantly lower fee tier
- Lower fees → the same strategy becomes more profitable (virtuous cycle)

## Implementation Plan

### Phase 1: Data Collection
- Log order flow, book pressure, VWAP for BTC/ETH for 1 week
- Backtest signal combinations against actual price moves
- Determine realistic win rate

### Phase 2: Paper Trading
- Run the bot with logging but no real orders
- Track hypothetical P&L for 1 week
- Validate signal quality

### Phase 3: Small Scale Live
- Start with $5k-$10k positions
- Validate fill quality, slippage, actual costs
- Run for 1-2 weeks

### Phase 4: Scale Up
- Increase to $50k-$100k positions
- Add short mode with ETH collateral
- Automate everything

## Technical Requirements
- Sub-second book data processing (already have via proxy WS)
- Order flow tracking (need to add trade tape subscription)
- Fast order placement (already have via proxy private WS)
- Kill switch and position limits (critical at this scale)
