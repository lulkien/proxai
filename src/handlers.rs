use crate::{error::ProxyError, error::Result, server::ProxyState};
use axum::{
    Extension, Json,
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use futures::StreamExt;
use serde_json::Value;
use tracing::{error, info};

use crate::server::{ModelEntry, ModelList};

pub async fn list_models(State(state): State<ProxyState>) -> impl IntoResponse {
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

pub async fn chat_completions(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    key_hash: Option<Extension<crate::auth::AuthInfo>>,
    body: String,
) -> Result<impl IntoResponse> {
    let mut body_json: Value =
        serde_json::from_str(&body).map_err(|e| ProxyError::InvalidRequest(e.to_string()))?;

    let model = body_json
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| ProxyError::InvalidRequest("missing 'model' field".into()))?;

    info!("Request for model: {model}");

    let provider = {
        let owner = state
            .models
            .get(&model)
            .ok_or_else(|| ProxyError::UnknownModel(model.clone()))?;
        state
            .config
            .providers
            .iter()
            .find(|p| &p.name == owner)
            .ok_or_else(|| ProxyError::UnknownModel(model.clone()))?
    };

    // Strip namespace prefix for upstream (deepseek/gpt-4o -> gpt-4o)
    let prefix = format!("{}/", provider.name);
    let upstream_model = model.strip_prefix(&prefix).unwrap_or(&model);
    if upstream_model != model {
        body_json
            .as_object_mut()
            .unwrap()
            .insert("model".into(), Value::String(upstream_model.to_string()));
    }

    info!(
        "Routing to provider: {} -> {}",
        provider.name,
        provider.chat_url()
    );

    let is_streaming = body_json
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let upstream_body = serde_json::to_string(&body_json).unwrap_or(body);

    let mut upstream_request = state
        .client
        .post(provider.chat_url())
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", provider.api_key),
        )
        .header(header::CONTENT_TYPE, "application/json")
        .body(upstream_body.clone());

    if let Some(stream_val) = headers.get("x-stream") {
        upstream_request = upstream_request.header("x-stream", stream_val);
    }

    let upstream_response = upstream_request.send().await.map_err(|e| {
        error!("Upstream request failed: {e}");
        ProxyError::UpstreamError(e.to_string())
    })?;

    let status = upstream_response.status();
    let upstream_headers = upstream_response.headers().clone();

    // Extract auth info for metrics
    let auth = key_hash.map(|Extension(a)| a);

    if is_streaming {
        // Streaming: pass through, only count request
        if let Some(ref a) = auth {
            state.tracker.record(&a.key_hash, &a.key_name, &model, 0, 0);
        }

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

        return Ok(response.body(Body::from_stream(body_stream)).unwrap());
    }

    // Non-streaming: buffer response to count tokens
    let body_bytes = upstream_response
        .bytes()
        .await
        .map_err(|e| ProxyError::UpstreamError(e.to_string()))?;

    // Parse usage from upstream response
    if let Some(ref a) = auth {
        let (prompt_tok, comp_tok) = if status.is_success() {
            if let Ok(json) = serde_json::from_slice::<Value>(&body_bytes) {
                let usage = json.get("usage");
                let pt = usage
                    .and_then(|u| u.get("prompt_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let ct = usage
                    .and_then(|u| u.get("completion_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                (pt, ct)
            } else {
                (0, 0)
            }
        } else {
            (0, 0)
        };
        state
            .tracker
            .record(&a.key_hash, &a.key_name, &model, prompt_tok, comp_tok);
    }

    let mut response = axum::response::Response::builder().status(status);
    if let Some(ct) = upstream_headers.get(header::CONTENT_TYPE) {
        response = response.header(header::CONTENT_TYPE, ct.clone());
    }

    Ok(response.body(Body::from(body_bytes.to_vec())).unwrap())
}
