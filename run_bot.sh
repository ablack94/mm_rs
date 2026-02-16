#!/bin/bash
export PROXY_MODE=true
export PROXY_URL="http://10.255.255.254:8080"
export PROXY_TOKEN="0ba154d4e4886b262f7752ebcb5213a57ea8161b88c4b5f4eed5b0f79363d7ab"
export BOT_API_TOKEN="mcp-bot-token-2026"
export BOT_API_PORT="3030"
export RUST_LOG=info

cd /workarea/kraken_mm_rs
exec ./target/release/kraken-mm 2>&1 | tee -a /workarea/kraken_mm_rs/bot.log
