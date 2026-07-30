use crate::{
    auth,
    config::Config,
    error::{ProxyError, Result},
    handlers,
    key_manager::KeyManager,
};
use axum::{
    Router, middleware,
    routing::{get, post},
};
use reqwest::Client;
use serde::Serialize;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};
use tracing::{info, warn};

#[derive(Clone)]
pub struct ProxyState {
    pub client: Client,
    pub config: Arc<Config>,
    pub models: Arc<HashMap<String, String>>,
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

pub async fn serve(config_path: &str, key_path: &str, socket_path: &str) -> Result<()> {
    let config =
        Arc::new(Config::load(config_path).map_err(|e| ProxyError::ConfigError(e.to_string()))?);

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    let models = discover_models(&client, &config).await;
    let km = Arc::new(KeyManager::new(key_path));

    match km.list() {
        Ok(keys) if keys.is_empty() => {
            warn!(
                "No API keys in {key_path} — generate one with: proxai cli --socket {socket_path} generate-key <name>"
            );
        }
        Err(e) => warn!("Failed to read keys: {e}"),
        _ => {}
    }

    // Spawn Unix socket admin server
    let admin_km = km.clone();
    let admin_socket = socket_path.to_string();
    tokio::spawn(async move {
        crate::admin::run(&admin_socket, admin_km).await;
    });

    let state = ProxyState {
        client,
        config: config.clone(),
        models: Arc::new(models),
    };

    let app = Router::new()
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

    let addr = config.bind;
    info!("Proxy listening on {addr} (API key required)");
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
