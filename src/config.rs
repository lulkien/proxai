use crate::model_meta;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use tracing::warn;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    #[serde(default)]
    pub providers: Vec<Provider>,
    /// Optional: password for dashboard auth (no auth if unset).
    #[serde(default)]
    pub dashboard_password: Option<String>,
    /// Optional: SQLite database path (defaults to proxai.db).
    #[serde(default)]
    pub db_path: Option<String>,
    /// Timezone offset for dashboard chart, e.g. "+07:00" (default: UTC).
    #[serde(default = "default_timezone")]
    pub timezone: String,
    /// How many days of raw per-request usage rows to keep before folding
    /// them into per-(key, model) cumulative counters (default: 14).
    /// Must exceed the timeline chart's hard-coded 7-day maximum range,
    /// otherwise chart history silently truncates.
    #[serde(default = "default_usage_retention_days")]
    pub usage_retention_days: u64,
    /// Optional per-model properties to advertise on `/v1/models`, keyed by the
    /// model's own name as its provider calls it (`deepseek-v4-pro`,
    /// `nemotron-3.5`). See [`ModelProperties`].
    #[serde(default)]
    pub model_properties: HashMap<String, ModelProperties>,
}

fn default_timezone() -> String {
    "+00:00".into()
}

fn default_usage_retention_days() -> u64 {
    14
}

#[derive(Debug, Deserialize, Clone)]
pub struct Provider {
    pub name: String,
    /// Base URL, e.g. https://api.deepseek.com
    pub url: String,
    pub api_key: String,
    /// Optional allowlist of upstream model ids to advertise.
    /// Empty (default) = advertise every model the provider offers;
    /// non-empty = only these models, when the provider actually has them.
    #[serde(default)]
    pub models: Vec<String>,
}

impl Provider {
    pub fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.url.trim_end_matches('/'))
    }

    pub fn models_url(&self) -> String {
        format!("{}/models", self.url.trim_end_matches('/'))
    }
}

/// Properties to advertise for one model, overlaid on whatever the upstream
/// `/models` entry reported (config wins per key).
///
/// This exists for the holes in provider metadata: a provider that reports no
/// context window, or a wrong one, can be filled in or corrected without
/// proxai inventing values on its own. `context_length` is the canonical
/// window; every other key is advertised verbatim, so the table doubles as a
/// carrier for any extra detail a client should see.
///
/// The table is keyed by the model's own name, not the namespaced id proxai
/// advertises: `[model_properties.deepseek-v4-pro]` or
/// `[model_properties."nemotron-3.5"]`, never `deepseek/deepseek-v4-pro`.
/// (A key naming the advertised or upstream id resolves too, and keys are
/// matched against every provider offering that model.)
#[derive(Debug, Deserialize, Clone, Default, PartialEq)]
pub struct ModelProperties {
    /// Context window to advertise, e.g. `1000000`. Out-of-band values are
    /// dropped with a warning at load (see `Config::sanitize_model_properties`).
    #[serde(default)]
    pub context_length: Option<u64>,
    /// Any other property to advertise, as-is.
    #[serde(default, flatten)]
    pub extra: Map<String, Value>,
}

impl ModelProperties {
    /// Every declared property as one JSON object (`context_length` included),
    /// the shape the advertised entry's properties use.
    pub fn as_map(&self) -> Map<String, Value> {
        let mut map = self.extra.clone();
        if let Some(length) = self.context_length {
            map.insert("context_length".into(), Value::from(length));
        }
        map
    }
}

impl Config {
    pub fn load(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let mut config: Self = toml::from_str(&content)?;
        config.sanitize_model_properties();
        Ok(config)
    }

    /// Drop declared `context_length` values outside the plausible token-window
    /// band. A window outside it is a typo or a byte/parameter count dressed up
    /// as a window, and advertising it hands clients a limit they cannot use.
    /// Every other declared property is advertised verbatim.
    fn sanitize_model_properties(&mut self) {
        for (id, properties) in &mut self.model_properties {
            if let Some(length) = properties.context_length
                && !model_meta::is_plausible_window(length)
            {
                warn!(
                    "model_properties['{id}'].context_length={length} is outside the plausible \
                     token-window band proxai accepts — ignored"
                );
                properties.context_length = None;
            }
        }
    }

    /// Parse timezone like "+07:00" into offset seconds and SQL modifier.
    /// Returns (offset_seconds, sql_modifier) e.g. (25200, "+7 hours").
    pub fn timezone_offset(&self) -> (i32, String) {
        let tz = self.timezone.trim();
        let sign = if tz.starts_with('-') { -1 } else { 1 };
        let tz = tz.trim_start_matches(&['+', '-'][..]);
        let parts: Vec<&str> = tz.split(':').collect();
        let hours: i32 = parts.first().and_then(|h| h.parse().ok()).unwrap_or(0);
        let mins: i32 = parts.get(1).and_then(|m| m.parse().ok()).unwrap_or(0);
        let secs = sign * (hours * 3600 + mins * 60);
        let sql = format!("{}{} hours", if sign < 0 { "-" } else { "+" }, hours.abs());
        (secs, sql)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_tz(tz: &str) -> Config {
        Config {
            bind: "127.0.0.1:3000".parse().unwrap(),
            providers: vec![],
            dashboard_password: None,
            db_path: None,
            timezone: tz.to_string(),
            usage_retention_days: default_usage_retention_days(),
            model_properties: HashMap::new(),
        }
    }

    #[test]
    fn timezone_offset_positive() {
        let (secs, sql) = config_with_tz("+07:00").timezone_offset();
        assert_eq!(secs, 7 * 3600);
        assert_eq!(sql, "+7 hours");
    }

    #[test]
    fn timezone_offset_negative_with_minutes() {
        let (secs, sql) = config_with_tz("-05:30").timezone_offset();
        assert_eq!(secs, -(5 * 3600 + 30 * 60));
        assert_eq!(sql, "-5 hours");
    }

    #[test]
    fn timezone_offset_utc() {
        let (secs, _) = config_with_tz("+00:00").timezone_offset();
        assert_eq!(secs, 0);
    }

    #[test]
    fn timezone_offset_named_zone_falls_back_to_zero() {
        // Named IANA zones are unsupported; parsing must not panic.
        let (secs, _) = config_with_tz("Asia/Ho_Chi_Minh").timezone_offset();
        assert_eq!(secs, 0);
    }

    #[test]
    fn usage_retention_days_defaults_to_14() {
        let c: Config = toml::from_str("bind = '127.0.0.1:3000'\n").unwrap();
        assert_eq!(c.usage_retention_days, 14);
    }

    #[test]
    fn usage_retention_days_parses_override() {
        let c: Config =
            toml::from_str("bind = '127.0.0.1:3000'\nusage_retention_days = 30\n").unwrap();
        assert_eq!(c.usage_retention_days, 30);
    }

    #[test]
    fn provider_models_defaults_to_empty() {
        let p: Provider = toml::from_str(
            "name = 'deepseek'\nurl = 'https://api.deepseek.com'\napi_key = 'sk-x'\n",
        )
        .unwrap();
        assert!(p.models.is_empty(), "models must default to empty");
    }

    #[test]
    fn provider_models_parses_allowlist() {
        let p: Provider = toml::from_str(
            "name = 'deepseek'\nurl = 'https://api.deepseek.com'\napi_key = 'sk-x'\n\
             models = ['deepseek-v4-flash', 'deepseek-v4-pro']\n",
        )
        .unwrap();
        assert_eq!(p.models, vec!["deepseek-v4-flash", "deepseek-v4-pro"]);
    }

    // ── [model_properties] ──

    /// The shape the table is meant to have: a per-model sub-table keyed by the
    /// model's own name (what the provider calls it, and what the config table
    /// uses — not the namespaced `provider/model` id proxai advertises), one
    /// canonical `context_length` plus arbitrary extra detail.
    const MODEL_PROPERTIES_TOML: &str = "bind = '127.0.0.1:3000'\n\
         usage_retention_days = 14\n\
         \n\
         [model_properties]\n\
         [model_properties.deepseek-v4-pro]\n\
         context_length = 1000000\n\
         display_name = \"DeepSeek V4 Pro\"\n\
         max_output_tokens = 65536\n\
         \n\
         [model_properties.\"nemotron-3.5\"]\n\
         context_length = 2000000\n\
         [model_properties.\"nemotron-3.5\".pricing]\n\
         input = 0.5\n";

    #[test]
    fn model_properties_defaults_to_empty() {
        let c: Config = toml::from_str("bind = '127.0.0.1:3000'\n").unwrap();
        assert!(c.model_properties.is_empty());
    }

    #[test]
    fn model_properties_parses_context_length_and_extra_keys() {
        let c: Config = toml::from_str(MODEL_PROPERTIES_TOML).unwrap();
        assert_eq!(c.model_properties.len(), 2);

        let deepseek = &c.model_properties["deepseek-v4-pro"];
        assert_eq!(deepseek.context_length, Some(1_000_000));
        assert_eq!(
            deepseek.extra.get("display_name"),
            Some(&serde_json::json!("DeepSeek V4 Pro"))
        );
        assert_eq!(
            deepseek.extra.get("max_output_tokens"),
            Some(&serde_json::json!(65536))
        );

        // A dotted model name is quoted, a nested table comes along with it.
        let nemotron = &c.model_properties["nemotron-3.5"];
        assert_eq!(nemotron.context_length, Some(2_000_000));
        assert_eq!(nemotron.extra["pricing"]["input"], serde_json::json!(0.5));
    }

    #[test]
    fn model_properties_as_map_merges_context_length_with_extras() {
        let c: Config = toml::from_str(MODEL_PROPERTIES_TOML).unwrap();
        let map = c.model_properties["deepseek-v4-pro"].as_map();

        assert_eq!(map["context_length"], serde_json::json!(1_000_000));
        assert_eq!(map["display_name"], serde_json::json!("DeepSeek V4 Pro"));
        assert_eq!(map.len(), 3);

        let without_window = ModelProperties::default().as_map();
        assert!(without_window.is_empty());
    }

    #[test]
    fn model_properties_require_an_integer_context_length() {
        // A string window is a config error here, not silently coerced.
        let err = toml::from_str::<Config>(
            "bind = '127.0.0.1:3000'\n[model_properties.m]\ncontext_length = '128k'\n",
        );
        assert!(err.is_err(), "a non-integer context_length must not parse");
    }

    #[test]
    fn out_of_band_context_length_override_is_dropped() {
        let mut c: Config = toml::from_str(
            "bind = '127.0.0.1:3000'\n[model_properties.m]\ncontext_length = 500\nkeep = true\n",
        )
        .unwrap();
        c.sanitize_model_properties();

        assert_eq!(c.model_properties["m"].context_length, None);
        assert_eq!(
            c.model_properties["m"].extra.get("keep"),
            Some(&serde_json::json!(true)),
            "only the window is validated; other properties ride through"
        );
    }

    #[test]
    fn in_band_context_length_override_survives_sanitizing() {
        let mut c: Config = toml::from_str(MODEL_PROPERTIES_TOML).unwrap();
        c.sanitize_model_properties();
        assert_eq!(
            c.model_properties["deepseek-v4-pro"].context_length,
            Some(1_000_000)
        );
        assert_eq!(
            c.model_properties["nemotron-3.5"].context_length,
            Some(2_000_000)
        );
    }
}
