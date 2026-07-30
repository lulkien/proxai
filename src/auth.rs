use crate::key_manager::KeyManager;
use axum::{
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;

#[derive(Clone)]
pub struct AuthState {
    pub key_manager: Arc<KeyManager>,
}

/// Axum middleware that requires a valid API key.
pub async fn require_api_key(
    State(state): State<AuthState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let auth_header = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match auth_header {
        Some(key) => match state.key_manager.validate(key) {
            Ok(true) => next.run(request).await,
            Ok(false) => unauthorized("invalid api key"),
            Err(e) => {
                tracing::error!("Key validation error: {e}");
                internal_error("key validation failed")
            }
        },
        None => unauthorized("missing api key"),
    }
}

fn unauthorized(msg: &str) -> Response {
    let body = serde_json::json!({
        "error": {
            "message": msg,
            "type": "authentication_error",
            "code": 401,
        }
    });
    (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response()
}

fn internal_error(msg: &str) -> Response {
    let body = serde_json::json!({
        "error": {
            "message": msg,
            "type": "proxy_error",
            "code": 500,
        }
    });
    (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(body)).into_response()
}
