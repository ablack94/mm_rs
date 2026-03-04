#!/bin/bash
SS_TOKEN="a1fb58b5829e886cce2fa70516ac3495477834e025d8d91612584a82818acc69"
SS_URL="http://localhost:3040"
BOT_PID=311221
PROXY_PID=310494

while true; do
    echo "=== $(date -u '+%Y-%m-%d %H:%M:%S UTC') ==="
    if ! kill -0 $BOT_PID 2>/dev/null; then echo "ALERT: Bot DEAD"; fi
    if ! kill -0 $PROXY_PID 2>/dev/null; then echo "ALERT: Proxy DEAD"; fi
    HEALTH=$(curl -s -m 3 -H "Authorization: Bearer scanner" http://localhost:8082/health 2>/dev/null)
    if [ "$HEALTH" != '{"status":"ok"}' ]; then echo "ALERT: Proxy health FAILED"; fi
    PAIRS=$(curl -s -m 3 -H "Authorization: Bearer $SS_TOKEN" "$SS_URL/pairs" 2>/dev/null)
    if [ -n "$PAIRS" ]; then
        echo "$PAIRS" | python3 -c "
import sys,json
d=json.load(sys.stdin)
a=[p for p in d.get('pairs',[]) if p['state']=='active']
w=[p for p in d.get('pairs',[]) if p['state']=='wind_down']
l=[p for p in d.get('pairs',[]) if p['state']=='liquidating']
print(f'  Pairs: {len(a)} active, {len(w)} winddown, {len(l)} liquidating')
for p in w: print(f'    WINDDOWN: {p[\"symbol\"]}')
for p in l: print(f'    LIQUIDATING: {p[\"symbol\"]}')
" 2>/dev/null
    fi
    if [ -f /workarea/pnl_state.json ]; then
        python3 -c "
import json
with open('/workarea/pnl_state.json') as f: d=json.load(f)
pnl=float(d.get('realized_pnl',0)); fees=float(d.get('total_fees',0)); trades=d.get('trade_count',0)
pos=d.get('positions',{})
ps=', '.join(f'{k}:{v[\"qty\"]}' for k,v in pos.items()) if pos else 'none'
print(f'  PnL: \${pnl:.4f} | Fees: \${fees:.4f} | Trades: {trades}')
print(f'  Positions: {ps}')
" 2>/dev/null
    fi
    if [ -f /workarea/logs/trades.csv ]; then
        LAST=$(tail -1 /workarea/logs/trades.csv 2>/dev/null)
        if echo "$LAST" | grep -q ",20"; then
            echo "  Last: $(echo $LAST | cut -d, -f1-3,8)"
        fi
    fi
    echo ""
    sleep 30
done
