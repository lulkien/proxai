mod admin;
mod auth;
mod config;
mod error;
mod key_manager;

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    middleware,
    response::IntoResponse,
    routing::{get, post},
};
use config::Config;
use error::{ProxyError, Result};
use futures::StreamExt;
use key_manager::{KeyInfo, KeyManager};
use reqwest::Client;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{error, info, warn};

// ── shared state ───────────────────────────────────────────────────────

#[derive(Clone)]
struct ProxyState {
    client: Client,
    config: Arc<Config>,
    models: Arc<HashMap<String, String>>,
}

#[derive(Serialize)]
struct ModelEntry {
    id: String,
    object: String,
    owned_by: String,
}

#[derive(Serialize)]
struct ModelList {
    object: String,
    data: Vec<ModelEntry>,
}

// ── main / CLI ─────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "proxai=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(|s| s.as_str()) {
        Some("serve") => {
            let config_path = parse_flag(&args, "--config", "config.toml");
            let key_path = parse_flag(&args, "--key", "keys.json");
            let socket_path = parse_flag(&args, "--socket", admin::DEFAULT_SOCKET);
            serve(&config_path, &key_path, &socket_path).await
        }

        Some("cli") => {
            let socket_path = parse_flag(&args, "--socket", admin::DEFAULT_SOCKET);
            let action = find_positional(&args, 2, &["--socket"]);
            match action.as_deref() {
                Some("generate-key") => {
                    let name = find_positional(&args, 2, &["--socket", "generate-key"])
                        .ok_or_else(|| {
                            ProxyError::InvalidRequest(
                                "usage: proxai cli --socket <path> generate-key <name>".into(),
                            )
                        })?;
                    cli_generate_key(&socket_path, &name).await
                }
                Some("list-keys") => cli_list_keys(&socket_path).await,
                Some("revoke-key") => {
                    let target = find_positional(&args, 2, &["--socket", "revoke-key"])
                        .ok_or_else(|| {
                            ProxyError::InvalidRequest(
                                "usage: proxai cli --socket <path> revoke-key <name-or-id>".into(),
                            )
                        })?;
                    cli_revoke_key(&socket_path, &target).await
                }
                _ => {
                    eprintln!(
                        "usage: proxai cli --socket <path> <generate-key|list-keys|revoke-key> [arg]"
                    );
                    Ok(())
                }
            }
        }

        // Local bootstrap (direct filesystem, no server needed)
        Some("key") => {
            let key_path = parse_flag(&args, "--key", "keys.json");
            match args.get(2).map(|s| s.as_str()) {
                Some("generate") => {
                    let name = find_positional(&args, 3, &["--key"]).ok_or_else(|| {
                        ProxyError::InvalidRequest(
                            "usage: proxai key generate --key <path> <name>".into(),
                        )
                    })?;
                    let km = KeyManager::new(&key_path);
                    let key = km.generate(&name).map_err(ProxyError::Internal)?;
                    println!("API key generated (save it — shown only once!):");
                    println!();
                    println!("  {key}");
                    println!();
                    println!("Use: Authorization: Bearer {key}");
                    Ok(())
                }
                Some("list") => {
                    let km = KeyManager::new(&key_path);
                    let keys = km.list().map_err(ProxyError::Internal)?;
                    if keys.is_empty() {
                        println!("No keys in {key_path}");
                    } else {
                        println!("Keys in {key_path}:");
                        print_key_table(&keys);
                    }
                    Ok(())
                }
                _ => {
                    eprintln!("usage: proxai key <generate|list> --key <path> [name]");
                    Ok(())
                }
            }
        }

        Some("--help" | "-h") | None => {
            print_usage();
            Ok(())
        }

        _ => {
            let config_path = "config.toml".to_string();
            let key_path = "keys.json".to_string();
            serve(&config_path, &key_path, admin::DEFAULT_SOCKET).await
        }
    }
}

// ── CLI helpers ────────────────────────────────────────────────────────

fn parse_flag(args: &[String], flag: &str, default: &str) -> String {
    for i in 0..args.len() {
        if args[i] == flag
            && let Some(val) = args.get(i + 1)
            && !val.starts_with('-')
        {
            return val.clone();
        }
    }
    default.to_string()
}

fn find_positional(args: &[String], start: usize, skip_flags: &[&str]) -> Option<String> {
    let mut i = start;
    while i < args.len() {
        if skip_flags.contains(&args[i].as_str()) {
            if args[i].starts_with("--") {
                i += 2; // --flag <value>
            } else {
                i += 1; // bare positional marker
            }
        } else {
            return Some(args[i].clone());
        }
    }
    None
}

fn print_key_table(keys: &[KeyInfo]) {
    println!("{:<4} {:<20} {:<24} CREATED", "#", "NAME", "KEY");
    for k in keys {
        println!(
            "{:<4} {:<20} {:<24} {}",
            k.id, k.name, k.partial, k.created_at
        );
    }
}

fn print_usage() {
    println!("proxai — OpenAI-compatible proxy");
    println!();
    println!("Server:");
    println!("  proxai serve --config <path> --key <path> [--socket <path>]");
    println!();
    println!("Client (talks to server via Unix socket):");
    println!("  proxai cli --socket <path> generate-key <name>");
    println!("  proxai cli --socket <path> list-keys");
    println!("  proxai cli --socket <path> revoke-key <name-or-id>");
    println!();
    println!("Local bootstrap (direct filesystem):");
    println!("  proxai key generate --key <path> <name>");
    println!("  proxai key list --key <path>");
    println!();
    println!("  proxai (no args)  Start server (config.toml, keys.json, /tmp/proxai.sock)");
}

// ── client (Unix socket) ───────────────────────────────────────────────

async fn cli_generate_key(socket_path: &str, name: &str) -> Result<()> {
    let response = send_admin_request(
        socket_path,
        &admin::AdminRequest::GenerateKey {
            name: name.to_string(),
        },
    )
    .await?;

    match response {
        admin::AdminResponse::KeyGenerated(resp) => {
            println!("API key generated (save it — shown only once!):");
            println!();
            println!("  {}", resp.key);
            println!();
            println!("Use: Authorization: Bearer {}", resp.key);
        }
        admin::AdminResponse::Error(e) => eprintln!("Server error: {e}"),
        _ => eprintln!("Unexpected response"),
    }
    Ok(())
}

async fn cli_list_keys(socket_path: &str) -> Result<()> {
    let response = send_admin_request(socket_path, &admin::AdminRequest::ListKeys).await?;

    match response {
        admin::AdminResponse::KeyList(keys) => {
            if keys.is_empty() {
                println!("No keys.");
            } else {
                print_key_table(&keys);
            }
        }
        admin::AdminResponse::Error(e) => eprintln!("Server error: {e}"),
        _ => eprintln!("Unexpected response"),
    }
    Ok(())
}

async fn cli_revoke_key(socket_path: &str, target: &str) -> Result<()> {
    let response = send_admin_request(
        socket_path,
        &admin::AdminRequest::RevokeKey {
            target: target.to_string(),
        },
    )
    .await?;

    match response {
        admin::AdminResponse::KeyRevoked { id, name } => {
            println!("Key revoked: #{id} {name}");
        }
        admin::AdminResponse::Error(e) => eprintln!("Server error: {e}"),
        _ => eprintln!("Unexpected response"),
    }
    Ok(())
}

async fn send_admin_request(
    socket_path: &str,
    request: &admin::AdminRequest,
) -> std::result::Result<admin::AdminResponse, ProxyError> {
    use tokio::net::UnixStream;

    let mut stream = UnixStream::connect(socket_path).await.map_err(|e| {
        ProxyError::Internal(format!(
            "cannot connect to admin socket {socket_path}: {e}\n\
             Is the server running? Start with: proxai serve --socket {socket_path}"
        ))
    })?;

    // Serialize request
    let payload = bincode::serialize(request).map_err(|e| ProxyError::Internal(e.to_string()))?;
    let len = (payload.len() as u32).to_le_bytes();

    // Send: 4-byte length + payload
    stream.write_all(&len).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;

    // Read response: 4-byte length
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let resp_len = u32::from_le_bytes(len_buf) as usize;

    // Read response payload
    let mut resp_payload = vec![0u8; resp_len];
    stream.read_exact(&mut resp_payload).await?;

    bincode::deserialize(&resp_payload).map_err(|e| ProxyError::Internal(e.to_string()))
}

// ── server ─────────────────────────────────────────────────────────────

async fn serve(config_path: &str, key_path: &str, socket_path: &str) -> Result<()> {
    let config =
        Arc::new(Config::load(config_path).map_err(|e| ProxyError::ConfigError(e.to_string()))?);

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    let models = discover_models(&client, &config).await;
    let km = Arc::new(KeyManager::new(key_path));

    // Warn if no keys exist (file auto-created by KeyManager)
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
        admin::run(&admin_socket, admin_km).await;
    });

    // TCP API proxy
    let state = ProxyState {
        client,
        config: config.clone(),
        models: Arc::new(models),
    };

    let app = Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
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

// ── model discovery ────────────────────────────────────────────────────

async fn discover_models(client: &Client, config: &Config) -> HashMap<String, String> {
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

// ── proxy endpoints ────────────────────────────────────────────────────

async fn list_models(State(state): State<ProxyState>) -> impl IntoResponse {
    let data: Vec<ModelEntry> = state
        .models
        .iter()
        .map(|(id, owner)| ModelEntry {
            id: id.clone(),
            object: "model".into(),
            owned_by: owner.clone(),
        })
        .collect();

    let list = ModelList {
        object: "list".into(),
        data,
    };

    (StatusCode::OK, Json(list))
}

async fn chat_completions(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    body: String,
) -> Result<impl IntoResponse> {
    let body_json: Value =
        serde_json::from_str(&body).map_err(|e| ProxyError::InvalidRequest(e.to_string()))?;

    let model = body_json
        .get("model")
        .and_then(|m| m.as_str())
        .ok_or_else(|| ProxyError::InvalidRequest("missing 'model' field".into()))?;

    info!("Request for model: {model}");

    let provider = {
        let owner = state
            .models
            .get(model)
            .ok_or_else(|| ProxyError::UnknownModel(model.to_string()))?;
        state
            .config
            .providers
            .iter()
            .find(|p| &p.name == owner)
            .ok_or_else(|| ProxyError::UnknownModel(model.to_string()))?
    };

    info!(
        "Routing to provider: {} -> {}",
        provider.name,
        provider.chat_url()
    );

    let mut upstream_request = state
        .client
        .post(provider.chat_url())
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", provider.api_key),
        )
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.clone());

    if let Some(stream_val) = headers.get("x-stream") {
        upstream_request = upstream_request.header("x-stream", stream_val);
    }

    let upstream_response = upstream_request.send().await.map_err(|e| {
        error!("Upstream request failed: {e}");
        ProxyError::UpstreamError(e.to_string())
    })?;

    let status = upstream_response.status();
    let upstream_headers = upstream_response.headers().clone();

    let mut response = axum::response::Response::builder().status(status);

    if let Some(ct) = upstream_headers.get(header::CONTENT_TYPE) {
        response = response.header(header::CONTENT_TYPE, ct.clone());
    }
    if let Some(te) = upstream_headers.get(header::TRANSFER_ENCODING) {
        response = response.header(header::TRANSFER_ENCODING, te.clone());
    }

    let body_stream = upstream_response.bytes_stream().map(|chunk| {
        chunk.map_err(|e| {
            error!("Stream error: {e}");
            std::io::Error::other(e)
        })
    });

    Ok(response.body(Body::from_stream(body_stream)).unwrap())
}
