use crate::{key_manager::KeyManager, metrics::UsageTracker};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    routing::get,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Returns an API-only router (used at /dashboard/api).
pub fn dashboard_api_router(
    tracker: Arc<UsageTracker>,
    key_manager: Arc<KeyManager>,
    dashboard_password: &Option<String>,
) -> Router {
    let state = DashboardState {
        tracker,
        key_manager,
        password: dashboard_password.clone(),
    };

    Router::new()
        .route("/stats", get(stats_handler))
        .route("/stats/timeline", get(timeline_handler))
        .route("/keys", get(list_keys))
        .route(
            "/keys/generate",
            get(post_generate_fallback).post(generate_key),
        )
        .route("/keys/revoke", get(post_revoke_fallback).post(revoke_key))
        .with_state(state)
}

#[derive(Clone)]
struct DashboardState {
    tracker: Arc<UsageTracker>,
    key_manager: Arc<KeyManager>,
    password: Option<String>,
}

// ── Auth helper ──

fn check_auth(headers: &HeaderMap, password: &Option<String>) -> Result<(), StatusCode> {
    match password {
        None => Ok(()),
        Some(pw) => {
            let auth = headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "));
            match auth {
                Some(token) if token == pw.as_str() => Ok(()),
                _ => Err(StatusCode::UNAUTHORIZED),
            }
        }
    }
}

// ── Fallbacks for GET on POST-only endpoints (browser preflight / user error) ──

async fn post_generate_fallback() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(serde_json::json!({"error": "use POST"})),
    )
}

async fn post_revoke_fallback() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(serde_json::json!({"error": "use POST"})),
    )
}

// ── API handlers ──

async fn stats_handler(
    State(state): State<DashboardState>,
    headers: HeaderMap,
) -> Result<Json<crate::metrics::UsageSnapshot>, StatusCode> {
    check_auth(&headers, &state.password)?;
    let active = state
        .key_manager
        .active_hashes()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(state.tracker.snapshot(&active)))
}

#[derive(Debug, Deserialize)]
struct TimelineQuery {
    #[serde(default = "default_range")]
    range: String,
}

fn default_range() -> String {
    "1d".into()
}

async fn timeline_handler(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<TimelineQuery>,
) -> Result<Json<Vec<crate::storage::TimelineBucket>>, StatusCode> {
    check_auth(&headers, &state.password)?;
    let range = match q.range.as_str() {
        "1d" | "7d" => q.range.as_str(),
        _ => "1d",
    };
    let active = state
        .key_manager
        .active_hashes()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(state.tracker.timeline(range, &active)))
}

#[derive(Debug, Deserialize)]
struct GenerateKeyRequest {
    name: String,
}

#[derive(Debug, Serialize)]
struct GenerateKeyResponse {
    id: u64,
    name: String,
    key: String,
    partial: String,
    created_at: String,
}

async fn generate_key(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Json(body): Json<GenerateKeyRequest>,
) -> Result<Json<GenerateKeyResponse>, (StatusCode, Json<serde_json::Value>)> {
    check_auth(&headers, &state.password)
        .map_err(|s| (s, Json(serde_json::json!({"error": "unauthorized"}))))?;

    match state.key_manager.generate(&body.name) {
        Ok(key) => match state.key_manager.list() {
            Ok(keys) => {
                if let Some(entry) = keys.last() {
                    Ok(Json(GenerateKeyResponse {
                        id: entry.id,
                        name: entry.name.clone(),
                        key,
                        partial: entry.partial.clone(),
                        created_at: entry.created_at.clone(),
                    }))
                } else {
                    Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": "key not persisted"})),
                    ))
                }
            }
            Err(e) => Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e})),
            )),
        },
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        )),
    }
}

#[derive(Debug, Deserialize)]
struct RevokeKeyRequest {
    target: String,
}

async fn revoke_key(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Json(body): Json<RevokeKeyRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    check_auth(&headers, &state.password)
        .map_err(|s| (s, Json(serde_json::json!({"error": "unauthorized"}))))?;

    match state.key_manager.revoke(&body.target) {
        Ok(Some((id, name))) => Ok(Json(serde_json::json!({
            "revoked": true,
            "id": id,
            "name": name
        }))),
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("key not found: {}", body.target)})),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        )),
    }
}

async fn list_keys(
    State(state): State<DashboardState>,
    headers: HeaderMap,
) -> Result<Json<Vec<crate::key_manager::KeyInfo>>, (StatusCode, Json<serde_json::Value>)> {
    check_auth(&headers, &state.password)
        .map_err(|s| (s, Json(serde_json::json!({"error": "unauthorized"}))))?;

    match state.key_manager.list() {
        Ok(keys) => Ok(Json(keys)),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        )),
    }
}
