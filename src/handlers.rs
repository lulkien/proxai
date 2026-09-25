use crate::{
    error::{ProxyError, Result},
    model_meta::{AdvertisedModel, ModelEntry, ModelList},
    server::ProxyState,
};
use axum::{
    Extension, Json,
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use futures::StreamExt;
use serde_json::Value;
use std::collections::HashMap;
use tracing::{debug, error};

/// Every advertised model, each entry re-serving the upstream `/models`
/// entry's own properties (see `model_meta::AdvertisedModel::entry`).
pub fn model_entries(models: &HashMap<String, AdvertisedModel>) -> Vec<ModelEntry> {
    models.iter().map(|(id, model)| model.entry(id)).collect()
}

pub async fn list_models(State(state): State<ProxyState>) -> impl IntoResponse {
    let list = ModelList {
        object: "list".into(),
        data: model_entries(&state.models),
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

    debug!("Request for model: {model}");

    // Advertised record: which provider to route to, and the upstream id to
    // forward. The stored id is authoritative — the namespaced id cannot be
    // stripped back to it, because a provider's own ids may already carry its
    // name (nvidia/nemotron-…).
    let advertised = state
        .models
        .get(&model)
        .ok_or_else(|| ProxyError::UnknownModel(model.clone()))?;
    let provider = state
        .config
        .providers
        .iter()
        .find(|p| p.name == advertised.provider)
        .ok_or_else(|| ProxyError::UnknownModel(model.clone()))?;

    if advertised.upstream_id != model
        && let Some(obj) = body_json.as_object_mut()
    {
        obj.insert(
            "model".into(),
            Value::String(advertised.upstream_id.clone()),
        );
    }

    debug!(
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
            let mut capture = UsageCapture::default();
            let mut stream = upstream_response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        // Accounting tee only — `bytes` is sent on unchanged, so
                        // the client body stays byte-transparent either way.
                        capture.push(&bytes);
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
                let (pt, ct) = capture.finish();
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

/// Trailing window of a streamed SSE response, kept for token accounting.
///
/// A chunk is whatever one socket read produced: it can begin and end
/// anywhere, including inside a line or a UTF-8 code point. Bytes are
/// therefore held as bytes and decoded *per line* at parse time. Decoding a
/// chunk on its own and dropping it when that fails lost the `usage` event
/// whenever a boundary landed inside a multi-byte character — the chunks
/// either side of such a boundary are both invalid UTF-8 alone, so both
/// vanished and the request was recorded as 0/0. SSE `usage` arrives last,
/// so only the tail is kept.
#[derive(Default)]
struct UsageCapture {
    pending: Vec<u8>,
    chunks: u64,
    mid_line_chunks: u64,
}

/// Cap on the captured tail. Upstream sends usage in the final event, so a
/// window this size always contains it.
const USAGE_CAPTURE_MAX: usize = 64 * 1024;

impl UsageCapture {
    fn push(&mut self, chunk: &[u8]) {
        self.pending.extend_from_slice(chunk);
        self.chunks += 1;
        if self.pending.len() > USAGE_CAPTURE_MAX {
            self.trim();
        }
        // Bytes after the last `\n` are an unfinished line, held until the
        // chunk that completes it arrives. Counting these is the only way to
        // see that real traffic exercises a mid-line boundary.
        if !self.pending.is_empty() && self.pending.last() != Some(&b'\n') {
            self.mid_line_chunks += 1;
        }
    }

    /// Drop everything before the last `USAGE_CAPTURE_MAX` bytes, starting at
    /// the first line boundary inside the window so whole lines are kept.
    fn trim(&mut self) {
        let cut = self.pending.len() - USAGE_CAPTURE_MAX;
        let start = match self.pending[cut..].iter().position(|b| *b == b'\n') {
            Some(offset) => cut + offset + 1,
            // No terminator in the window (one enormous line): keep the raw
            // tail — the parser skips a fragment that cannot be a `data:` line.
            None => cut,
        };
        self.pending.drain(..start);
    }

    /// Tracked tokens from the captured tail. A last line without a
    /// terminator is still a line; the stream may have ended mid-line.
    fn finish(self) -> (u64, u64) {
        if self.mid_line_chunks > 0 {
            debug!(
                chunks = self.chunks,
                mid_line_chunks = self.mid_line_chunks,
                held = self.pending.len(),
                "streaming usage capture saw chunk(s) ending mid-line"
            );
        }
        sse_usage_tokens(&self.pending)
    }
}

/// Extract (prompt_tokens, completion_tokens) from a streamed SSE body.
/// Upstream sends usage in a final `data: {...}` event when
/// `stream_options.include_usage` is enabled.
///
/// Decodes per line: a line that is not valid UTF-8 is not something that can
/// be read, and skipping it must not discard the lines around it.
fn sse_usage_tokens(body: &[u8]) -> (u64, u64) {
    let mut prompt = 0u64;
    let mut completion = 0u64;
    for raw in body.split(|b| *b == b'\n') {
        let Ok(line) = std::str::from_utf8(raw) else {
            continue;
        };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_entries_re_serve_upstream_properties() {
        let entry = serde_json::json!({
            "id": "bonsai-27b",
            "object": "model",
            "owned_by": "llamacpp",
            "aliases": ["bonsai-27b"],
            "meta": {"n_ctx": 131072, "n_ctx_train": 262144},
        });
        let models = HashMap::from([(
            "localai/bonsai-27b".to_string(),
            AdvertisedModel::from_entry("localai", "bonsai-27b", &entry),
        )]);

        let list = ModelList {
            object: "list".into(),
            data: model_entries(&models),
        };
        let payload = serde_json::to_value(list).unwrap();
        let item = &payload["data"][0];

        assert_eq!(item["id"], "localai/bonsai-27b");
        assert_eq!(item["object"], "model");
        assert_eq!(item["owned_by"], "localai");
        assert_eq!(item["context_length"], 131072);
        assert_eq!(item["aliases"][0], "bonsai-27b");
        assert_eq!(item["meta"]["n_ctx"], 131072);
    }

    #[test]
    fn model_entries_omit_unknown_context_length() {
        // DeepSeek-shaped entry: nothing to advertise but id/object/owner.
        let entry = serde_json::json!({"id": "m", "object": "model", "owned_by": "deepseek"});
        let models = HashMap::from([(
            "deepseek/m".to_string(),
            AdvertisedModel::from_entry("deepseek", "m", &entry),
        )]);

        let payload = serde_json::to_value(model_entries(&models)).unwrap();
        assert!(payload[0].get("context_length").is_none());
        assert_eq!(payload[0]["id"], "deepseek/m");
    }

    #[test]
    fn sse_usage_tokens_parses_final_usage_chunk() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\" there\"}}]}\n\n\
                    data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7}}\n\n\
                    data: [DONE]\n\n";
        assert_eq!(sse_usage_tokens(body.as_bytes()), (11, 7));
    }

    #[test]
    fn sse_usage_tokens_no_usage_is_zero() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        assert_eq!(sse_usage_tokens(body.as_bytes()), (0, 0));
    }

    #[test]
    fn sse_usage_tokens_skips_only_the_undecodable_line() {
        // A line that is not valid UTF-8 cannot be read, but skipping it must
        // not discard the lines around it.
        let body: Vec<u8> = [
            &b"data: {\"choices\":[{\"delta\":{\"content\":\"\xff\xfe\"}}]}\n\n"[..],
            &b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":4}}\n\n"
                [..],
        ]
        .concat();
        assert_eq!(sse_usage_tokens(&body), (3, 4));
    }

    /// Feed chunks through the capture exactly as the streaming task does.
    fn capture_of(chunks: &[&[u8]]) -> (u64, u64) {
        let mut capture = UsageCapture::default();
        for chunk in chunks {
            capture.push(chunk);
        }
        capture.finish()
    }

    fn find(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("needle is present in the fixture")
    }

    /// A streamed reply shaped like the real thing: non-ASCII content events,
    /// then upstream's `usage` event, then `[DONE]`.
    fn stream_with_multibyte_content() -> Vec<u8> {
        [
            "data: {\"id\":\"1\",\"choices\":[{\"delta\":{\"content\":\"café ☕\"}}]}\n\n",
            "data: {\"id\":\"1\",\"choices\":[{\"delta\":{\"content\":\"!\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7}}\n\n",
            "data: [DONE]\n\n",
        ]
        .concat()
        .into_bytes()
    }

    #[test]
    fn usage_survives_chunk_split_inside_a_multibyte_char() {
        // The boundary — a socket read, not a line — lands inside 'é'. Both
        // halves are invalid UTF-8 on their own, which used to drop the whole
        // capture, usage event included, and record the request as 0/0.
        let stream = stream_with_multibyte_content();
        let split = find(&stream, "é".as_bytes()) + 1;
        assert!(std::str::from_utf8(&stream[..split]).is_err());

        assert_eq!(capture_of(&[&stream[..split], &stream[split..]]), (11, 7));
    }

    #[test]
    fn usage_survives_split_inside_the_usage_event() {
        let stream = stream_with_multibyte_content();
        let split = find(&stream, b"\"prompt_tokens\"") + 3;

        assert_eq!(capture_of(&[&stream[..split], &stream[split..]]), (11, 7));
    }

    #[test]
    fn usage_survives_a_read_that_coalesces_content_and_usage() {
        // One socket read carrying the tail of a content event plus the whole
        // usage event, with the previous boundary mid-character.
        let stream = stream_with_multibyte_content();
        let split = find(&stream, "é".as_bytes()) + 1;
        let usage = find(&stream, b"data: {\"choices\":[],\"usage\"");
        let mid = usage + 40;

        assert_eq!(
            capture_of(&[&stream[..split], &stream[split..mid], &stream[mid..]]),
            (11, 7)
        );
    }

    #[test]
    fn usage_survives_every_single_split_offset() {
        let stream = stream_with_multibyte_content();
        let mut mid_character_offsets = 0;

        for offset in 1..stream.len() {
            if std::str::from_utf8(&stream[..offset]).is_err() {
                mid_character_offsets += 1;
            }
            assert_eq!(
                capture_of(&[&stream[..offset], &stream[offset..]]),
                (11, 7),
                "split at byte {offset} lost the usage event"
            );
        }

        // The fixture must actually contain the offsets that exercise the
        // multi-byte case, or this test proves nothing about it.
        assert!(mid_character_offsets >= 2);
    }

    #[test]
    fn capture_counts_only_chunks_that_end_mid_line() {
        let mut capture = UsageCapture::default();
        capture.push(b"");
        capture.push(b"data: {\"choices\":[]}\n\n");
        assert_eq!(capture.mid_line_chunks, 0);

        capture.push(b"data: {\"choi");
        assert_eq!(capture.mid_line_chunks, 1);
    }

    #[test]
    fn capture_keeps_a_bounded_tail_of_whole_lines() {
        let filler = b"data: {\"choices\":[{\"delta\":{\"content\":\"...\"}}]}\n\n";
        let mut stream = b"junk line without usage\n".to_vec();
        let mut capture = UsageCapture::default();
        capture.push(&stream);
        for _ in 0..4096 {
            stream.extend_from_slice(filler);
            capture.push(filler);
        }

        assert!(capture.pending.len() <= USAGE_CAPTURE_MAX);
        // The window is a suffix of what was pushed that starts right after a
        // terminator — whole lines, never a fragment of one.
        assert!(stream.ends_with(&capture.pending));
        let dropped = stream.len() - capture.pending.len();
        assert!(dropped > 0, "the fixture must exceed the window");
        assert_eq!(stream[dropped - 1], b'\n');

        capture.push(
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7}}\n\n",
        );
        assert_eq!(capture.finish(), (11, 7));
    }

    #[test]
    fn capture_keeps_a_raw_tail_when_the_window_has_no_line_boundary() {
        let mut capture = UsageCapture::default();
        let blob = vec![b'x'; USAGE_CAPTURE_MAX + 10];
        capture.push(&blob);

        assert_eq!(capture.pending.len(), USAGE_CAPTURE_MAX);
        assert_eq!(capture.finish(), (0, 0));
    }
}
