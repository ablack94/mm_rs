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

use kraken_core::proxy::rest_proxy::{build_proxy_router, ProxyState};
use kraken_core::proxy::ws_proxy::{handle_public_ws_connection, handle_ws_connection, WsProxyState};

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
    let rest_state = Arc::new(ProxyState {
        api_key: api_key.clone(),
        api_secret: api_secret.clone(),
        proxy_token: proxy_token.clone(),
        kraken_base_url: kraken_base_url.clone(),
        client: reqwest::Client::new(),
    });

    // WS proxy state
    let ws_state = Arc::new(WsProxyState::new(
        api_key,
        api_secret,
        kraken_ws_url,
        kraken_base_url,
    ));

    // Build combined router
    let rest_router = build_proxy_router(rest_state);
    let ws_token = proxy_token;

    let ws_token_pub = ws_token.clone();
    let ws_router = Router::new()
        .route(
            "/ws/private",
            get(
                move |headers: HeaderMap,
                      ws: WebSocketUpgrade,
                      State(state): State<Arc<WsProxyState>>| async move {
                    // Check bearer token
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");

                    // Also accept token via Sec-WebSocket-Protocol for clients
                    // that can't set Authorization headers on WS upgrades.
                    let token_from_protocol = headers
                        .get("sec-websocket-protocol")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");

                    if auth != format!("Bearer {}", ws_token)
                        && token_from_protocol != ws_token
                    {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }

                    ws.on_upgrade(move |socket| handle_ws_connection(socket, state))
                        .into_response()
                },
            ),
        )
        .route(
            "/ws/public",
            get(
                move |headers: HeaderMap,
                      ws: WebSocketUpgrade| async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    let token_from_protocol = headers
                        .get("sec-websocket-protocol")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");

                    if auth != format!("Bearer {}", ws_token_pub)
                        && token_from_protocol != ws_token_pub
                    {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }

                    let url = kraken_public_ws_url.clone();
                    ws.on_upgrade(move |socket| handle_public_ws_connection(socket, url))
                        .into_response()
                },
            ),
        )
        .with_state(ws_state);

    let app = rest_router.merge(ws_router);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", args.port)).await?;
    tracing::info!(port = args.port, "Proxy listening");

    axum::serve(listener, app).await?;
    Ok(())
}
