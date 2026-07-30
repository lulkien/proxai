use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use std::fmt;

#[derive(Debug)]
pub enum ProxyError {
    InvalidRequest(String),
    UnknownModel(String),
    UpstreamError(String),
    ConfigError(String),
    Internal(String),
}

pub type Result<T> = std::result::Result<T, ProxyError>;

impl fmt::Display for ProxyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(msg) => write!(f, "invalid request: {msg}"),
            Self::UnknownModel(model) => write!(f, "unknown model: {model}"),
            Self::UpstreamError(msg) => write!(f, "upstream error: {msg}"),
            Self::ConfigError(msg) => write!(f, "config error: {msg}"),
            Self::Internal(msg) => write!(f, "internal error: {msg}"),
        }
    }
}

impl std::error::Error for ProxyError {}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            Self::InvalidRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            Self::UnknownModel(model) => (
                StatusCode::BAD_REQUEST,
                format!("model '{model}' not supported by any provider"),
            ),
            Self::UpstreamError(msg) => (
                StatusCode::BAD_GATEWAY,
                format!("upstream provider error: {msg}"),
            ),
            Self::ConfigError(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("configuration error: {msg}"),
            ),
            Self::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.clone()),
        };

        let body = serde_json::json!({
            "error": {
                "message": message,
                "type": "proxy_error",
                "code": status.as_u16(),
            }
        });

        (status, axum::Json(body)).into_response()
    }
}

impl From<Box<dyn std::error::Error>> for ProxyError {
    fn from(e: Box<dyn std::error::Error>) -> Self {
        Self::Internal(e.to_string())
    }
}

impl From<reqwest::Error> for ProxyError {
    fn from(e: reqwest::Error) -> Self {
        Self::UpstreamError(e.to_string())
    }
}

impl From<std::io::Error> for ProxyError {
    fn from(e: std::io::Error) -> Self {
        Self::Internal(e.to_string())
    }
}
