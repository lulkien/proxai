use crate::{
    auth,
    config::{Config, ModelProperties, Provider},
    error::{ProxyError, Result},
    handlers,
    key_manager::KeyManager,
    metrics::UsageTracker,
    model_meta::{self, AdvertisedModel},
    storage::Storage,
};
use axum::{
    Router,
    body::Body,
    http::{StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use reqwest::Client;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    sync::Arc,
};
use tracing::{info, warn};

#[derive(Clone)]
pub struct ProxyState {
    pub client: Client,
    pub config: Arc<Config>,
    /// Advertised model id (namespaced) -> record holding the provider it
    /// routes to, the upstream id to forward, and the upstream entry's own
    /// properties (re-served on `/v1/models`).
    pub models: Arc<HashMap<String, AdvertisedModel>>,
    pub tracker: Arc<UsageTracker>,
}

/// Result of model discovery: the advertised map (used for routing and
/// /v1/models) plus the ids a provider offered but that were filtered out
/// by its allowlist. The latter are counted for the dashboard's
/// "deactivated models" card.
pub struct ModelDiscovery {
    pub advertised: HashMap<String, AdvertisedModel>,
    /// Namespaced ids the provider offered but that its `models` allowlist
    /// filtered out (discovered but not advertised).
    pub inactive: Vec<String>,
}

pub async fn serve(config_path: &str, key_db: &str, socket_path: &str) -> Result<()> {
    let config =
        Arc::new(Config::load(config_path).map_err(|e| ProxyError::ConfigError(e.to_string()))?);

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    let discovery = discover_models(&client, &config).await;
    let (tz_secs, tz_sql) = config.timezone_offset();
    let km = Arc::new(KeyManager::open_with_tz(key_db, tz_secs).map_err(ProxyError::Internal)?);

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
    let storage =
        Arc::new(Storage::open_with_tz(&db_path, tz_secs, tz_sql).map_err(ProxyError::Internal)?);
    let tracker = Arc::new(UsageTracker::new(storage));

    // Database maintenance, runs at startup (first tick fires immediately)
    // then daily: fold raw rows older than the configured retention window
    // into per-(key, model) counters, then fold idle revoked keys into the
    // deleted-usage rollup. Retention must stay >= 7d or the timeline
    // chart's hard-coded 7-day window would read rows that were already
    // folded.
    let retention = config.usage_retention_days.max(7);
    if config.usage_retention_days < 7 {
        warn!(
            "usage_retention_days={} is below the 7d chart window — clamping to 7",
            config.usage_retention_days
        );
    }
    {
        let km = km.clone();
        let tracker = tracker.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(24 * 60 * 60));
            loop {
                ticker.tick().await;
                match km.active_hashes() {
                    Ok(active) => {
                        // Consolidation is sync SQLite work; keep it off the
                        // async runtime thread.
                        let tracker = tracker.clone();
                        let retention = retention;
                        match tokio::task::spawn_blocking(move || {
                            let aged = tracker.consolidate_aged(&active, retention)?;
                            let deleted = tracker.consolidate_deleted(&active)?;
                            Ok::<_, String>((aged, deleted))
                        })
                        .await
                        {
                            Ok(Ok((0, 0))) => {}
                            Ok(Ok((aged, deleted))) => {
                                info!(
                                    "DB maintenance: folded {aged} raw row(s) older than {retention}d, \
                                     consolidated {deleted} idle revoked key(s) into the deleted-usage rollup"
                                );
                            }
                            Ok(Err(e)) => warn!("consolidation failed: {e}"),
                            Err(e) => warn!("consolidation task failed: {e}"),
                        }
                    }
                    Err(e) => warn!("active_hashes failed, skipping consolidation: {e}"),
                }
            }
        });
    }

    // Bind the admin Unix socket up front so a bind failure aborts startup
    // (rather than silently losing admin capability).
    let admin_socket = socket_path.to_string();
    let admin_listener = crate::admin::bind(&admin_socket).map_err(|e| {
        ProxyError::Internal(format!("failed to bind admin socket {admin_socket}: {e}"))
    })?;

    let admin_km = km.clone();
    let admin_tracker = tracker.clone();
    tokio::spawn(async move {
        crate::admin::run(admin_listener, admin_km, admin_tracker).await;
    });

    let state = ProxyState {
        client,
        config: config.clone(),
        models: Arc::new(discovery.advertised),
        tracker: tracker.clone(),
    };

    // Advertised ids + deactivated count feed the dashboard Models tab.
    // Captured before `state` is moved into the router below.
    let advertised_models: Vec<String> = state.models.keys().cloned().collect();
    let deactivated_count = discovery.inactive.len();

    let api_routes = Router::new()
        .route("/v1/models", get(handlers::list_models))
        .route("/v1/chat/completions", post(handlers::chat_completions))
        .layer(middleware::from_fn_with_state(
            auth::AuthState {
                key_manager: km.clone(),
            },
            auth::require_api_key,
        ))
        .with_state(state);

    let dashboard_api = crate::webui::dashboard_api_router(
        tracker.clone(),
        km.clone(),
        &config.dashboard_password,
        advertised_models,
        deactivated_count,
    );

    // Serve embedded dashboard WASM files
    let dash_files = Router::new()
        .route("/", get(serve_dash_index_embedded))
        .route("/{*path}", get(serve_dash_file_embedded));

    // Redirect /dashboard -> /dashboard/ so relative CSS/JS resolve correctly
    async fn redirect_dashboard() -> Response {
        Response::builder()
            .status(StatusCode::MOVED_PERMANENTLY)
            .header(header::LOCATION, "/dashboard/")
            .body(Body::empty())
            .unwrap()
    }

    let app = Router::new()
        .nest("/dashboard/api", dashboard_api)
        .nest("/dashboard/", dash_files)
        .route("/dashboard", get(redirect_dashboard))
        .merge(api_routes);

    let addr = config.bind;
    info!("Proxy listening on {addr} (API key required)");
    info!("Dashboard at http://{addr}/dashboard");
    info!("Admin socket: @{socket_path} (abstract, local, no auth)");
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

async fn serve_dash_index_embedded() -> Response {
    serve_embedded("index.html")
}

async fn serve_dash_file_embedded(path: axum::extract::Path<String>) -> Response {
    serve_embedded(&path.0)
}

fn serve_embedded(path: &str) -> Response {
    let path = if path.is_empty() || path == "/" {
        "index.html"
    } else {
        path.trim_start_matches('/')
    };

    match crate::dashboard_assets::DashboardAssets::get(path) {
        Some(file) => {
            let ct = content_type(path);
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, ct)
                .body(Body::from(file.data.into_owned()))
                .unwrap()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
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
    } else {
        "application/octet-stream"
    }
}

pub async fn discover_models(client: &Client, config: &Config) -> ModelDiscovery {
    use axum::http::header;

    let mut map = HashMap::new();
    let mut inactive: Vec<String> = Vec::new();

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
                            let discovered = discovered_ids(&json);

                            // Preferred models the provider doesn't offer
                            // are skipped, not fatal.
                            for want in &provider.models {
                                if !discovered.iter().any(|id| id == want) {
                                    warn!(
                                        "Provider {} does not offer preferred model '{}' — skipped",
                                        provider.name, want
                                    );
                                }
                            }

                            let (found, filtered) = select_from_payload(provider, &json);
                            for (namespaced, model) in &found {
                                match model_meta::find_context_length(&model.properties) {
                                    Some((length, key)) => {
                                        info!("  + {namespaced} (context {length} from {key})");
                                    }
                                    None => info!(
                                        "  + {namespaced} (no context window reported by {})",
                                        provider.name
                                    ),
                                }
                            }
                            map.extend(found);
                            // Discovered but filtered out by the allowlist:
                            // counted (not advertised) for the dashboard.
                            inactive.extend(filtered);
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

    // Operator-declared properties (config `[model_properties]`) are overlaid
    // last, so a model is only reported as having no window when neither the
    // provider nor the config supplied one.
    apply_model_properties(&mut map, &config.model_properties);

    // Models whose provider reports no window are the ones clients still have
    // to guess at; name them so the gap shows up in the journal instead of
    // silently becoming a client-side default.
    for (id, model) in &map {
        if model.context_length().is_none() {
            warn!("  ! {id} advertises no context window — clients fall back to their own default");
        }
    }
    let with_window = map
        .values()
        .filter(|m| m.context_length().is_some())
        .count();
    if !map.is_empty() {
        info!(
            "Context windows: {with_window}/{} advertised model(s) report one",
            map.len()
        );
    }

    if map.is_empty() {
        warn!("No models discovered — proxy will reject all chat requests");
    } else {
        info!("Total models advertised: {}", map.len());
    }
    if !inactive.is_empty() {
        info!(
            "  {} discovered model(s) not advertised (allowlist) — counted as deactivated",
            inactive.len()
        );
    }

    ModelDiscovery {
        advertised: map,
        inactive,
    }
}

/// Overlay the config `[model_properties]` table on the advertised map.
///
/// A key is either the advertised id (`provider/upstream-id`, what clients send
/// and `/v1/models` serves) or a bare upstream id, which applies to every
/// provider offering that model. Keys matching nothing are warned about — a
/// typo would otherwise leave the fallback silently unadvertised.
///
/// Keys are applied in sorted order so that two keys matching the same model
/// (an advertised id and its bare upstream id, say) resolve the same way on
/// every boot: the later key wins where they declare the same property.
fn apply_model_properties(
    models: &mut HashMap<String, AdvertisedModel>,
    overrides: &HashMap<String, ModelProperties>,
) {
    let mut entries: Vec<(&String, &ModelProperties)> = overrides.iter().collect();
    entries.sort_by_key(|(key, _)| *key);

    for (key, properties) in entries {
        let declared = properties.as_map();
        if declared.is_empty() {
            // Reachable when the table entry is empty, or everything it
            // declared was rejected (out-of-band window, proxy-owned key).
            warn!("model_properties['{key}'] declares no advertisable property — nothing to do");
            continue;
        }
        let targets = override_targets(models, key);

        if targets.is_empty() {
            warn!(
                "model_properties['{key}'] matches no advertised model — ignored (name the model \
                 as the provider does, e.g. 'deepseek-v4-pro' or 'nemotron-3.5')"
            );
            continue;
        }
        for id in targets {
            let Some(model) = models.get_mut(&id) else {
                continue;
            };
            let applied = model.apply_overrides(&declared);
            let window = model
                .context_length()
                .map(|length| length.to_string())
                .unwrap_or_else(|| "none".into());
            info!(
                "  ~ {id}: advertising config properties [{}] (context {window})",
                applied.join(", ")
            );
        }
    }
}

/// Advertised ids a config `[model_properties]` key applies to.
///
/// A key names the model as its provider calls it — `deepseek-v4-pro`,
/// `nemotron-3.5` — which is also what a provider's own prefixed id reduces to
/// (`nvidia/nemotron-3.5` -> `nemotron-3.5`). Both the advertised id and the
/// upstream id are accepted as well, so an entry may name the model either way.
/// Sorted, so the startup log order does not depend on HashMap iteration.
fn override_targets(models: &HashMap<String, AdvertisedModel>, key: &str) -> Vec<String> {
    let mut targets: Vec<String> = models
        .iter()
        .filter(|(id, model)| {
            id.as_str() == key
                || model.upstream_id == key
                || model_meta::bare_model_name(&model.provider, &model.upstream_id) == key
        })
        .map(|(id, _)| id.clone())
        .collect();
    targets.sort();
    targets
}

/// Upstream model ids listed by a provider's `/models` payload.
fn discovered_ids(payload: &Value) -> Vec<&str> {
    payload
        .get("data")
        .and_then(|d| d.as_array())
        .map(|data| {
            data.iter()
                .filter_map(|entry| entry.get("id").and_then(|id| id.as_str()))
                .collect()
        })
        .unwrap_or_default()
}

/// Advertised models for one provider's `/models` payload: namespaced id ->
/// record, plus the ids its allowlist filtered out. Each record keeps the
/// upstream entry's own properties so `/v1/models` can re-serve them, and the
/// upstream id so requests are forwarded without re-deriving it.
fn select_from_payload(
    provider: &Provider,
    payload: &Value,
) -> (HashMap<String, AdvertisedModel>, Vec<String>) {
    let Some(data) = payload.get("data").and_then(|d| d.as_array()) else {
        return (HashMap::new(), Vec::new());
    };
    let ids: Vec<&str> = data
        .iter()
        .filter_map(|entry| entry.get("id").and_then(|id| id.as_str()))
        .collect();
    let keep: HashSet<&str> = select_models(&ids, &provider.models).into_iter().collect();

    let mut advertised = HashMap::new();
    let mut filtered = Vec::new();
    for entry in data {
        let Some(upstream_id) = entry.get("id").and_then(|id| id.as_str()) else {
            continue;
        };
        let namespaced = model_meta::namespace_model(&provider.name, upstream_id);
        if !keep.contains(upstream_id) {
            filtered.push(namespaced);
            continue;
        }
        match advertised.entry(namespaced) {
            Entry::Occupied(existing) => warn!(
                "Provider {} advertises {} twice (upstream ids collide after namespacing) — keeping the first",
                provider.name,
                existing.key()
            ),
            Entry::Vacant(slot) => {
                slot.insert(AdvertisedModel::from_entry(
                    &provider.name,
                    upstream_id,
                    entry,
                ));
            }
        }
    }
    (advertised, filtered)
}

/// The subset of a provider's discovered model ids to advertise.
///
/// An empty `preferred` list advertises every discovered model; a non-empty
/// list restricts advertising to those ids, but only such ids the provider
/// actually offers (missing entries are simply absent from the result).
fn select_models<'a>(discovered: &[&'a str], preferred: &[String]) -> Vec<&'a str> {
    if preferred.is_empty() {
        return discovered.to_vec();
    }
    discovered
        .iter()
        .copied()
        .filter(|id| preferred.iter().any(|want| want == id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn provider(name: &str, models: &[&str]) -> Provider {
        Provider {
            name: name.to_string(),
            url: format!("http://{name}.test/v1"),
            api_key: "sk-test".to_string(),
            models: models.iter().map(|m| m.to_string()).collect(),
        }
    }

    #[test]
    fn select_from_payload_keeps_upstream_properties_and_limits() {
        let payload = json!({"data": [
            {"id": "bonsai-27b", "object": "model", "owned_by": "llamacpp",
             "aliases": ["bonsai-27b"], "meta": {"n_ctx": 131072}},
            {"id": "other", "object": "model"},
        ]});
        let (advertised, filtered) =
            select_from_payload(&provider("localai", &["bonsai-27b"]), &payload);

        assert_eq!(filtered, vec!["localai/other"]);
        let model = &advertised["localai/bonsai-27b"];
        assert_eq!(model.provider, "localai");
        assert_eq!(model.upstream_id, "bonsai-27b");
        assert_eq!(model.context_length(), Some(131072));
        assert!(
            model.properties.contains_key("aliases"),
            "properties are kept verbatim"
        );
        assert!(
            !model.properties.contains_key("id"),
            "id belongs to the proxy"
        );
    }

    #[test]
    fn select_from_payload_without_allowlist_advertises_everything() {
        let payload = json!({"data": [{"id": "a"}, {"id": "b"}]});
        let (advertised, filtered) = select_from_payload(&provider("p", &[]), &payload);
        assert_eq!(advertised.len(), 2);
        assert!(filtered.is_empty());
    }

    #[test]
    fn select_from_payload_dedups_provider_prefix() {
        let payload = json!({"data": [{"id": "nvidia/nemotron-3-super-120b-a12b"}]});
        let (advertised, filtered) = select_from_payload(&provider("nvidia", &[]), &payload);
        assert!(advertised.contains_key("nvidia/nemotron-3-super-120b-a12b"));
        assert!(filtered.is_empty());
        assert_eq!(
            advertised["nvidia/nemotron-3-super-120b-a12b"].upstream_id,
            "nvidia/nemotron-3-super-120b-a12b"
        );
    }

    #[test]
    fn select_from_payload_keeps_first_on_namespace_collision() {
        let payload = json!({"data": [
            {"id": "x", "owned_by": "first"},
            {"id": "nvidia/x", "owned_by": "second"},
        ]});
        let (advertised, filtered) = select_from_payload(&provider("nvidia", &[]), &payload);
        assert_eq!(advertised.len(), 1);
        assert_eq!(advertised["nvidia/x"].upstream_id, "x");
        assert!(filtered.is_empty(), "a collision is not a deactivation");
    }

    #[test]
    fn select_from_payload_ignores_entries_without_an_id() {
        let payload = json!({"data": [{"object": "model"}, {"id": "real"}]});
        let (advertised, _) = select_from_payload(&provider("p", &[]), &payload);
        assert_eq!(advertised.len(), 1);
        assert!(advertised.contains_key("p/real"));
    }

    #[test]
    fn discovered_ids_lists_upstream_ids() {
        let payload = json!({"data": [{"id": "a"}, {"object": "model"}, {"id": "b"}]});
        assert_eq!(discovered_ids(&payload), vec!["a", "b"]);
        assert!(discovered_ids(&json!({"object": "list"})).is_empty());
    }

    #[test]
    fn select_models_empty_preferred_advertises_all() {
        let discovered = vec!["deepseek-chat", "deepseek-reasoner", "gpt-4o"];
        let advertised = select_models(&discovered, &[]);
        assert_eq!(advertised, discovered);
    }

    #[test]
    fn select_models_filters_to_preferred_existing_only() {
        let discovered = vec!["deepseek-chat", "deepseek-reasoner", "gpt-4o"];
        let preferred: Vec<String> = ["deepseek-chat", "gpt-4o", "does-not-exist"]
            .map(String::from)
            .to_vec();
        let advertised = select_models(&discovered, &preferred);
        assert_eq!(advertised, vec!["deepseek-chat", "gpt-4o"]);
    }

    #[test]
    fn select_models_matches_exact_ids_only() {
        // A preferred id must match the whole upstream id, not a substring.
        let discovered = vec!["deepseek-chat"];
        let preferred: Vec<String> = ["chat"].map(String::from).to_vec();
        assert!(select_models(&discovered, &preferred).is_empty());
    }

    // ── config [model_properties] overlay ──

    fn props(context_length: Option<u64>, extra: &[(&str, serde_json::Value)]) -> ModelProperties {
        ModelProperties {
            context_length,
            extra: extra
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        }
    }

    fn advertised(pairs: &[(&str, &str, Value)]) -> HashMap<String, AdvertisedModel> {
        pairs
            .iter()
            .map(|(provider, id, entry)| {
                (
                    model_meta::namespace_model(provider, id),
                    AdvertisedModel::from_entry(provider, id, entry),
                )
            })
            .collect()
    }

    #[test]
    fn override_targets_accepts_advertised_and_upstream_ids() {
        let models = advertised(&[
            ("deepseek", "deepseek-v4-pro", json!({})),
            ("localai", "bonsai-27b", json!({})),
        ]);

        // The model's own name — the form the config table uses.
        assert_eq!(
            override_targets(&models, "deepseek-v4-pro"),
            vec!["deepseek/deepseek-v4-pro"]
        );
        assert_eq!(
            override_targets(&models, "bonsai-27b"),
            vec!["localai/bonsai-27b"]
        );
        // The advertised id works too.
        assert_eq!(
            override_targets(&models, "deepseek/deepseek-v4-pro"),
            vec!["deepseek/deepseek-v4-pro"]
        );
        // Nothing advertises it, and a prefix is not a match.
        assert!(override_targets(&models, "bonsai-27").is_empty());
        assert!(override_targets(&models, "other/bonsai-27b").is_empty());
    }

    #[test]
    fn override_targets_strips_the_providers_own_namespace() {
        // NVIDIA ids carry the provider name; the table names the model.
        let models = advertised(&[("nvidia", "nvidia/nemotron-3.5", json!({}))]);
        assert_eq!(
            override_targets(&models, "nemotron-3.5"),
            vec!["nvidia/nemotron-3.5"],
            "a table key is the model name, not the namespaced id"
        );
    }

    #[test]
    fn override_targets_of_a_bare_id_covers_every_provider() {
        let models = advertised(&[
            ("alpha", "gpt-4o", json!({})),
            ("beta", "gpt-4o", json!({})),
        ]);
        assert_eq!(
            override_targets(&models, "gpt-4o"),
            vec!["alpha/gpt-4o", "beta/gpt-4o"]
        );
    }

    #[test]
    fn override_targets_uses_the_upstream_id_verbatim() {
        // A provider whose own ids carry its name is not prefixed twice, so the
        // upstream id, the bare name and the advertised id can all be written.
        let models = advertised(&[("nvidia", "nvidia/nemotron-3-super-120b-a12b", json!({}))]);
        for key in [
            "nvidia/nemotron-3-super-120b-a12b",
            "nemotron-3-super-120b-a12b",
        ] {
            assert_eq!(
                override_targets(&models, key),
                vec!["nvidia/nemotron-3-super-120b-a12b"],
                "key '{key}' must resolve to the advertised model"
            );
        }
    }

    #[test]
    fn apply_model_properties_fills_in_a_missing_window() {
        let mut models = advertised(&[("deepseek", "deepseek-v4-pro", json!({}))]);
        let overrides = HashMap::from([(
            "deepseek/deepseek-v4-pro".to_string(),
            props(
                Some(1_000_000),
                &[("display_name", json!("DeepSeek V4 Pro"))],
            ),
        )]);

        apply_model_properties(&mut models, &overrides);

        let model = &models["deepseek/deepseek-v4-pro"];
        assert_eq!(model.context_length(), Some(1_000_000));
        let out = serde_json::to_value(model.entry("deepseek/deepseek-v4-pro")).unwrap();
        assert_eq!(out["context_length"], 1_000_000);
        assert_eq!(out["display_name"], "DeepSeek V4 Pro");
        assert_eq!(out["owned_by"], "deepseek");
    }

    #[test]
    fn apply_model_properties_overrides_a_reported_window() {
        let mut models = advertised(&[("p", "m", json!({"max_model_len": 8192}))]);
        let overrides = HashMap::from([("p/m".to_string(), props(Some(131072), &[]))]);

        apply_model_properties(&mut models, &overrides);

        assert_eq!(models["p/m"].context_length(), Some(131072));
    }

    #[test]
    fn apply_model_properties_leaves_undeclared_models_untouched() {
        let mut models = advertised(&[
            ("p", "declared", json!({"context_length": 8192})),
            ("p", "other", json!({"context_length": 4096})),
        ]);
        let overrides = HashMap::from([("p/declared".to_string(), props(Some(32768), &[]))]);

        apply_model_properties(&mut models, &overrides);

        assert_eq!(models["p/declared"].context_length(), Some(32768));
        assert_eq!(
            models["p/other"].context_length(),
            Some(4096),
            "a model the table does not name keeps its upstream window"
        );
    }

    #[test]
    fn apply_model_properties_is_a_noop_for_an_empty_table() {
        let mut models = advertised(&[("p", "m", json!({}))]);
        apply_model_properties(&mut models, &HashMap::new());
        assert_eq!(models["p/m"].context_length(), None);
    }

    #[test]
    fn apply_model_properties_skips_a_declaration_with_nothing_in_it() {
        // E.g. a table entry whose only property was rejected at load.
        let mut models = advertised(&[("p", "m", json!({"context_length": 8192}))]);
        let overrides = HashMap::from([("p/m".to_string(), props(None, &[]))]);

        apply_model_properties(&mut models, &overrides);

        assert_eq!(models["p/m"].context_length(), Some(8192));
        assert_eq!(models["p/m"].properties.len(), 1);
    }

    #[test]
    fn apply_model_properties_ignores_an_unmatched_key() {
        // Warned about, not fatal: a typo must not stop startup, and must not
        // touch the models that do exist.
        let mut models = advertised(&[("p", "m", json!({}))]);
        let overrides = HashMap::from([("p/typo".to_string(), props(Some(1_000_000), &[]))]);

        apply_model_properties(&mut models, &overrides);

        assert_eq!(models.len(), 1);
        assert_eq!(models["p/m"].context_length(), None);
    }

    #[test]
    fn apply_model_properties_applies_keys_in_a_stable_order() {
        // An advertised id and its bare upstream id both match the same model;
        // the result must not depend on HashMap iteration order.
        let overrides = HashMap::from([
            ("p/m".to_string(), props(Some(32768), &[])),
            ("m".to_string(), props(Some(131072), &[])),
        ]);

        let mut first = advertised(&[("p", "m", json!({}))]);
        apply_model_properties(&mut first, &overrides);
        let mut second = advertised(&[("p", "m", json!({}))]);
        apply_model_properties(&mut second, &overrides);

        assert_eq!(
            first["p/m"].context_length(),
            second["p/m"].context_length()
        );
        // Sorted order: "m" before "p/m", so the advertised-id entry wins.
        assert_eq!(first["p/m"].context_length(), Some(32768));
    }
}
