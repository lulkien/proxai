//! Model metadata: namespacing, the window keys a provider may report, and the
//! `/v1/models` entry built from an upstream entry's own properties.
//!
//! proxai routes model metadata as well as requests: whatever properties a
//! provider puts on its `/models` entry are re-served verbatim (see
//! [`ModelEntry`]), with only `id`/`object`/`owned_by` owned by the proxy and a
//! canonical `context_length` derived when the provider reported the window
//! under one of its other names.

use serde::Serialize;
use serde_json::{Map, Value};

/// Plausibility band for a token window. Outside it the value is provider junk
/// (a byte count, a parameter count) and must not be advertised as a window.
const MIN_WINDOW: u64 = 1024;
const MAX_WINDOW: u64 = 10_000_000;

/// Window keys in PRECEDENCE order — not payload order, so the window does not
/// depend on how a provider happens to order its JSON. Served/allocated keys
/// precede trained maxima: llama.cpp serves `n_ctx` 131072 while `n_ctx_train`
/// reports 262144, and advertising the trained value lets a client overrun the
/// served window. `max_tokens` is deliberately absent — on passthrough payloads
/// it is a request's output cap, not a window.
const WINDOW_KEYS: &[&str] = &[
    "context_length",
    "max_model_len",
    "context_window",
    "context_size",
    "max_input_tokens",
    "max_context_length",
    "max_seq_len",
    "max_sequence_length",
    "n_ctx",
    "ctx_size",
    "n_ctx_train",
    "max_position_embeddings",
];

/// Keys the proxy owns on an advertised entry; upstream copies are dropped.
const OWNED_KEYS: &[&str] = &["id", "object", "owned_by"];

/// Whether a length could plausibly be a token window — the band clients
/// themselves accept. Shared with the config `[model_properties]` validation,
/// so a value proxai refuses to derive is also refused when declared by hand.
pub fn is_plausible_window(length: u64) -> bool {
    (MIN_WINDOW..=MAX_WINDOW).contains(&length)
}

/// Namespaced id used for routing, usage rows and `/v1/models`:
/// `provider/upstream-id`. A provider whose own ids already carry its name
/// (NVIDIA: `nvidia/nemotron-…`) is not prefixed twice.
pub fn namespace_model(provider: &str, model: &str) -> String {
    if model.starts_with(&format!("{provider}/")) {
        model.to_string()
    } else {
        format!("{provider}/{model}")
    }
}

/// The model's own name, with the provider's namespace removed when the upstream
/// id carries it: `nvidia/nemotron-3.5` -> `nemotron-3.5` for provider `nvidia`.
/// An id that is not prefixed with its provider's name (NVIDIA ids are, DeepSeek
/// ids are not) is already bare and comes back unchanged.
///
/// This is the name a config `[model_properties]` key uses — the model as the
/// provider calls it, not the namespaced id proxai advertises — so a table entry
/// reads `nemotron-3.5`, never `nvidia/nemotron-3.5`.
pub fn bare_model_name<'a>(provider: &str, model: &'a str) -> &'a str {
    // Only a non-empty remainder counts as the bare name: an id that is nothing
    // but the namespace has no model name to reduce to, and must not come back
    // as an empty string that could match an empty table key.
    model
        .strip_prefix(provider)
        .and_then(|rest| rest.strip_prefix('/'))
        .filter(|name| !name.is_empty())
        .unwrap_or(model)
}

/// One advertised model: the upstream id requests are forwarded with, plus the
/// upstream entry's own properties.
#[derive(Debug, Clone)]
pub struct AdvertisedModel {
    pub provider: String,
    /// Upstream model id, forwarded verbatim. Never re-derived by stripping the
    /// namespace: that is ambiguous once a provider's ids carry their own org
    /// prefix (`nvidia/nemotron-x` upstream, `nvidia/nemotron-x` advertised).
    pub upstream_id: String,
    /// The upstream entry's properties minus `id`/`object`/`owned_by`.
    pub properties: Map<String, Value>,
}

impl AdvertisedModel {
    /// Record one upstream `/models` entry.
    pub fn from_entry(provider: &str, upstream_id: &str, entry: &Value) -> Self {
        let mut properties = entry.as_object().cloned().unwrap_or_default();
        for key in OWNED_KEYS {
            properties.remove(*key);
        }
        Self {
            provider: provider.to_string(),
            upstream_id: upstream_id.to_string(),
            properties,
        }
    }

    /// The context window this provider reports for the model, if any.
    pub fn context_length(&self) -> Option<u64> {
        find_context_length(&self.properties).map(|(length, _)| length)
    }

    /// Overlay operator-declared properties (config `[model_properties]`) on the
    /// upstream entry's own. Config wins per key: filling in a window the
    /// provider never reported and correcting one it reported wrong are the same
    /// operation. Returns the keys that were applied (map order = sorted, so
    /// startup logs are stable), for logging.
    ///
    /// `id`/`object`/`owned_by` are refused: they belong to the proxy (see
    /// [`ModelEntry`]), and an overlay carrying them would emit them twice.
    pub fn apply_overrides(&mut self, overrides: &Map<String, Value>) -> Vec<String> {
        let mut applied = Vec::new();
        for (key, value) in overrides {
            if OWNED_KEYS
                .iter()
                .any(|owned| owned.eq_ignore_ascii_case(key))
            {
                tracing::warn!(
                    "{}: property '{key}' is set by the proxy and cannot be overridden — ignored",
                    self.upstream_id
                );
                continue;
            }
            self.properties.insert(key.clone(), value.clone());
            applied.push(key.clone());
        }
        applied
    }

    /// The `/v1/models` entry for this model: our `id`/`object`/`owned_by` plus
    /// every upstream property, with a derived `context_length` only when the
    /// provider did not report one under that name.
    pub fn entry(&self, id: &str) -> ModelEntry {
        let mut properties = self.properties.clone();
        let upstream_reported_window = properties
            .keys()
            .any(|k| k.eq_ignore_ascii_case("context_length"));
        if !upstream_reported_window && let Some(length) = self.context_length() {
            properties.insert("context_length".into(), Value::from(length));
        }
        ModelEntry {
            id: id.to_string(),
            object: "model".into(),
            owned_by: self.provider.clone(),
            properties,
        }
    }
}

/// One `/v1/models` item. `properties` is flattened, so every upstream property
/// (`meta`, `max_model_len`, pricing, …) rides along untouched.
#[derive(Debug, Serialize)]
pub struct ModelEntry {
    pub id: String,
    pub object: String,
    pub owned_by: String,
    #[serde(flatten)]
    pub properties: Map<String, Value>,
}

/// The OpenAI models envelope: `{"object": "list", "data": [...]}`.
#[derive(Debug, Serialize)]
pub struct ModelList {
    pub object: String,
    pub data: Vec<ModelEntry>,
}

/// The reported window and the key it came from (`n_ctx`, `max_model_len`, …),
/// used for startup logging.
pub fn find_context_length(properties: &Map<String, Value>) -> Option<(u64, &'static str)> {
    WINDOW_KEYS.iter().find_map(|key| {
        properties
            .iter()
            .find_map(|(name, value)| {
                if name.eq_ignore_ascii_case(key) {
                    plausible(value)
                } else {
                    find_int(value, key)
                }
            })
            .map(|length| (length, *key))
    })
}

/// Depth-first search for `key` under `value` (objects and arrays both).
fn find_int(value: &Value, key: &str) -> Option<u64> {
    match value {
        Value::Object(map) => map.iter().find_map(|(name, nested)| {
            if name.eq_ignore_ascii_case(key) {
                plausible(nested)
            } else {
                find_int(nested, key)
            }
        }),
        Value::Array(items) => items.iter().find_map(|item| find_int(item, key)),
        _ => None,
    }
}

/// A plausible token window: an integer (numeric strings tolerated, commas
/// stripped) inside the band clients themselves accept.
fn plausible(value: &Value) -> Option<u64> {
    let length = match value {
        Value::Number(n) => n.as_u64()?,
        Value::String(s) => s.trim().replace(',', "").parse().ok()?,
        _ => return None,
    };
    is_plausible_window(length).then_some(length)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn properties(entry: Value) -> Map<String, Value> {
        AdvertisedModel::from_entry("test", "id", &entry).properties
    }

    // ── namespacing ──

    #[test]
    fn namespace_model_prepends_provider() {
        assert_eq!(
            namespace_model("deepseek", "deepseek-flash"),
            "deepseek/deepseek-flash"
        );
    }

    #[test]
    fn namespace_model_dedups_matching_provider_prefix() {
        assert_eq!(
            namespace_model("nvidia", "nvidia/nemotron-3-super-120b-a12b"),
            "nvidia/nemotron-3-super-120b-a12b"
        );
    }

    #[test]
    fn namespace_model_keeps_other_org_prefix() {
        // An org prefix that is not the provider name is data, not duplication.
        assert_eq!(
            namespace_model("openrouter", "x-ai/grok-4.6"),
            "openrouter/x-ai/grok-4.6"
        );
    }

    #[test]
    fn namespace_model_does_not_false_match_partial_name() {
        assert_eq!(
            namespace_model("deep", "deepseek/deepseek-chat"),
            "deep/deepseek/deepseek-chat"
        );
    }

    // ── bare model name (config [model_properties] keys) ──

    #[test]
    fn bare_model_name_strips_a_provider_prefix_the_id_carries() {
        assert_eq!(
            bare_model_name("nvidia", "nvidia/nemotron-3-super-120b-a12b"),
            "nemotron-3-super-120b-a12b"
        );
        assert_eq!(bare_model_name("nvidia", "nemotron-3.5"), "nemotron-3.5");
    }

    #[test]
    fn bare_model_name_keeps_ids_that_are_not_prefixed_by_their_provider() {
        assert_eq!(
            bare_model_name("deepseek", "deepseek-v4-pro"),
            "deepseek-v4-pro"
        );
        // An org prefix that is not the provider name is part of the name.
        assert_eq!(
            bare_model_name("openrouter", "x-ai/grok-4.6"),
            "x-ai/grok-4.6"
        );
    }

    #[test]
    fn bare_model_name_does_not_strip_a_partial_provider_match() {
        assert_eq!(
            bare_model_name("deep", "deepseek/deepseek-chat"),
            "deepseek/deepseek-chat"
        );
        // A provider named like its own model's prefix leaves nothing behind.
        assert_eq!(bare_model_name("nvidia", "nvidia/"), "nvidia/");
    }

    // ── window precedence ──

    #[test]
    fn window_keys_rank_served_before_trained() {
        let served = WINDOW_KEYS.iter().position(|k| *k == "n_ctx").unwrap();
        let trained = WINDOW_KEYS
            .iter()
            .position(|k| *k == "n_ctx_train")
            .unwrap();
        assert!(served < trained, "n_ctx must outrank n_ctx_train");
        let positioned = WINDOW_KEYS
            .iter()
            .position(|k| *k == "max_position_embeddings")
            .unwrap();
        assert!(
            served < positioned,
            "n_ctx must outrank max_position_embeddings"
        );
    }

    #[test]
    fn plausible_accepts_number_and_comma_string() {
        assert_eq!(plausible(&json!(131072)), Some(131072));
        assert_eq!(plausible(&json!("131,072")), Some(131072));
        assert_eq!(plausible(&json!(" 32768 ")), Some(32768));
    }

    #[test]
    fn plausible_rejects_bool_garbage_and_out_of_band() {
        assert_eq!(plausible(&json!(true)), None);
        assert_eq!(plausible(&json!("lots")), None);
        assert_eq!(plausible(&json!(0)), None);
        assert_eq!(plausible(&json!(512)), None);
        assert_eq!(plausible(&json!(20_000_000)), None);
    }

    // ── window extraction, one per real provider shape ──

    #[test]
    fn minimal_openai_entry_has_no_window() {
        // DeepSeek and NVIDIA NIM both report only these keys.
        let entry = json!({"id": "deepseek-flash", "object": "model", "owned_by": "deepseek"});
        assert_eq!(find_context_length(&properties(entry)), None);
    }

    #[test]
    fn llamacpp_meta_uses_served_n_ctx() {
        let entry = json!({
            "id": "bonsai-27b",
            "object": "model",
            "owned_by": "llamacpp",
            "meta": {"n_vocab": 248320, "n_ctx": 131072, "n_ctx_train": 262144},
        });
        assert_eq!(
            find_context_length(&properties(entry)),
            Some((131072, "n_ctx")),
            "the served window must win over the trained maximum"
        );
    }

    #[test]
    fn vllm_entry_reads_max_model_len() {
        let entry = json!({"id": "qwen3", "max_model_len": 32768});
        assert_eq!(
            find_context_length(&properties(entry)),
            Some((32768, "max_model_len"))
        );
    }

    #[test]
    fn openrouter_entry_reads_context_length() {
        let entry = json!({
            "id": "x-ai/grok-4.6",
            "context_length": 2_000_000,
            "top_provider": {"context_length": 131072, "max_completion_tokens": 4096},
        });
        assert_eq!(
            find_context_length(&properties(entry)),
            Some((2_000_000, "context_length"))
        );
    }

    #[test]
    fn anthropic_entry_reads_max_input_tokens() {
        let entry = json!({"id": "claude-x", "max_input_tokens": 1_000_000, "max_tokens": 128_000});
        assert_eq!(
            find_context_length(&properties(entry)),
            Some((1_000_000, "max_input_tokens")),
            "max_tokens is an output cap, never the window"
        );
    }

    #[test]
    fn nested_and_array_values_are_found() {
        let entry = json!({
            "id": "lmstudio-model",
            "loaded_instances": [{"config": {"context_length": 65536}}],
        });
        assert_eq!(
            find_context_length(&properties(entry)),
            Some((65536, "context_length"))
        );
    }

    // ── the advertised entry ──

    #[test]
    fn entry_passes_unknown_properties_through() {
        let entry = json!({
            "id": "bonsai-27b",
            "object": "model",
            "owned_by": "llamacpp",
            "aliases": ["bonsai-27b"],
            "tags": [],
            "created": 1789747893,
            "meta": {"n_ctx": 131072},
        });
        let model = AdvertisedModel::from_entry("localai", "bonsai-27b", &entry);
        let out = serde_json::to_value(model.entry("localai/bonsai-27b")).unwrap();

        assert_eq!(out["id"], "localai/bonsai-27b");
        assert_eq!(out["object"], "model");
        assert_eq!(out["owned_by"], "localai");
        assert_eq!(out["context_length"], 131072);
        assert_eq!(out["aliases"], json!(["bonsai-27b"]));
        assert_eq!(out["tags"], json!([]));
        assert_eq!(out["created"], 1789747893);
        assert_eq!(out["meta"]["n_ctx"], 131072);
    }

    #[test]
    fn entry_overrides_upstream_id_object_owner() {
        let entry = json!({"id": "bonsai-27b", "object": "model", "owned_by": "llamacpp"});
        let model = AdvertisedModel::from_entry("localai", "bonsai-27b", &entry);
        let out = serde_json::to_value(model.entry("localai/bonsai-27b")).unwrap();

        let obj = out.as_object().unwrap();
        assert_eq!(
            obj.len(),
            3,
            "upstream id/object/owned_by must not be emitted twice: {out}"
        );
        assert_eq!(obj["owned_by"], "localai");
    }

    #[test]
    fn entry_keeps_upstream_context_length() {
        let entry = json!({"id": "m", "context_length": 2_000_000, "max_model_len": 32768});
        let model = AdvertisedModel::from_entry("p", "m", &entry);
        let out = serde_json::to_value(model.entry("p/m")).unwrap();
        assert_eq!(out["context_length"], 2_000_000);
        assert_eq!(out["max_model_len"], 32768);
    }

    #[test]
    fn entry_omits_context_length_when_unknown() {
        let entry = json!({"id": "m", "object": "model", "owned_by": "deepseek"});
        let model = AdvertisedModel::from_entry("deepseek", "m", &entry);
        let out = serde_json::to_value(model.entry("deepseek/m")).unwrap();
        assert!(
            out.get("context_length").is_none(),
            "unknown window must not be advertised: {out}"
        );
    }

    #[test]
    fn upstream_id_is_stored_not_stripped() {
        let entry = json!({"id": "nvidia/nemotron-3-super-120b-a12b"});
        let model =
            AdvertisedModel::from_entry("nvidia", "nvidia/nemotron-3-super-120b-a12b", &entry);
        assert_eq!(model.upstream_id, "nvidia/nemotron-3-super-120b-a12b");
    }

    // ── config overrides ([model_properties]) ──

    fn overrides(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn override_supplies_a_window_the_provider_never_reported() {
        // The fallback case: DeepSeek-shaped entry, window from config.
        let entry = json!({"id": "m", "object": "model", "owned_by": "deepseek"});
        let mut model = AdvertisedModel::from_entry("deepseek", "m", &entry);
        assert_eq!(model.context_length(), None);

        let applied = model.apply_overrides(&overrides(&[("context_length", json!(1_000_000))]));

        assert_eq!(applied, vec!["context_length"]);
        assert_eq!(model.context_length(), Some(1_000_000));
        let out = serde_json::to_value(model.entry("deepseek/m")).unwrap();
        assert_eq!(out["context_length"], 1_000_000);
        // Proxy-owned fields still come from the proxy, exactly once.
        assert_eq!(out["id"], "deepseek/m");
        assert_eq!(out["owned_by"], "deepseek");
        assert_eq!(out.as_object().unwrap().len(), 4);
    }

    #[test]
    fn override_corrects_a_window_the_provider_reported() {
        let entry = json!({"id": "m", "context_length": 8192});
        let mut model = AdvertisedModel::from_entry("p", "m", &entry);

        model.apply_overrides(&overrides(&[("context_length", json!(131072))]));

        assert_eq!(model.context_length(), Some(131072));
        let out = serde_json::to_value(model.entry("p/m")).unwrap();
        assert_eq!(out["context_length"], 131072);
    }

    #[test]
    fn override_carries_arbitrary_properties_alongside_upstream_ones() {
        let entry = json!({"id": "m", "aliases": ["m"], "meta": {"n_vocab": 248320}});
        let mut model = AdvertisedModel::from_entry("p", "m", &entry);

        model.apply_overrides(&overrides(&[
            ("display_name", json!("My Model")),
            ("pricing", json!({"input": 0.5})),
        ]));

        let out = serde_json::to_value(model.entry("p/m")).unwrap();
        assert_eq!(out["display_name"], "My Model");
        assert_eq!(out["pricing"]["input"], 0.5);
        assert_eq!(
            out["aliases"],
            json!(["m"]),
            "upstream properties the config does not name ride through"
        );
        assert_eq!(out["meta"]["n_vocab"], 248320);
    }

    #[test]
    fn override_refuses_keys_the_proxy_owns() {
        // Emitting id/object/owned_by twice would produce an entry with
        // duplicate JSON keys.
        let entry = json!({"id": "m", "object": "model", "owned_by": "deepseek"});
        let mut model = AdvertisedModel::from_entry("deepseek", "m", &entry);

        let applied = model.apply_overrides(&overrides(&[
            ("id", json!("hijacked")),
            ("object", json!("hijacked")),
            ("owned_by", json!("hijacked")),
            ("context_length", json!(65536)),
        ]));

        assert_eq!(applied, vec!["context_length"]);
        let out = serde_json::to_value(model.entry("deepseek/m")).unwrap();
        assert_eq!(out["id"], "deepseek/m");
        assert_eq!(out["object"], "model");
        assert_eq!(out["owned_by"], "deepseek");
        assert_eq!(out.as_object().unwrap().len(), 4);
    }

    #[test]
    fn override_of_an_unreported_window_stays_within_the_proxy_band() {
        // The band is shared with config validation; a value outside it is
        // never derived here, and is dropped at config load before it reaches
        // this function.
        assert!(is_plausible_window(1_000_000));
        assert!(!is_plausible_window(500));
        assert!(!is_plausible_window(20_000_000));
        assert!(is_plausible_window(MIN_WINDOW));
        assert!(is_plausible_window(MAX_WINDOW));
    }
}
