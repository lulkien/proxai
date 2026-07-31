use crate::key_manager::KeyManager;
use axum::{
    extract::{ConnectInfo, State},
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Instant};
use tokio::sync::Mutex;

/// Injected into request extensions by auth middleware so handlers can
/// track usage per-key without re-looking up the key name.
#[derive(Clone, Debug)]
pub struct AuthInfo {
    pub key_hash: String,
    pub key_name: String,
}

#[derive(Clone)]
pub struct AuthState {
    pub key_manager: Arc<KeyManager>,
}

/// In-memory rate limiter for failed auth attempts per IP.
#[derive(Clone, Default)]
struct RateLimiter {
    attempts: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
}

impl RateLimiter {
    /// Check if an IP is rate-limited. Returns true if blocked.
    async fn check(&self, ip: &str) -> bool {
        let mut map = self.attempts.lock().await;
        let now = Instant::now();
        let window = std::time::Duration::from_secs(60);
        let max_failures: usize = 20;

        let entry = map.entry(ip.to_string()).or_default();
        entry.retain(|t| now.duration_since(*t) < window);

        if entry.len() >= max_failures {
            return true; // blocked
        }
        entry.push(now);
        false
    }

    /// Reset rate limit for an IP (called on successful auth).
    async fn reset(&self, ip: &str) {
        let mut map = self.attempts.lock().await;
        map.entry(ip.to_string()).or_default().clear();
    }
}

/// Axum middleware that requires a valid API key with rate limiting.
pub async fn require_api_key(
    State(state): State<AuthState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let ip = addr.ip().to_string();

    static RATE_LIMITER: std::sync::LazyLock<RateLimiter> =
        std::sync::LazyLock::new(RateLimiter::default);

    let auth_header = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string());

    match auth_header {
        Some(key) => match state.key_manager.validate(&key) {
            Ok(true) => {
                RATE_LIMITER.reset(&ip).await;
                let hash = crate::key_manager::hash_key(&key);
                let name = state
                    .key_manager
                    .lookup_name(&key)
                    .unwrap_or_else(|| format!("key-{}", &hash[..hash.len().min(8)]));
                request.extensions_mut().insert(AuthInfo {
                    key_hash: hash,
                    key_name: name,
                });
                next.run(request).await
            }
            Ok(false) => {
                if RATE_LIMITER.check(&ip).await {
                    return rate_limited();
                }
                unauthorized("invalid api key")
            }
            Err(e) => {
                tracing::error!("Key validation error: {e}");
                internal_error("key validation failed")
            }
        },
        None => {
            if RATE_LIMITER.check(&ip).await {
                return rate_limited();
            }
            unauthorized("missing api key")
        }
    }
}

fn rate_limited() -> Response {
    let body = serde_json::json!({
        "error": {
            "message": "rate limited — too many failed attempts, try again later",
            "type": "rate_limit_error",
            "code": 429,
        }
    });
    (StatusCode::TOO_MANY_REQUESTS, axum::Json(body)).into_response()
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
