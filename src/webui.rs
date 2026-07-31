use crate::{key_manager::KeyManager, metrics::UsageTracker};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub fn dashboard_router(
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
        .route("/api/stats", get(stats_handler))
        .route("/api/keys", get(list_keys))
        .route("/api/keys/generate", post(generate_key))
        .route("/api/keys/revoke", post(revoke_key))
        .route("/", get(index_handler))
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

// ── API handlers ──

async fn stats_handler(
    State(state): State<DashboardState>,
    headers: HeaderMap,
) -> Result<Json<crate::metrics::UsageSnapshot>, StatusCode> {
    check_auth(&headers, &state.password)?;
    Ok(Json(state.tracker.snapshot()))
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

// ── Index handler ──

async fn index_handler() -> axum::response::Html<&'static str> {
    axum::response::Html(DASHBOARD_HTML)
}

const DASHBOARD_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>ProxAI Dashboard</title>
<style>
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; background: #0d1117; color: #c9d1d9; padding: 2rem; }
  h1 { font-size: 1.5rem; margin-bottom: 0.5rem; color: #58a6ff; }
  h2 { font-size: 1rem; color: #8b949e; margin-bottom: 1rem; font-weight: normal; }
  .tabs { display: flex; gap: 0; margin-bottom: 1.5rem; }
  .tab { padding: 0.5rem 1rem; cursor: pointer; border: 1px solid #30363d; background: #161b22; color: #8b949e; font-size: 0.85rem; }
  .tab:first-child { border-radius: 6px 0 0 6px; }
  .tab:last-child { border-radius: 0 6px 6px 0; }
  .tab.active { background: #1f6feb; color: #fff; border-color: #1f6feb; }
  .panel { display: none; }
  .panel.active { display: block; }
  .cards { display: flex; gap: 1rem; margin-bottom: 2rem; flex-wrap: wrap; }
  .card { background: #161b22; border: 1px solid #30363d; border-radius: 8px; padding: 1rem 1.5rem; min-width: 180px; }
  .card .label { font-size: 0.75rem; color: #8b949e; text-transform: uppercase; letter-spacing: 0.05em; }
  .card .value { font-size: 1.75rem; font-weight: 600; color: #f0f6fc; margin-top: 0.25rem; }
  table { width: 100%; border-collapse: collapse; }
  th, td { text-align: left; padding: 0.75rem 1rem; border-bottom: 1px solid #21262d; }
  th { font-size: 0.75rem; color: #8b949e; text-transform: uppercase; letter-spacing: 0.05em; }
  td { font-size: 0.875rem; }
  .key-name { color: #58a6ff; }
  .token { font-variant-numeric: tabular-nums; }
  .model-badge { display: inline-block; background: #1f6feb22; color: #58a6ff; border: 1px solid #1f6feb44; border-radius: 4px; padding: 0.1rem 0.5rem; font-size: 0.75rem; margin: 0.15rem 0.25rem; }
  .empty { text-align: center; color: #484f58; padding: 3rem; font-style: italic; }
  .updated { font-size: 0.7rem; color: #484f58; margin-top: 2rem; }
  .expand-row { cursor: pointer; user-select: none; }
  .expand-row:hover { color: #f0f6fc; }
  .model-detail { display: none; }
  .model-detail.open { display: table-row; }
  .model-detail td { padding-left: 2rem; font-size: 0.8rem; color: #8b949e; }
  /* Key management */
  .form-row { display: flex; gap: 0.5rem; margin-bottom: 1rem; align-items: center; }
  input { background: #0d1117; border: 1px solid #30363d; color: #c9d1d9; padding: 0.5rem 0.75rem; border-radius: 6px; font-size: 0.875rem; outline: none; }
  input:focus { border-color: #58a6ff; }
  button { background: #238636; border: none; color: #fff; padding: 0.5rem 1rem; border-radius: 6px; font-size: 0.875rem; cursor: pointer; }
  button:hover { background: #2ea043; }
  button.danger { background: #da3633; }
  button.danger:hover { background: #f85149; }
  button.secondary { background: #21262d; color: #c9d1d9; border: 1px solid #30363d; }
  button.secondary:hover { background: #30363d; }
  .key-display { background: #161b22; border: 1px solid #30363d; border-radius: 6px; padding: 0.5rem 0.75rem; margin-top: 0.5rem; font-family: monospace; font-size: 0.8rem; word-break: break-all; display: none; }
  .key-display.show { display: block; }
  .error { color: #f85149; font-size: 0.85rem; margin-top: 0.5rem; }
  .success { color: #3fb950; font-size: 0.85rem; margin-top: 0.5rem; }
</style>
</head>
<body>
  <h1>ProxAI Dashboard</h1>
  <div class="tabs">
    <div class="tab active" onclick="switchTab('usage')">Usage</div>
    <div class="tab" onclick="switchTab('keys')">Keys</div>
  </div>

  <!-- Usage panel -->
  <div id="panel-usage" class="panel active">
    <div class="cards">
      <div class="card"><div class="label">Total Requests</div><div class="value" id="total-req">-</div></div>
      <div class="card"><div class="label">Total Tokens</div><div class="value" id="total-tok">-</div></div>
      <div class="card"><div class="label">Active Keys</div><div class="value" id="active-keys">-</div></div>
    </div>
    <table>
      <thead><tr><th>Key</th><th>Requests</th><th>Prompt Tok</th><th>Comp Tok</th><th>Last Used</th></tr></thead>
      <tbody id="key-table"></tbody>
    </table>
    <div class="updated" id="updated-at"></div>
  </div>

  <!-- Keys panel -->
  <div id="panel-keys" class="panel">
    <h2>Generate New Key</h2>
    <div class="form-row">
      <input type="text" id="key-name-input" placeholder="Key name (e.g. hermes)" />
      <button onclick="generateKey()">Generate</button>
    </div>
    <div class="key-display" id="generated-key"></div>
    <div class="error" id="key-gen-error"></div>

    <h2 style="margin-top: 2rem;">Existing Keys</h2>
    <table>
      <thead><tr><th>ID</th><th>Name</th><th>Partial</th><th>Created</th><th></th></tr></thead>
      <tbody id="keys-table"></tbody>
    </table>
  </div>

  <div class="error" id="auth-error" style="display:none; margin-top: 1rem;">Auth required. Set Authorization: Bearer &lt;password&gt;</div>

<script>
  const AUTH_TOKEN = localStorage.getItem('proxai_dashboard_token') || '';

  async function api(method, path, body) {
    const headers = {};
    if (AUTH_TOKEN) headers['Authorization'] = 'Bearer ' + AUTH_TOKEN;
    if (body) headers['Content-Type'] = 'application/json';
    const r = await fetch(path, { method, headers, body: body ? JSON.stringify(body) : undefined });
    if (r.status === 401) {
      document.getElementById('auth-error').style.display = 'block';
      throw new Error('unauthorized');
    }
    document.getElementById('auth-error').style.display = 'none';
    if (!r.ok) {
      const err = await r.json().catch(() => ({}));
      throw new Error(err.error || r.statusText);
    }
    return r.json();
  }

  // ── Usage ──

  async function fetchStats() {
    try {
      const d = await api('GET', '/api/stats');
      let totalReq = 0, totalTok = 0;
      d.keys.forEach(k => {
        totalReq += k.total_requests;
        totalTok += k.total_prompt_tokens + k.total_completion_tokens;
      });
      document.getElementById('total-req').textContent = totalReq.toLocaleString();
      document.getElementById('total-tok').textContent = totalTok.toLocaleString();
      document.getElementById('active-keys').textContent = d.keys.length;
      document.getElementById('updated-at').textContent = 'Updated: ' + new Date(d.updated_at).toLocaleTimeString();

      const tbody = document.getElementById('key-table');
      if (d.keys.length === 0) {
        tbody.innerHTML = '<tr><td colspan="5" class="empty">No usage yet. Send a request to start tracking.</td></tr>';
        return;
      }
      tbody.innerHTML = d.keys.map((k, i) => {
        const models = k.model_usage ? Object.entries(k.model_usage) : [];
        const badges = models.map(([m, u]) =>
          '<span class="model-badge">' + m + ': ' + u.requests + ' req, ' + (u.prompt_tokens + u.completion_tokens).toLocaleString() + ' tok</span>'
        ).join('');
        const hasModels = badges.length > 0;
        const rowId = 'row-' + i;
        return '<tr class="expand-row" id="' + rowId + '" onclick="toggleModels(\'' + rowId + '\')">' +
          '<td class="key-name">' + (hasModels ? '&#9656; ' : '') + esc(k.key_name) + '</td>' +
          '<td>' + k.total_requests.toLocaleString() + '</td>' +
          '<td class="token">' + k.total_prompt_tokens.toLocaleString() + '</td>' +
          '<td class="token">' + k.total_completion_tokens.toLocaleString() + '</td>' +
          '<td>' + (k.last_used ? new Date(k.last_used).toLocaleString() : 'never') + '</td>' +
          '</tr>' +
          '<tr class="model-detail" id="' + rowId + '-detail">' +
          '<td colspan="5">' + (badges || '&nbsp;') + '</td>' +
          '</tr>';
      }).join('');
    } catch(e) { if (e.message !== 'unauthorized') console.error(e); }
  }

  function toggleModels(rowId) {
    const detail = document.getElementById(rowId + '-detail');
    const row = document.getElementById(rowId);
    if (detail) {
      detail.classList.toggle('open');
      const arrow = row.querySelector('.key-name');
      if (arrow) {
        arrow.textContent = detail.classList.contains('open')
          ? arrow.textContent.replace('\u25b6', '\u25bc')
          : arrow.textContent.replace('\u25bc', '\u25b6');
      }
    }
  }

  // ── Keys ──

  async function fetchKeys() {
    try {
      const keys = await api('GET', '/api/keys');
      const tbody = document.getElementById('keys-table');
      if (keys.length === 0) {
        tbody.innerHTML = '<tr><td colspan="5" class="empty">No keys yet. Generate one above.</td></tr>';
        return;
      }
      tbody.innerHTML = keys.map(k =>
        '<tr>' +
        '<td>' + k.id + '</td>' +
        '<td class="key-name">' + esc(k.name) + '</td>' +
        '<td>' + esc(k.partial) + '</td>' +
        '<td>' + k.created_at + '</td>' +
        '<td><button class="danger" onclick="revokeKey(\'' + esc(k.name) + '\')">Revoke</button></td>' +
        '</tr>'
      ).join('');
    } catch(e) { if (e.message !== 'unauthorized') console.error(e); }
  }

  async function generateKey() {
    const name = document.getElementById('key-name-input').value.trim();
    if (!name) return;
    document.getElementById('key-gen-error').textContent = '';
    document.getElementById('generated-key').classList.remove('show');
    try {
      const r = await api('POST', '/api/keys/generate', { name });
      document.getElementById('generated-key').textContent = 'API key (shown once!): ' + r.key;
      document.getElementById('generated-key').classList.add('show');
      document.getElementById('key-name-input').value = '';
      fetchKeys();
    } catch(e) {
      document.getElementById('key-gen-error').textContent = e.message;
    }
  }

  async function revokeKey(target) {
    if (!confirm('Revoke key "' + target + '"? This cannot be undone.')) return;
    try {
      await api('POST', '/api/keys/revoke', { target });
      fetchKeys();
    } catch(e) { alert(e.message); }
  }

  // ── Tabs ──

  function switchTab(name) {
    document.querySelectorAll('.tab').forEach(t => t.classList.remove('active'));
    document.querySelectorAll('.panel').forEach(p => p.classList.remove('active'));
    event.target.classList.add('active');
    document.getElementById('panel-' + name).classList.add('active');
    if (name === 'keys') fetchKeys();
  }

  function esc(s) { return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;'); }

  // Init
  if (AUTH_TOKEN) { localStorage.setItem('proxai_dashboard_token', AUTH_TOKEN); }
  fetchStats();
  setInterval(fetchStats, 5000);
</script>
</body>
</html>"#;
