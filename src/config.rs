use serde::Deserialize;
use std::net::SocketAddr;

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
}

fn default_timezone() -> String {
    "+00:00".into()
}

#[derive(Debug, Deserialize, Clone)]
pub struct Provider {
    pub name: String,
    /// Base URL, e.g. https://api.deepseek.com/v1
    pub url: String,
    pub api_key: String,
}

impl Provider {
    pub fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.url.trim_end_matches('/'))
    }

    pub fn models_url(&self) -> String {
        format!("{}/models", self.url.trim_end_matches('/'))
    }
}

impl Config {
    pub fn load(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&content)?;
        Ok(config)
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
