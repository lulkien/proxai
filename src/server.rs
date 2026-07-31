use crate::{
    auth,
    config::Config,
    error::{ProxyError, Result},
    handlers,
    key_manager::KeyManager,
    metrics::UsageTracker,
    storage::Storage,
};
use axum::{
    Router,
    extract::State,
    middleware,
    routing::{get, post},
};
use axum::{
    body::Body,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use reqwest::Client;
use serde::Serialize;
use serde_json::Value;
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tracing::{info, warn};

#[derive(Clone)]
pub struct ProxyState {
    pub client: Client,
    pub config: Arc<Config>,
    pub models: Arc<HashMap<String, String>>,
    pub tracker: Arc<UsageTracker>,
}

#[derive(Serialize)]
pub struct ModelEntry {
    pub id: String,
    pub object: String,
    pub owned_by: String,
}

#[derive(Serialize)]
pub struct ModelList {
    pub object: String,
    pub data: Vec<ModelEntry>,
}

pub async fn serve(
    config_path: &str,
    key_db: &str,
    socket_path: &str,
    dashboard_dist_override: Option<&str>,
) -> Result<()> {
    let config =
        Arc::new(Config::load(config_path).map_err(|e| ProxyError::ConfigError(e.to_string()))?);

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    let models = discover_models(&client, &config).await;
    let km = Arc::new(KeyManager::open(key_db).map_err(ProxyError::Internal)?);

    match km.list() {
        Ok(keys) if keys.is_empty() => {
            warn!(
                "No API keys in {key_db} — generate one with: proxai cli --socket {socket_path} generate-key <name>"
            );
        }
        Err(e) => warn!("Failed to read keys: {e}"),
        _ => {}
    }

    // Open SQLite database for persistent usage tracking
    let db_path = config
        .db_path
        .clone()
        .unwrap_or_else(|| "proxai.db".to_string());
    let storage = Arc::new(Storage::open(&db_path).map_err(ProxyError::Internal)?);
    let tracker = Arc::new(UsageTracker::new(storage));

    // Spawn Unix socket admin server
    let admin_km = km.clone();
    let admin_tracker = tracker.clone();
    let admin_socket = socket_path.to_string();
    tokio::spawn(async move {
        crate::admin::run(&admin_socket, admin_km, admin_tracker).await;
    });

    let state = ProxyState {
        client,
        config: config.clone(),
        models: Arc::new(models),
        tracker: tracker.clone(),
    };

    let api_routes = Router::new()
        .route("/v1/models", get(handlers::list_models))
        .route("/v1/chat/completions", post(handlers::chat_completions))
        .route("/v1/responses", post(handlers::responses))
        .layer(middleware::from_fn_with_state(
            auth::AuthState {
                key_manager: km.clone(),
            },
            auth::require_api_key,
        ))
        .with_state(state);

    let dashboard_api =
        crate::webui::dashboard_api_router(tracker.clone(), km.clone(), &config.dashboard_password);

    // Serve dashboard WASM files from dist directory
    let dashboard_dist = dashboard_dist_override
        .map(|s| s.to_string())
        .or_else(|| config.dashboard_dist.clone())
        .unwrap_or_else(|| "target/dx/proxai-dashboard/debug/web/public".to_string());
    let dist = Arc::new(dashboard_dist);

    let dash_files = Router::new()
        .route("/", get(serve_dash_index))
        .route("/{*path}", get(serve_dash_file_handler))
        .with_state(dist.clone());

    let app = Router::new()
        .nest("/dashboard/api", dashboard_api)
        .nest("/dashboard", dash_files)
        .merge(api_routes);

    let addr = config.bind;
    info!("Proxy listening on {addr} (API key required)");
    info!("Dashboard at http://{addr}/dashboard");
    info!("Admin socket: {socket_path} (local, no auth)");
    info!(
        "Providers: {:?}",
        config.providers.iter().map(|p| &p.name).collect::<Vec<_>>()
    );

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;

    Ok(())
}

/// Simple MIME-type mapping for dashboard static files.
fn content_type(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html"
    } else if path.ends_with(".js") {
        "application/javascript"
    } else if path.ends_with(".wasm") {
        "application/wasm"
    } else if path.ends_with(".css") {
        "text/css"
    } else if path.ends_with(".json") {
        "application/json"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "application/octet-stream"
    }
}

async fn serve_dash_index(State(dist): State<Arc<String>>) -> Response {
    serve_dash_file(&dist, "").await
}

/// Axum handler that serves dashboard static files.
async fn serve_dash_file_handler(
    State(dist): State<Arc<String>>,
    path: axum::extract::Path<String>,
) -> Response {
    serve_dash_file(&dist, &path.0).await
}

async fn serve_dash_file(dist: &str, path: &str) -> Response {
    let file_path = if path.is_empty() || path == "/" {
        "index.html"
    } else {
        path.trim_start_matches('/')
    };

    let full = PathBuf::from(dist).join(file_path);

    // Prevent directory traversal
    if !full.starts_with(dist) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    match tokio::fs::read(&full).await {
        Ok(data) => {
            let ct = content_type(file_path);
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, ct)
                .body(Body::from(data))
                .unwrap()
        }
        Err(_) => {
            // Try index.html for directory-like paths
            if !file_path.ends_with(".html") && !file_path.contains('.') {
                let html = PathBuf::from(dist).join("index.html");
                if let Ok(data) = tokio::fs::read(&html).await {
                    return Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "text/html")
                        .body(Body::from(data))
                        .unwrap();
                }
            }
            (StatusCode::NOT_FOUND, "not found").into_response()
        }
    }
}

pub async fn discover_models(client: &Client, config: &Config) -> HashMap<String, String> {
    use axum::http::header;

    let mut map = HashMap::new();

    for provider in &config.providers {
        let models_url = provider.models_url();
        info!(
            "Discovering models from {} ({})...",
            provider.name, models_url
        );

        match client
            .get(&models_url)
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", provider.api_key),
            )
            .send()
            .await
        {
            Ok(resp) => {
                if resp.status().is_success() {
                    match resp.json::<Value>().await {
                        Ok(json) => {
                            if let Some(data) = json.get("data").and_then(|d| d.as_array()) {
                                for entry in data {
                                    if let Some(id) = entry.get("id").and_then(|i| i.as_str()) {
                                        info!("  + {id} ({})", provider.name);
                                        map.insert(id.to_string(), provider.name.clone());
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Failed to parse models from {}: {e}", provider.name);
                        }
                    }
                } else {
                    warn!(
                        "{} returned {} for /v1/models — skipping",
                        provider.name,
                        resp.status()
                    );
                }
            }
            Err(e) => {
                warn!(
                    "Failed to reach {} for models: {e} — skipping",
                    provider.name
                );
            }
        }
    }

    if map.is_empty() {
        warn!("No models discovered — proxy will reject all chat requests");
    } else {
        info!("Total models discovered: {}", map.len());
    }

    map
}
