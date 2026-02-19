mod auth;
mod rest;
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

use crate::rest::{KrakenRestState, build_kraken_rest_router};
use crate::ws::{KrakenWsState, handle_kraken_private_ws, handle_kraken_public_ws};

#[derive(Parser)]
#[command(name = "kraken-proxy", about = "Kraken API proxy — holds keys, signs requests")]
struct Args {
    /// Port to listen on
    #[arg(long, default_value = "8080")]
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

    let api_key = std::env::var("KRAKEN_API_KEY")
        .expect("KRAKEN_API_KEY environment variable required");
    let api_secret = std::env::var("KRAKEN_API_SECRET")
        .expect("KRAKEN_API_SECRET environment variable required");
    let proxy_token = std::env::var("PROXY_TOKEN")
        .expect("PROXY_TOKEN environment variable required");

    let kraken_base_url = std::env::var("KRAKEN_REST_URL")
        .unwrap_or_else(|_| "https://api.kraken.com".to_string());
    let kraken_ws_url = std::env::var("KRAKEN_WS_URL")
        .unwrap_or_else(|_| "wss://ws-auth.kraken.com/v2".to_string());
    let kraken_public_ws_url = std::env::var("KRAKEN_PUBLIC_WS_URL")
        .unwrap_or_else(|_| "wss://ws.kraken.com/v2".to_string());

    tracing::info!(
        port = args.port,
        kraken_rest = kraken_base_url,
        kraken_ws = kraken_ws_url,
        "Starting Kraken proxy"
    );

    // REST proxy state
    let rest_state = Arc::new(KrakenRestState {
        api_key: api_key.clone(),
        api_secret: api_secret.clone(),
        proxy_token: proxy_token.clone(),
        kraken_base_url: kraken_base_url.clone(),
        client: reqwest::Client::new(),
    });

    // Rate limit config from env vars
    let rate_limit_config = TokenBucketConfig {
        max_tokens: std::env::var("RATE_LIMIT_MAX_TOKENS")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(40.0),
        refill_rate: std::env::var("RATE_LIMIT_REFILL_RATE")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(1.0),
        order_cost: std::env::var("RATE_LIMIT_ORDER_COST")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(2.0),
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
    let ws_state = Arc::new(KrakenWsState::new(
        api_key,
        api_secret,
        kraken_ws_url,
        kraken_base_url,
        rate_limit_config,
    ));

    // Build combined router
    let rest_router = build_kraken_rest_router(rest_state);
    let ws_token = proxy_token;

    let ws_token_pub = ws_token.clone();
    let ws_router = Router::new()
        .route(
            "/ws/private",
            get(
                move |headers: HeaderMap,
                      ws: WebSocketUpgrade,
                      State(state): State<Arc<KrakenWsState>>| async move {
                    if !check_ws_auth(&headers, &ws_token) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    ws.on_upgrade(move |socket| handle_kraken_private_ws(socket, state))
                        .into_response()
                },
            ),
        )
        .route(
            "/ws/public",
            get(
                move |headers: HeaderMap,
                      ws: WebSocketUpgrade| async move {
                    if !check_ws_auth(&headers, &ws_token_pub) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    let url = kraken_public_ws_url.clone();
                    ws.on_upgrade(move |socket| handle_kraken_public_ws(socket, url))
                        .into_response()
                },
            ),
        )
        .with_state(ws_state);

    let app = rest_router.merge(ws_router);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", args.port)).await?;
    tracing::info!(port = args.port, "Kraken proxy listening");

    axum::serve(listener, app).await?;
    Ok(())
}
