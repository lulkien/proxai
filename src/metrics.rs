use crate::storage::Storage;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};

/// Snapshot returned to the dashboard / admin CLI.
/// Built from Storage data at query time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub keys: Vec<KeyUsageSnapshot>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyUsageSnapshot {
    pub key_name: String,
    pub total_requests: u64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub last_used: Option<String>,
    pub model_usage: HashMap<String, ModelUsageSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelUsageSnapshot {
    pub requests: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

#[derive(Clone)]
pub struct UsageTracker {
    storage: Arc<Storage>,
}

impl UsageTracker {
    pub fn new(storage: Arc<Storage>) -> Self {
        Self { storage }
    }

    /// Record a completed request. Persists to SQLite.
    pub fn record(
        &self,
        key_hash: &str,
        key_name: &str,
        model: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
    ) {
        self.storage
            .record(key_hash, key_name, model, prompt_tokens, completion_tokens);
    }

    /// Snapshot of current usage by querying Storage.
    pub fn snapshot(&self) -> UsageSnapshot {
        let rows = self.storage.snapshot();
        UsageSnapshot {
            keys: rows
                .into_iter()
                .map(|r| {
                    let model_usage: HashMap<String, ModelUsageSnapshot> = r
                        .models
                        .into_iter()
                        .map(|m| {
                            (
                                m.model,
                                ModelUsageSnapshot {
                                    requests: m.requests as u64,
                                    prompt_tokens: m.prompt_tokens as u64,
                                    completion_tokens: m.completion_tokens as u64,
                                },
                            )
                        })
                        .collect();
                    KeyUsageSnapshot {
                        key_name: r.key_name,
                        total_requests: r.total_requests as u64,
                        total_prompt_tokens: r.total_prompt_tokens as u64,
                        total_completion_tokens: r.total_completion_tokens as u64,
                        last_used: r.last_used,
                        model_usage,
                    }
                })
                .collect(),
            updated_at: Utc::now(),
        }
    }

    /// Time-bucketed usage for the chart.
    pub fn timeline(&self, range: &str) -> Vec<crate::storage::TimelineBucket> {
        self.storage.timeline(range)
    }
}
