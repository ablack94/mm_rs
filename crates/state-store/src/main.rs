mod auth;
mod handlers;
mod state;
mod store;
mod types;
mod ws;

use std::sync::Arc;
use tokio::sync::RwLock;

use axum::{
    extract::{Query, State, WebSocketUpgrade},
    middleware,
    response::IntoResponse,
    routing::{delete, get, patch, put},
    Extension, Router,
};
use tracing::{error, info};

use auth::AuthToken;
use state::{AppState, SharedState};
use store::{JsonFileStore, Store};
use types::WsQuery;

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("state_store=info")),
        )
        .init();

    // Read configuration from environment
    let port: u16 = std::env::var("STATE_STORE_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3040);

    let token = match std::env::var("STATE_STORE_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => {
            error!("STATE_STORE_TOKEN environment variable is required");
            std::process::exit(1);
        }
    };

    let file_path = std::env::var("STATE_STORE_FILE").unwrap_or_else(|_| "state_store.json".into());

    info!(port, file = %file_path, "starting state store");

    // Load persisted state
    let file_store = JsonFileStore::new(&file_path);
    let store_data = match file_store.load().await {
        Ok(d) => d,
        Err(e) => {
            error!(error = %e, "failed to load state file");
            std::process::exit(1);
        }
    };

    info!(
        pairs = store_data.pairs.len(),
        "state loaded successfully"
    );

    // Build shared state
    let (app_state, _initial_rx) = AppState::new(store_data, Box::new(file_store));
    let shared_state: SharedState = Arc::new(RwLock::new(app_state));

    // Build the router
    let token_for_ws = token.clone();

    let app = Router::new()
        // REST endpoints
        .route("/health", get(handlers::health))
        .route("/pairs", get(handlers::list_pairs))
        .route("/pairs/{symbol}", get(handlers::get_pair))
        .route("/pairs/{symbol}", put(handlers::put_pair))
        .route("/pairs/{symbol}", patch(handlers::patch_pair))
        .route("/pairs/{symbol}", delete(handlers::delete_pair))
        .route("/defaults", get(handlers::get_defaults))
        .route("/defaults", patch(handlers::patch_defaults))
        // WebSocket endpoint (auth handled inside handler via query param)
        .route(
            "/ws",
            get(
                move |State(state): State<SharedState>,
                      Query(query): Query<WsQuery>,
                      ws: WebSocketUpgrade| async move {
                    ws_upgrade_handler(state, query, ws, token_for_ws).await
                },
            ),
        )
        // Auth middleware (applied to all routes, WS skipped internally)
        .layer(middleware::from_fn(auth::auth_middleware))
        // Inject the auth token as an extension
        .layer(Extension(AuthToken(token)))
        .with_state(shared_state);

    // Start the server
    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    info!(address = %addr, "state store listening");

    axum::serve(listener, app).await.unwrap();
}

/// Wrapper for WebSocket upgrade that handles auth.
async fn ws_upgrade_handler(
    state: SharedState,
    query: WsQuery,
    ws_upgrade: WebSocketUpgrade,
    expected_token: String,
) -> impl IntoResponse {
    ws::ws_upgrade(
        State(state),
        Query(query),
        ws_upgrade,
        expected_token,
    )
    .await
}
