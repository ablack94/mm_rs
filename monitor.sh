#!/bin/bash
# Market-open bot launcher + PnL monitor
# Starts the bot at the target UTC time, then monitors session PnL.
# Kills the bot if session losses exceed the threshold.

set -euo pipefail

TARGET_HOUR_UTC=${TARGET_HOUR_UTC:-11}  # 6 AM ET = 11 UTC
TARGET_MIN_UTC=${TARGET_MIN_UTC:-0}
SESSION_LOSS_LIMIT=100  # dollars (positive number, triggers on -$100)
POLL_INTERVAL=30        # seconds between PnL checks

# Service config
PROXY_URL="http://localhost:8080"
PROXY_TOKEN="367f1923618511fe5814aa774b87f462e463326aa4e7298e1c1983315f6b1120"
STATE_STORE_URL="http://localhost:3040"
STATE_STORE_TOKEN="a1fb58b5829e886cce2fa70516ac3495477834e025d8d91612584a82818acc69"
PNL_API_URL="http://localhost:3031"
PNL_API_TOKEN="ed0cdf16290995c909431ebdb595f452db1e937d146f94efed8827da5b254a5d"
BOT_BINARY="/workarea/target/release/kraken-mm"
SESSION_FILE="/workarea/session.json"

# --- Phase 1: Wait for market open ---
now_epoch=$(date -u +%s)
target_today=$(date -u -d "$(date -u +%Y-%m-%d) ${TARGET_HOUR_UTC}:${TARGET_MIN_UTC}:00" +%s)

if [ "$now_epoch" -ge "$target_today" ]; then
    echo "[$(date -u +%H:%M:%S)] Target time already passed today. Starting immediately."
    sleep_secs=0
else
    sleep_secs=$((target_today - now_epoch))
    hours=$((sleep_secs / 3600))
    mins=$(( (sleep_secs % 3600) / 60 ))
    echo "[$(date -u +%H:%M:%S)] Waiting ${hours}h ${mins}m until $(date -u -d @${target_today} +%H:%M) UTC (6:00 AM ET)..."
    sleep "$sleep_secs"
fi

# --- Phase 2: Record baseline PnL ---
echo "[$(date -u +%H:%M:%S)] Market open! Capturing baseline PnL..."
if [ -n "${BASELINE_OVERRIDE:-}" ]; then
    baseline_pnl="$BASELINE_OVERRIDE"
    echo "[$(date -u +%H:%M:%S)] Using overridden baseline PnL: \$${baseline_pnl}"
else
    baseline_pnl=$(curl -s -H "Authorization: Bearer ${PNL_API_TOKEN}" "${PNL_API_URL}/pnl" | \
        python3 -c "import sys,json; d=json.load(sys.stdin); print(float(d.get('pnl',{}).get('net_pnl',0)) + float(d.get('pnl',{}).get('unrealized_pnl',0)))" 2>/dev/null || echo "0")
fi
echo "[$(date -u +%H:%M:%S)] Baseline net PnL: \$${baseline_pnl}"

# Write initial session file
SESSION_STARTED_AT="${SESSION_STARTED_AT:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"
python3 -c "
import json, time
json.dump({
    'baseline_pnl': ${baseline_pnl},
    'session_pnl': 0.0,
    'session_loss_limit': ${SESSION_LOSS_LIMIT},
    'started_at': '${SESSION_STARTED_AT}',
    'updated_at': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
}, open('${SESSION_FILE}', 'w'))
"

# --- Phase 3: Start the bot ---
echo "[$(date -u +%H:%M:%S)] Starting bot..."
PROXY_URL="$PROXY_URL" \
PROXY_TOKEN="$PROXY_TOKEN" \
STATE_STORE_URL="$STATE_STORE_URL" \
STATE_STORE_TOKEN="$STATE_STORE_TOKEN" \
RUST_LOG=info \
"$BOT_BINARY" &
BOT_PID=$!
echo "[$(date -u +%H:%M:%S)] Bot started (PID: ${BOT_PID})"

# Ensure bot is killed on script exit
trap "echo '[$(date -u +%H:%M:%S)] Script exiting — killing bot (PID: ${BOT_PID})'; kill $BOT_PID 2>/dev/null; wait $BOT_PID 2>/dev/null" EXIT

# Give bot time to connect and start quoting
sleep 10

# --- Phase 4: Monitor PnL ---
echo "[$(date -u +%H:%M:%S)] Monitoring session PnL (limit: -\$${SESSION_LOSS_LIMIT})..."
echo "---"

while kill -0 "$BOT_PID" 2>/dev/null; do
    pnl_json=$(curl -s -H "Authorization: Bearer ${PNL_API_TOKEN}" "${PNL_API_URL}/pnl" 2>/dev/null || echo "{}")

    current_pnl=$(echo "$pnl_json" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    p = d.get('pnl', {})
    realized = float(p.get('realized_pnl', 0))
    unrealized = float(p.get('unrealized_pnl', 0))
    fees = float(p.get('total_fees', 0))
    trades = int(p.get('trade_count', 0))
    net = realized + unrealized
    print(f'{net}|{realized}|{unrealized}|{fees}|{trades}')
except:
    print('0|0|0|0|0')
" 2>/dev/null)

    net=$(echo "$current_pnl" | cut -d'|' -f1)
    realized=$(echo "$current_pnl" | cut -d'|' -f2)
    unrealized=$(echo "$current_pnl" | cut -d'|' -f3)
    fees=$(echo "$current_pnl" | cut -d'|' -f4)
    trades=$(echo "$current_pnl" | cut -d'|' -f5)

    session_pnl=$(python3 -c "print(round(${net} - ${baseline_pnl}, 2))" 2>/dev/null || echo "0")

    # Update session file for dashboard
    python3 -c "
import json, time
json.dump({
    'baseline_pnl': ${baseline_pnl},
    'session_pnl': ${session_pnl},
    'session_loss_limit': ${SESSION_LOSS_LIMIT},
    'started_at': json.load(open('${SESSION_FILE}')).get('started_at', ''),
    'updated_at': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
}, open('${SESSION_FILE}', 'w'))
" 2>/dev/null || true

    echo "[$(date -u +%H:%M:%S)] Session P&L: \$${session_pnl} | Net: \$$(python3 -c "print(round(${net},2))") | Realized: \$$(python3 -c "print(round(${realized},2))") | Unrealized: \$$(python3 -c "print(round(${unrealized},2))") | Fees: \$${fees} | Trades: ${trades}"

    # PnL staleness check: warn if last_fill_time is >5 min old
    last_fill_time=$(echo "$pnl_json" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    lft = d.get('last_fill_time')
    if lft:
        from datetime import datetime, timezone
        ts = datetime.fromisoformat(lft.replace('Z', '+00:00'))
        age_secs = (datetime.now(timezone.utc) - ts).total_seconds()
        if age_secs > 300:
            print(f'STALE:{int(age_secs // 60)}')
        else:
            print('OK')
    else:
        print('NONE')
except:
    print('ERR')
" 2>/dev/null || echo "ERR")

    if echo "$last_fill_time" | grep -q "^STALE:"; then
        stale_mins=$(echo "$last_fill_time" | cut -d: -f2)
        echo "[$(date -u +%H:%M:%S)] WARNING: PnL data stale — last fill was ${stale_mins} minutes ago"
    fi

    # Check if session loss exceeds limit
    is_blown=$(python3 -c "print('yes' if ${session_pnl} < -${SESSION_LOSS_LIMIT} else 'no')" 2>/dev/null || echo "no")
    if [ "$is_blown" = "yes" ]; then
        echo ""
        echo "!!! SESSION LOSS LIMIT HIT: \$${session_pnl} exceeds -\$${SESSION_LOSS_LIMIT} !!!"
        echo "[$(date -u +%H:%M:%S)] Killing bot..."
        kill "$BOT_PID" 2>/dev/null
        wait "$BOT_PID" 2>/dev/null || true
        echo "[$(date -u +%H:%M:%S)] Bot stopped. Final session P&L: \$${session_pnl}"
        trap - EXIT  # Clear the trap since we handled it
        exit 1
    fi

    sleep "$POLL_INTERVAL"
done

echo "[$(date -u +%H:%M:%S)] Bot process exited on its own."
trap - EXIT
