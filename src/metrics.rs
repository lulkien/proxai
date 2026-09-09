use crate::storage::Storage;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

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
    /// True when the key has been revoked/deleted but usage rows remain.
    pub deleted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelUsageSnapshot {
    pub requests: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// Serialize a u64 as a JSON string. Token totals can exceed JavaScript's
/// Number.MAX_SAFE_INTEGER (2^53) once summed across many requests, so the
/// dashboard receives them as strings and does arithmetic with BigInt.
fn token_as_string<S: serde::Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
    s.collect_str(v)
}

/// One advertised model's all-time usage across all keys. Token fields
/// serialize as JSON strings (see `token_as_string`).
#[derive(Debug, Clone, Serialize)]
pub struct ModelStatEntry {
    pub model: String,
    pub requests: u64,
    #[serde(serialize_with = "token_as_string")]
    pub prompt_tokens: u64,
    #[serde(serialize_with = "token_as_string")]
    pub completion_tokens: u64,
}

/// Dashboard Models tab payload: one row per advertised model (zero-filled
/// when unused) plus the count of discovered-but-not-advertised models.
#[derive(Debug, Clone, Serialize)]
pub struct ModelStats {
    pub active: Vec<ModelStatEntry>,
    pub deactivated_count: usize,
    pub updated_at: DateTime<Utc>,
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
    ///
    /// `active` holds the hashes of keys that still exist; usage rows for
    /// revoked keys are kept but flagged `deleted`.
    pub fn snapshot(&self, active: &HashSet<String>) -> UsageSnapshot {
        let rows = self.storage.snapshot(active);
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
                        deleted: r.deleted,
                    }
                })
                .collect(),
            updated_at: Utc::now(),
        }
    }

    /// Time-bucketed usage for the chart.
    ///
    /// `active` holds the hashes of keys that still exist; entries for
    /// revoked keys are kept but flagged `deleted`.
    pub fn timeline(
        &self,
        range: &str,
        active: &HashSet<String>,
    ) -> Vec<crate::storage::TimelineBucket> {
        self.storage.timeline(range, active)
    }

    /// Fold usage rows of revoked keys idle past the retention window into
    /// the `deleted_usage` rollup (see `Storage::consolidate_deleted`).
    /// Returns the number of keys folded.
    pub fn consolidate_deleted(&self, active: &HashSet<String>) -> Result<usize, String> {
        self.storage.consolidate_deleted(active)
    }

    /// Fold raw usage rows older than `retention_days` into per-(key, model)
    /// cumulative counters (see `Storage::consolidate_aged`). Returns the
    /// number of raw rows folded.
    pub fn consolidate_aged(
        &self,
        active: &HashSet<String>,
        retention_days: u64,
    ) -> Result<usize, String> {
        self.storage.consolidate_aged(active, retention_days)
    }

    /// Per-model usage across all keys for the dashboard Models tab.
    ///
    /// Produces exactly one row per advertised model id — zero-filled for
    /// models with no traffic — sorted by requests desc (name asc as a
    /// deterministic tie-break). Usage rows for models no longer advertised
    /// are excluded: those models are not in `advertised` and therefore not
    /// counted at all. `deactivated_count` (discovered-but-not-advertised
    /// models, from model discovery) passes through into the response.
    pub fn model_stats(
        &self,
        active: &HashSet<String>,
        advertised: &[String],
        deactivated_count: usize,
    ) -> ModelStats {
        let snap = self.snapshot(active);

        // Merge each key's per-model usage into one all-keys total.
        let mut agg: HashMap<String, (u64, u64, u64)> = HashMap::new();
        for key in &snap.keys {
            for (model, u) in &key.model_usage {
                let e = agg.entry(model.clone()).or_insert((0, 0, 0));
                e.0 += u.requests;
                e.1 += u.prompt_tokens;
                e.2 += u.completion_tokens;
            }
        }

        // One row per advertised model, zero-filled when unused.
        let mut rows: Vec<ModelStatEntry> = advertised
            .iter()
            .map(|m| {
                let (r, p, c) = agg.get(m).copied().unwrap_or((0, 0, 0));
                ModelStatEntry {
                    model: m.clone(),
                    requests: r,
                    prompt_tokens: p,
                    completion_tokens: c,
                }
            })
            .collect();
        rows.sort_by(|a, b| {
            b.requests
                .cmp(&a.requests)
                .then_with(|| a.model.cmp(&b.model))
        });

        ModelStats {
            active: rows,
            deactivated_count,
            updated_at: Utc::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Storage;

    fn test_tracker() -> UsageTracker {
        let storage =
            Arc::new(Storage::open_with_tz(":memory:", 0, "+0 hours".to_string()).unwrap());
        UsageTracker::new(storage)
    }

    #[test]
    fn model_stats_aggregates_across_keys_and_zero_fills_unused() {
        let t = test_tracker();
        t.record("hash-a", "alice", "deepseek/deepseek-chat", 10, 5);
        t.record("hash-b", "bob", "deepseek/deepseek-chat", 3, 1);
        t.record("hash-a", "alice", "deepseek/deepseek-reasoner", 100, 20);

        let active: HashSet<String> = ["hash-a".into(), "hash-b".into()].into_iter().collect();
        let stats = t.model_stats(
            &active,
            &[
                "deepseek/deepseek-chat".to_string(),
                "unused/model".to_string(),
            ],
            3,
        );

        assert_eq!(stats.deactivated_count, 3);
        assert_eq!(stats.active.len(), 2);

        let chat = stats
            .active
            .iter()
            .find(|m| m.model == "deepseek/deepseek-chat")
            .unwrap();
        assert_eq!(chat.requests, 2);
        assert_eq!(chat.prompt_tokens, 13);
        assert_eq!(chat.completion_tokens, 6);

        // Advertised-but-unused models still get a (zero-filled) row.
        let unused = stats
            .active
            .iter()
            .find(|m| m.model == "unused/model")
            .unwrap();
        assert_eq!(
            (
                unused.requests,
                unused.prompt_tokens,
                unused.completion_tokens
            ),
            (0, 0, 0)
        );
    }

    #[test]
    fn model_stats_excludes_usage_for_unadvertised_models() {
        let t = test_tracker();
        // Recorded under a model that is no longer advertised (config drift).
        t.record("hash-a", "alice", "old/gpt-4", 50, 10);

        let active: HashSet<String> = ["hash-a".into()].into_iter().collect();
        let stats = t.model_stats(&active, &["deepseek/deepseek-chat".to_string()], 0);

        assert_eq!(stats.active.len(), 1);
        assert_eq!(
            stats.active[0].requests, 0,
            "stale usage must not leak into rows"
        );
    }

    #[test]
    fn model_stats_sorts_by_requests_desc() {
        let t = test_tracker();
        t.record("hash-a", "alice", "m/low", 1, 1);
        t.record("hash-a", "alice", "m/high", 9, 1);

        let active: HashSet<String> = ["hash-a".into()].into_iter().collect();
        let stats = t.model_stats(
            &active,
            &[
                "m/high".to_string(),
                "m/low".to_string(),
                "m/zero".to_string(),
            ],
            0,
        );

        let order: Vec<&str> = stats.active.iter().map(|m| m.model.as_str()).collect();
        assert_eq!(order, vec!["m/high", "m/low", "m/zero"]);
    }

    #[test]
    fn model_stats_serializes_tokens_as_strings() {
        // Token counts may exceed JS Number.MAX_SAFE_INTEGER; JSON must carry
        // them as strings so the dashboard can use BigInt.
        let t = test_tracker();
        t.record("hash-a", "alice", "m/big", 9_007_199_254_740_993, 1);

        let active: HashSet<String> = ["hash-a".into()].into_iter().collect();
        let stats = t.model_stats(&active, &["m/big".to_string()], 0);
        let json = serde_json::to_value(stats).unwrap();

        assert_eq!(json["active"][0]["requests"], 1);
        assert_eq!(
            json["active"][0]["prompt_tokens"],
            serde_json::Value::String("9007199254740993".into())
        );
        assert_eq!(json["active"][0]["completion_tokens"], "1");
    }
}
