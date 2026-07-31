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

/// Handler for OpenAI Responses API (/v1/responses).
/// Translates `input` field to `messages`, forwards to upstream, and
/// transforms the response to Responses API format.
pub async fn responses(
    State(state): State<ProxyState>,
    _headers: HeaderMap,
    key_hash: Option<Extension<crate::auth::AuthInfo>>,
    body: String,
) -> Result<impl IntoResponse> {
    // Remap "input" -> "messages"
    let mut body_json: Value =
        serde_json::from_str(&body).map_err(|e| ProxyError::InvalidRequest(e.to_string()))?;

    if let Some(input) = body_json.get("input").cloned() {
        body_json
            .as_object_mut()
            .unwrap()
            .insert("messages".into(), input);
        body_json.as_object_mut().unwrap().remove("input");
    }

    let remapped =
        serde_json::to_string(&body_json).map_err(|e| ProxyError::InvalidRequest(e.to_string()))?;

    // Look up provider
    let model = body_json
        .get("model")
        .and_then(|m| m.as_str())
        .ok_or_else(|| ProxyError::InvalidRequest("missing 'model' field".into()))?;

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

    // Forward to upstream
    let upstream_response = state
        .client
        .post(provider.chat_url())
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", provider.api_key),
        )
        .header(header::CONTENT_TYPE, "application/json")
        .body(remapped)
        .send()
        .await
        .map_err(|e| ProxyError::UpstreamError(e.to_string()))?;

    let status = upstream_response.status();
    let body_bytes = upstream_response
        .bytes()
        .await
        .map_err(|e| ProxyError::UpstreamError(e.to_string()))?;

    if !status.is_success() {
        let mut response = axum::response::Response::builder().status(status);
        response = response.header(header::CONTENT_TYPE, "application/json");
        return Ok(response.body(Body::from(body_bytes.to_vec())).unwrap());
    }

    // Transform Chat Completions response -> Responses API format
    let upstream_json: Value =
        serde_json::from_slice(&body_bytes).map_err(|e| ProxyError::Internal(e.to_string()))?;

    // Track metrics
    if let Some(Extension(auth)) = key_hash {
        let usage = upstream_json.get("usage");
        let pt = usage
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let ct = usage
            .and_then(|u| u.get("completion_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        state
            .tracker
            .record(&auth.key_hash, &auth.key_name, model, pt, ct);
    }

    let transformed = transform_to_responses(upstream_json);

    let resp_body =
        serde_json::to_string(&transformed).map_err(|e| ProxyError::Internal(e.to_string()))?;

    let response = axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json");
    Ok(response.body(Body::from(resp_body)).unwrap())
}

/// Convert Chat Completions response JSON to Responses API format.
pub fn transform_to_responses(mut json: Value) -> Value {
    let obj = json.as_object_mut().unwrap();

    obj.insert("object".into(), Value::String("response".into()));

    if let Some(choices) = obj.remove("choices") {
        let output: Vec<Value> = choices
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .map(|choice| {
                let msg = choice.get("message").cloned().unwrap_or_default();
                let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
                serde_json::json!({
                    "type": "message",
                    "role": msg.get("role").and_then(|r| r.as_str()).unwrap_or("assistant"),
                    "content": [{
                        "type": "output_text",
                        "text": content
                    }]
                })
            })
            .collect();
        obj.insert("output".into(), Value::Array(output));
    }

    if let Some(usage) = obj.get_mut("usage")
        && let Some(u) = usage.as_object_mut()
    {
        if let Some(pt) = u.remove("prompt_tokens") {
            u.insert("input_tokens".into(), pt);
        }
        if let Some(ct) = u.remove("completion_tokens") {
            u.insert("output_tokens".into(), ct);
        }
    }

    obj.remove("system_fingerprint");
    obj.remove("logprobs");

    json
}

pub async fn chat_completions(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    key_hash: Option<Extension<crate::auth::AuthInfo>>,
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

    let is_streaming = body_json
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

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

    // Extract auth info for metrics
    let auth = key_hash.map(|Extension(a)| a);

    if is_streaming {
        // Streaming: pass through, only count request
        if let Some(ref a) = auth {
            state.tracker.record(&a.key_hash, &a.key_name, model, 0, 0);
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
            .record(&a.key_hash, &a.key_name, model, prompt_tok, comp_tok);
    }

    let mut response = axum::response::Response::builder().status(status);
    if let Some(ct) = upstream_headers.get(header::CONTENT_TYPE) {
        response = response.header(header::CONTENT_TYPE, ct.clone());
    }

    Ok(response.body(Body::from(body_bytes.to_vec())).unwrap())
}
