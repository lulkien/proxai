use dioxus::prelude::*;
use gloo_net::http::Request;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

fn main() {
    launch(app);
}

// ── API types ──

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct UsageSnapshot {
    keys: Vec<KeyUsage>,
    updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct KeyUsage {
    key_name: String,
    total_requests: u64,
    total_prompt_tokens: u64,
    total_completion_tokens: u64,
    last_used: Option<String>,
    model_usage: HashMap<String, ModelUsage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ModelUsage {
    requests: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct KeyInfo {
    id: u64,
    name: String,
    partial: String,
    created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct GenerateKeyResponse {
    id: u64,
    name: String,
    key: String,
    partial: String,
    created_at: String,
}

#[derive(Debug, Serialize)]
struct GenerateKeyRequest {
    name: String,
}

#[derive(Debug, Serialize)]
struct RevokeKeyRequest {
    target: String,
}

fn api_base() -> String {
    "/dashboard/api".to_string()
}

fn fmt_num(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}

// ── App ──

const CSS: &str = r#"
*{margin:0;padding:0;box-sizing:border-box}
body{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;background:#0d1117;color:#c9d1d9}
.app{max-width:960px;margin:0 auto;padding:2rem;min-height:100vh}
h1{font-size:1.5rem;color:#58a6ff;margin-bottom:1rem}
h2{font-size:1rem;color:#8b949e;margin-bottom:.5rem;font-weight:400}
.tabs{display:flex;gap:0;margin-bottom:1.5rem}
.tab-btn{padding:.5rem 1rem;cursor:pointer;border:1px solid #30363d;background:#161b22;color:#8b949e;font-size:.85rem;font-family:inherit}
.tab-btn:first-child{border-radius:6px 0 0 6px}
.tab-btn:last-child{border-radius:0 6px 6px 0}
.tab-btn.active{background:#1f6feb;color:#fff;border-color:#1f6feb}
.cards{display:flex;gap:1rem;margin-bottom:1.5rem;flex-wrap:wrap}
.card{background:#161b22;border:1px solid #30363d;border-radius:8px;padding:1rem 1.5rem;min-width:180px}
.card .label{font-size:.75rem;color:#8b949e;text-transform:uppercase;letter-spacing:.05em}
.card .value{font-size:1.75rem;font-weight:600;color:#f0f6fc;margin-top:.25rem}
table{width:100%;border-collapse:collapse}
th,td{text-align:left;padding:.75rem 1rem;border-bottom:1px solid #21262d}
th{font-size:.75rem;color:#8b949e;text-transform:uppercase;letter-spacing:.05em;font-weight:500}
td{font-size:.875rem}
.key-name{color:#58a6ff}
.token{font-variant-numeric:tabular-nums}
.model-badge{display:inline-block;background:#1f6feb22;color:#58a6ff;border:1px solid #1f6feb44;border-radius:4px;padding:.1rem .5rem;font-size:.75rem;margin:.15rem .25rem}
.empty{text-align:center;color:#484f58;padding:3rem;font-style:italic}
.error-msg{color:#f85149;font-size:.85rem;margin-bottom:1rem}
.updated{font-size:.7rem;color:#484f58;margin-top:2rem}
.usage-row{cursor:pointer;user-select:none}
.usage-row:hover{color:#f0f6fc}
.model-detail td{padding-left:2rem;font-size:.8rem;color:#8b949e}
.form-row{display:flex;gap:.5rem;margin-bottom:1rem;align-items:center}
input{background:#0d1117;border:1px solid #30363d;color:#c9d1d9;padding:.5rem .75rem;border-radius:6px;font-size:.875rem;outline:none;flex:1;font-family:inherit}
input:focus{border-color:#58a6ff}
.btn{background:#238636;border:none;color:#fff;padding:.5rem 1rem;border-radius:6px;font-size:.875rem;cursor:pointer;font-family:inherit}
.btn:hover{background:#2ea043}
.btn-danger{background:#da3633;padding:.25rem .75rem;border-radius:4px;font-size:.8rem}
.btn-danger:hover{background:#f85149}
.key-display{background:#161b22;border:1px solid #30363d;border-radius:6px;padding:.5rem .75rem;margin-bottom:1rem;font-family:monospace;font-size:.8rem;word-break:break-all}
.section-head{margin:2rem 0 .5rem}
"#;

fn app() -> Element {
    let mut tab = use_signal(|| "usage");

    // Override dx's "ProxAI Dashboarddioxus | ⛺" title
    if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
        doc.set_title("ProxAI Dashboard");
    }

    rsx! {
        style { {CSS} }
        div { class: "app",
            h1 { "ProxAI Dashboard" }
            div { class: "tabs",
                button {
                    class: format!("tab-btn{}", if tab() == "usage" { " active" } else { "" }),
                    onclick: move |_| tab.set("usage"),
                    "Usage"
                }
                button {
                    class: format!("tab-btn{}", if tab() == "keys" { " active" } else { "" }),
                    onclick: move |_| tab.set("keys"),
                    "Keys"
                }
            }
            if tab() == "usage" {
                UsagePanel {}
            } else {
                KeysPanel {}
            }
        }
    }
}

// ── Usage panel ──

fn UsagePanel() -> Element {
    let snapshot = use_resource(fetch_stats);

    match &*snapshot.read_unchecked() {
        Some(Ok(s)) => render_usage(s),
        Some(Err(e)) => rsx! { p { class: "error-msg", "Error: {e}" } },
        None => rsx! { p { class: "empty", "Loading..." } },
    }
}

fn render_usage(s: &UsageSnapshot) -> Element {
    let total_req: u64 = s.keys.iter().map(|k| k.total_requests).sum();

    rsx! {
        div { class: "cards",
            div { class: "card",
                div { class: "label", "Total Requests" }
                div { class: "value", "{fmt_num(total_req)}" }
            }
            div { class: "card",
                div { class: "label", "Active Keys" }
                div { class: "value", "{s.keys.len()}" }
            }
        }

        if s.keys.is_empty() {
            p { class: "empty", "No usage yet. Send a request to start tracking." }
        } else {
            table {
                thead {
                    tr {
                        th { "Key" }
                        th { "Requests" }
                        th { "Last Used" }
                    }
                }
                tbody {
                    for data in &s.keys {
                        UsageRow { data: data.clone() }
                    }
                }
            }
            div { class: "updated", "Updated: {s.updated_at}" }
        }
    }
}

#[component]
fn UsageRow(data: KeyUsage) -> Element {
    let mut expanded = use_signal(|| false);
    let has_models = !data.model_usage.is_empty();

    rsx! {
        tr {
            class: "usage-row",
            onclick: move |_| expanded.toggle(),
            td { class: "key-name",
                if has_models { if expanded() { "\u{25bc} " } else { "\u{25b6} " } }
                "{data.key_name}"
            }
            td { "{fmt_num(data.total_requests)}" }
            td {
                if let Some(ref lu) = data.last_used { "{lu}" } else { "never" }
            }
        }
        if expanded() && has_models {
            tr { class: "model-detail",
                td { colspan: "3",
                    for (model, usage) in &data.model_usage {
                        span { class: "model-badge",
                            "{model}: {usage.requests} req"
                        }
                    }
                }
            }
        }
    }
}

// ── Keys panel ──

fn KeysPanel() -> Element {
    let mut keys = use_resource(fetch_keys);
    let mut name_input = use_signal(String::new);
    let mut generated_key = use_signal(|| None::<GenerateKeyResponse>);
    let mut error = use_signal(String::new);
    let mut pending_revoke = use_signal(|| None::<(String, String)>); // (target, name_for_display)

    let on_generate = move |_| {
        let name = name_input();
        if name.is_empty() {
            return;
        }
        let name_clone = name.clone();
        spawn(async move {
            match generate_key_api(&name_clone).await {
                Ok(resp) => {
                    generated_key.set(Some(resp));
                    name_input.set(String::new());
                    error.set(String::new());
                    keys.restart();
                }
                Err(e) => error.set(e),
            }
        });
    };

    let on_revoke = move |target: String| {
        spawn(async move {
            match revoke_key_api(&target).await {
                Ok(_) => keys.restart(),
                Err(e) => error.set(e),
            }
        });
    };

    rsx! {
        h2 { "Generate New Key" }
        div { class: "form-row",
            input {
                placeholder: "Key name (e.g. hermes)",
                value: "{name_input}",
                oninput: move |ev| name_input.set(ev.value()),
            }
            button { class: "btn", onclick: on_generate, "Generate" }
        }
        if let Some(ref k) = *generated_key.read() {
            // Key display modal
            div {
                style: "position: fixed; top: 0; left: 0; right: 0; bottom: 0; background: #00000088; display: flex; align-items: center; justify-content: center; z-index: 100;",
                onclick: move |_| generated_key.set(None),
                div {
                    style: "background: #161b22; border: 1px solid #30363d; border-radius: 8px; padding: 1.5rem; max-width: 520px; width: 90%;",
                    onclick: move |e| e.stop_propagation(),
                    p { style: "margin-bottom: 0.75rem; font-size: 0.9rem; color: #3fb950; font-weight: 600;",
                        "API key generated (shown once!)"
                    }
                    input {
                        style: "width: 100%; background: #0d1117; border: 1px solid #58a6ff; color: #c9d1d9; padding: 0.5rem 0.75rem; border-radius: 6px; font-family: monospace; font-size: 0.8rem; margin-bottom: 1rem;",
                        value: "{k.key}",
                        readonly: true,
                    }
                    div { style: "display: flex; gap: 0.5rem; justify-content: flex-end;",
                        button {
                            class: "btn",
                            style: "background: #21262d; border: 1px solid #30363d; color: #c9d1d9;",
                            onclick: move |_| generated_key.set(None),
                            "Close"
                        }
                        button {
                            class: "btn",
                            onclick: {
                                let key_val = k.key.clone();
                                move |_| {
                                    let _ = dioxus::document::eval(&format!(
                                        "navigator.clipboard.writeText('{}')", key_val
                                    ));
                                }
                            },
                            "Copy"
                        }
                    }
                }
            }
        }
        if !error().is_empty() {
            div { class: "error-msg", "{error}" }
        }

        h2 { class: "section-head", "Existing Keys" }
        {
            match &*keys.read_unchecked() {
                Some(Ok(list)) if list.is_empty() => rsx! {
                    p { class: "empty", "No keys yet." }
                },
                Some(Ok(list)) => {
                    rsx! {
                        table {
                            thead {
                                tr {
                                    th { "ID" }
                                    th { "Name" }
                                    th { "Partial" }
                                    th { "Created" }
                                    th { "" }
                                }
                            }
                            tbody {
                                for k in list {
                                    tr {
                                        td { "{k.id}" }
                                        td { class: "key-name", "{k.name}" }
                                        td { "{k.partial}" }
                                        td { "{k.created_at}" }
                                        td {
                                            button {
                                                class: "btn btn-danger",
                                                onclick: {
                                                    let target = k.name.clone();
                                                    let display = k.name.clone();
                                                    move |_| pending_revoke.set(Some((target.clone(), display.clone())))
                                                },
                                                "Revoke"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Some(Err(e)) => rsx! { p { class: "error-msg", "Error: {e}" } },
                None => rsx! { p { class: "empty", "Loading..." } },
            }
        }
        // Revoke confirmation modal
        if let Some((ref target, ref display)) = *pending_revoke.read() {
            div {
                style: "position: fixed; top: 0; left: 0; right: 0; bottom: 0; background: #00000088; display: flex; align-items: center; justify-content: center; z-index: 100;",
                onclick: move |_| pending_revoke.set(None),
                div {
                    style: "background: #161b22; border: 1px solid #30363d; border-radius: 8px; padding: 1.5rem; max-width: 400px;",
                    onclick: move |e| e.stop_propagation(),
                    p { style: "margin-bottom: 1rem; font-size: 0.95rem;",
                        "Revoke key " span { style: "color: #58a6ff; font-weight: 600;", "{display}" } "? This cannot be undone."
                    }
                    div { style: "display: flex; gap: 0.5rem; justify-content: flex-end;",
                        button {
                            class: "btn",
                            style: "background: #21262d; border: 1px solid #30363d; color: #c9d1d9;",
                            onclick: move |_| pending_revoke.set(None),
                            "Cancel"
                        }
                        button {
                            class: "btn btn-danger",
                            style: "padding: 0.5rem 1rem;",
                            onclick: {
                                let t = target.clone();
                                let revoke = on_revoke.clone();
                                let mut pending = pending_revoke;
                                move |_| { pending.set(None); revoke(t.clone()); }
                            },
                            "Revoke"
                        }
                    }
                }
            }
        }
    }
}

// ── API helpers ──

async fn fetch_stats() -> Result<UsageSnapshot, String> {
    let resp = Request::get(&format!("{}/stats", api_base()))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json().await.map_err(|e| e.to_string())
}

async fn fetch_keys() -> Result<Vec<KeyInfo>, String> {
    let resp = Request::get(&format!("{}/keys", api_base()))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json().await.map_err(|e| e.to_string())
}

async fn generate_key_api(name: &str) -> Result<GenerateKeyResponse, String> {
    let body = serde_json::to_string(&GenerateKeyRequest {
        name: name.to_string(),
    })
    .map_err(|e| e.to_string())?;
    let resp = Request::post(&format!("{}/keys/generate", api_base()))
        .header("Content-Type", "application/json")
        .body(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        let err = resp.text().await.unwrap_or_default();
        return Err(err);
    }
    resp.json().await.map_err(|e| e.to_string())
}

async fn revoke_key_api(target: &str) -> Result<(), String> {
    let body = serde_json::to_string(&RevokeKeyRequest {
        target: target.to_string(),
    })
    .map_err(|e| e.to_string())?;
    let resp = Request::post(&format!("{}/keys/revoke", api_base()))
        .header("Content-Type", "application/json")
        .body(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        let err = resp.text().await.unwrap_or_default();
        return Err(err);
    }
    Ok(())
}
