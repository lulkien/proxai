use serde::Deserialize;
use std::net::SocketAddr;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    #[serde(default)]
    pub providers: Vec<Provider>,
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
}
