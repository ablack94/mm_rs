use axum::{
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};

use crate::types::ErrorResponse;

/// Bearer token auth middleware.
/// Validates the `Authorization: Bearer <token>` header on every request.
/// The WS endpoint handles its own auth via query param, so it skips this.
pub async fn auth_middleware(request: Request, next: Next) -> Response {
    // Skip auth for WebSocket upgrade path (it uses ?token= query param)
    if request.uri().path() == "/ws" {
        return next.run(request).await;
    }

    let expected_token = match request.extensions().get::<AuthToken>() {
        Some(t) => t.0.clone(),
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: "auth not configured".to_string(),
                }),
            )
                .into_response();
        }
    };

    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let provided_token = auth_header.strip_prefix("Bearer ").unwrap_or("");

    if provided_token != expected_token {
        return (
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".to_string(),
            }),
        )
            .into_response();
    }

    next.run(request).await
}

/// Extension type to carry the expected token through middleware.
#[derive(Clone)]
pub struct AuthToken(pub String);
