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

    // Ask upstream to include token usage in the final SSE chunk so we can
    // count tokens for streaming requests too.
    if is_streaming {
        let stream_opts = body_json.as_object_mut().and_then(|o| {
            o.entry("stream_options")
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
        });
        if let Some(so) = stream_opts {
            so.entry("include_usage").or_insert(Value::Bool(true));
        }
    }

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
        // Stream the response through while capturing SSE `usage` chunks so we
        // can still count tokens (upstream sends usage in a final data: chunk
        // when stream_options.include_usage is set).
        let (tx, rx) = tokio::sync::mpsc::channel::<
            std::result::Result<axum::body::Bytes, std::io::Error>,
        >(16);

        let tracker = state.tracker.clone();
        let model = model.clone();
        let auth = auth.clone();
        tokio::spawn(async move {
            let mut buf = String::new();
            let mut stream = upstream_response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        if let Ok(s) = std::str::from_utf8(&bytes) {
                            append_tail(&mut buf, s, 64 * 1024);
                        }
                        if tx.send(Ok(bytes)).await.is_err() {
                            break; // client disconnected
                        }
                    }
                    Err(e) => {
                        error!("Stream error: {e}");
                        let _ = tx.send(Err(std::io::Error::other(e))).await;
                        break;
                    }
                }
            }
            // Count tokens from whatever usage chunk(s) we captured.
            if let Some(a) = auth {
                let (pt, ct) = sse_usage_tokens(&buf);
                tracker.record(&a.key_hash, &a.key_name, &model, pt, ct);
            }
        });

        let body_stream = futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });

        let mut response = axum::response::Response::builder().status(status);

        if let Some(ct) = upstream_headers.get(header::CONTENT_TYPE) {
            response = response.header(header::CONTENT_TYPE, ct.clone());
        }
        if let Some(te) = upstream_headers.get(header::TRANSFER_ENCODING) {
            response = response.header(header::TRANSFER_ENCODING, te.clone());
        }

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

/// Extract (prompt_tokens, completion_tokens) from an SSE response body.
/// Upstream sends usage in a final `data: {...}` chunk when
/// `stream_options.include_usage` is enabled.
fn sse_usage_tokens(body: &str) -> (u64, u64) {
    let mut prompt = 0u64;
    let mut completion = 0u64;
    for line in body.lines() {
        let line = line.trim();
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim_start();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(json) = serde_json::from_str::<Value>(data)
            && let Some(usage) = json.get("usage")
        {
            if let Some(v) = usage.get("prompt_tokens").and_then(|v| v.as_u64()) {
                prompt = v;
            }
            if let Some(v) = usage.get("completion_tokens").and_then(|v| v.as_u64()) {
                completion = v;
            }
        }
    }
    (prompt, completion)
}

/// Append `s` to `buf`, keeping only the trailing `max` bytes (UTF-8 safe).
/// SSE usage arrives in the final chunk, so only the tail matters for counting.
fn append_tail(buf: &mut String, s: &str, max: usize) {
    buf.push_str(s);
    if buf.len() <= max {
        return;
    }
    let mut start = buf.len() - max;
    while !buf.is_char_boundary(start) {
        start -= 1;
    }
    buf.drain(..start);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_usage_tokens_parses_final_usage_chunk() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\" there\"}}]}\n\n\
                    data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7}}\n\n\
                    data: [DONE]\n\n";
        assert_eq!(sse_usage_tokens(body), (11, 7));
    }

    #[test]
    fn sse_usage_tokens_no_usage_is_zero() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        assert_eq!(sse_usage_tokens(body), (0, 0));
    }

    #[test]
    fn append_tail_truncates_utf8_safely() {
        let mut buf = String::new();
        append_tail(&mut buf, "abc", 10);
        assert_eq!(buf, "abc");

        // "aaéé" is 6 bytes (é = 2 bytes); truncating to 3 bytes must back up
        // to a char boundary rather than splitting the multi-byte char.
        let mut buf = String::new();
        append_tail(&mut buf, "aaéé", 3);
        assert_eq!(buf, "éé");
    }
}
