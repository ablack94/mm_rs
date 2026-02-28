mod auth;
mod pairs;
mod rest;
mod translate;
mod ws;

use anyhow::Result;
use axum::{
    Router,
    extract::{State, WebSocketUpgrade},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
};
use clap::Parser;
use std::sync::Arc;

use proxy_common::auth::check_ws_auth;
use proxy_common::rate_limit::TokenBucketConfig;

use crate::rest::{CoinbaseRestState, build_coinbase_rest_router};
use crate::ws::{CoinbaseWsState, handle_coinbase_private_ws, handle_coinbase_public_ws};

#[derive(Parser)]
#[command(name = "coinbase-proxy", about = "Coinbase API proxy — holds keys, signs requests, translates protocol")]
struct Args {
    /// Port to listen on
    #[arg(long, default_value = "8081")]
    port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),
        )
        .init();

    let api_key = std::env::var("COINBASE_API_KEY")
        .expect("COINBASE_API_KEY environment variable required");
    let api_secret = std::env::var("COINBASE_API_SECRET")
        .expect("COINBASE_API_SECRET environment variable required");
    let proxy_token = std::env::var("PROXY_TOKEN")
        .expect("PROXY_TOKEN environment variable required");

    let coinbase_base_url = std::env::var("COINBASE_REST_URL")
        .unwrap_or_else(|_| "https://api.coinbase.com".to_string());

    tracing::info!(
        port = args.port,
        coinbase_rest = coinbase_base_url,
        "Starting Coinbase proxy"
    );

    // Fee configuration (Coinbase volume-tier dependent)
    let maker_fee_pct: f64 = std::env::var("COINBASE_MAKER_FEE")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(0.006);
    let taker_fee_pct: f64 = std::env::var("COINBASE_TAKER_FEE")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(0.012);

    tracing::info!(maker_fee_pct, taker_fee_pct, "Fee config");

    // REST proxy state
    let rest_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("Failed to build HTTP client");
    let rest_state = Arc::new(CoinbaseRestState {
        api_key: api_key.clone(),
        api_secret: api_secret.clone(),
        proxy_token: proxy_token.clone(),
        coinbase_base_url: coinbase_base_url.clone(),
        client: rest_client,
        maker_fee_pct,
        taker_fee_pct,
    });

    // Rate limit config
    let rate_limit_config = TokenBucketConfig {
        max_tokens: std::env::var("RATE_LIMIT_MAX_TOKENS")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(30.0),
        refill_rate: std::env::var("RATE_LIMIT_REFILL_RATE")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(5.0),
        order_cost: std::env::var("RATE_LIMIT_ORDER_COST")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(1.0),
        cancel_cost: std::env::var("RATE_LIMIT_CANCEL_COST")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(1.0),
    };

    tracing::info!(
        max_tokens = rate_limit_config.max_tokens,
        refill_rate = rate_limit_config.refill_rate,
        order_cost = rate_limit_config.order_cost,
        cancel_cost = rate_limit_config.cancel_cost,
        "Rate limit config"
    );

    // WS proxy state
    let ws_state = Arc::new(CoinbaseWsState::new(
        api_key,
        api_secret,
        coinbase_base_url,
        rate_limit_config,
    ));

    // Build combined router
    let rest_router = build_coinbase_rest_router(rest_state);
    let ws_token = proxy_token;

    let ws_token_pub = ws_token.clone();
    let ws_state_pub = ws_state.clone();
    let ws_router = Router::new()
        .route(
            "/ws/private",
            get(
                move |headers: HeaderMap,
                      ws: WebSocketUpgrade,
                      State(state): State<Arc<CoinbaseWsState>>| async move {
                    if !check_ws_auth(&headers, &ws_token) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    ws.on_upgrade(move |socket| handle_coinbase_private_ws(socket, state))
                        .into_response()
                },
            ),
        )
        .with_state(ws_state);

    let pub_ws_router = Router::new()
        .route(
            "/ws/public",
            get(
                move |headers: HeaderMap,
                      ws: WebSocketUpgrade,
                      State(state): State<Arc<CoinbaseWsState>>| async move {
                    if !check_ws_auth(&headers, &ws_token_pub) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    ws.on_upgrade(move |socket| handle_coinbase_public_ws(socket, state))
                        .into_response()
                },
            ),
        )
        .with_state(ws_state_pub);

    let app = rest_router.merge(ws_router).merge(pub_ws_router);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", args.port)).await?;
    tracing::info!(port = args.port, "Coinbase proxy listening");

    axum::serve(listener, app).await?;
    Ok(())
}
