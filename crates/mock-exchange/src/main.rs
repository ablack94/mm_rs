mod config;
mod messages;
mod orderbook;
mod orders;
mod rest;
pub mod scenario;
mod simulation;
mod state;
mod ws_private;
mod ws_public;

use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use axum::{
    extract::{State, WebSocketUpgrade},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Router,
};

use config::MockConfig;
use state::ExchangeState;

/// Shared application state passed to all handlers.
#[derive(Clone)]
struct AppState {
    exchange: Arc<RwLock<ExchangeState>>,
    book_tx: broadcast::Sender<String>,
    exec_tx: broadcast::Sender<String>,
}

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("mock_exchange=info")),
        )
        .init();

    let config = MockConfig::from_env();
    let port = config.port;

    tracing::info!(
        port = port,
        pairs = ?config.pairs.iter().map(|p| format!("{}@{}", p.symbol, p.starting_price)).collect::<Vec<_>>(),
        spread_pct = config.spread_pct,
        volatility = config.volatility,
        fill_probability = config.fill_probability,
        update_interval_ms = config.update_interval_ms,
        starting_usd = config.starting_usd,
        seed = ?config.seed,
        "Starting mock exchange"
    );

    let exchange_state = Arc::new(RwLock::new(ExchangeState::new(config)));

    // Broadcast channels: book updates and execution reports
    let (book_tx, _) = broadcast::channel::<String>(1024);
    let (exec_tx, _) = broadcast::channel::<String>(1024);

    let app_state = AppState {
        exchange: exchange_state.clone(),
        book_tx: book_tx.clone(),
        exec_tx: exec_tx.clone(),
    };

    // Spawn the simulation loop
    let sim_state = exchange_state.clone();
    let sim_book_tx = book_tx.clone();
    let sim_exec_tx = exec_tx.clone();
    tokio::spawn(async move {
        simulation::run_simulation(sim_state, sim_book_tx, sim_exec_tx).await;
    });

    // Build the router
    let app = Router::new()
        // REST endpoints
        .route("/health", get(rest::health))
        .route(
            "/0/private/GetWebSocketsToken",
            post(rest::get_ws_token),
        )
        .route(
            "/0/private/Balance",
            post(handle_balance),
        )
        .route("/0/public/AssetPairs", get(handle_asset_pairs))
        .route("/0/public/Ticker", get(handle_ticker))
        // Public WebSocket (book data)
        .route("/v2", get(ws_public_upgrade))
        .route("/ws/public", get(ws_public_upgrade))
        // Private WebSocket (executions + orders)
        .route("/ws/private", get(ws_private_upgrade))
        .with_state(app_state);

    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    tracing::info!(address = %addr, "Mock exchange listening");

    axum::serve(listener, app).await.unwrap();
}

// ─── REST handler wrappers (to pass SharedState from AppState) ───

async fn handle_balance(
    State(app): State<AppState>,
) -> (StatusCode, axum::response::Json<serde_json::Value>) {
    rest::get_balance(State(app.exchange)).await
}

async fn handle_asset_pairs(
    State(app): State<AppState>,
) -> (StatusCode, axum::response::Json<serde_json::Value>) {
    rest::get_asset_pairs(State(app.exchange)).await
}

async fn handle_ticker(
    State(app): State<AppState>,
    query: axum::extract::Query<rest::PairQuery>,
) -> (StatusCode, axum::response::Json<serde_json::Value>) {
    rest::get_ticker(State(app.exchange), query).await
}

// ─── WebSocket upgrade handlers ───

async fn ws_public_upgrade(
    State(app): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let state = app.exchange.clone();
    let book_rx = app.book_tx.subscribe();
    ws.on_upgrade(move |socket| ws_public::handle_public_ws(socket, state, book_rx))
}

async fn ws_private_upgrade(
    headers: HeaderMap,
    State(app): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // Accept any bearer token (it's a mock)
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !auth.is_empty() {
        tracing::debug!(auth, "Private WS auth accepted");
    }

    let state = app.exchange.clone();
    let exec_rx = app.exec_tx.subscribe();
    ws.on_upgrade(move |socket| ws_private::handle_private_ws(socket, state, exec_rx))
}
