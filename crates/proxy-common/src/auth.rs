use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use serde_json::{json, Value};

/// Check bearer token from Authorization header.
pub fn check_auth(headers: &HeaderMap, token: &str) -> Result<(), (StatusCode, Json<Value>)> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if auth == format!("Bearer {}", token) {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        ))
    }
}

/// Check bearer token from either Authorization header or Sec-WebSocket-Protocol header.
/// Returns true if authenticated.
pub fn check_ws_auth(headers: &HeaderMap, token: &str) -> bool {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let token_from_protocol = headers
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    auth == format!("Bearer {}", token) || token_from_protocol == token
}
