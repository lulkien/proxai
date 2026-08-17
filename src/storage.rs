use rusqlite::{Connection, params};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// Thin wrapper around SQLite for usage persistence.
#[derive(Clone)]
pub struct Storage {
    conn: Arc<Mutex<Connection>>,
    tz_offset_secs: i32,
    tz_sql: String,
}

impl Storage {
    pub fn open_with_tz(path: &str, tz_offset_secs: i32, tz_sql: String) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| format!("open db: {e}"))?;

        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS usage (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 key_hash TEXT NOT NULL,
                 key_name TEXT NOT NULL DEFAULT '',
                 model TEXT NOT NULL,
                 prompt_tokens INTEGER NOT NULL DEFAULT 0,
                 completion_tokens INTEGER NOT NULL DEFAULT 0,
                 created_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_usage_key ON usage(key_hash);
             CREATE INDEX IF NOT EXISTS idx_usage_model ON usage(key_hash, model);
             CREATE INDEX IF NOT EXISTS idx_usage_created ON usage(created_at);",
        )
        .map_err(|e| format!("migrate: {e}"))?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            tz_offset_secs,
            tz_sql,
        })
    }

    /// Record a single usage event.
    pub fn record(
        &self,
        key_hash: &str,
        key_name: &str,
        model: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
    ) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO usage (key_hash, key_name, model, prompt_tokens, completion_tokens, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
            params![key_hash, key_name, model, prompt_tokens, completion_tokens],
        );
    }

    /// Return aggregated usage per key, with per-model breakdown.
    ///
    /// `active` holds the hashes of keys that still exist; rows for revoked
    /// keys are kept but flagged `deleted`.
    pub fn snapshot(&self, active: &HashSet<String>) -> Vec<KeyUsageRow> {
        let conn = self.conn.lock().unwrap();

        // Per-key aggregates
        let tz = &self.tz_sql;
        let sql = format!(
            "SELECT key_hash, key_name,
                    COUNT(*) as total_requests,
                    COALESCE(SUM(prompt_tokens), 0) as total_prompt,
                    COALESCE(SUM(completion_tokens), 0) as total_completion,
                    datetime(MAX(created_at), '{tz}') as last_used
             FROM usage
             GROUP BY key_hash
             ORDER BY MAX(created_at) DESC"
        );
        let mut stmt = conn.prepare(&sql).unwrap();

        let mut rows: Vec<KeyUsageRow> = stmt
            .query_map([], |row| {
                Ok(KeyUsageRow {
                    key_hash: row.get(0)?,
                    key_name: row.get(1)?,
                    total_requests: row.get(2)?,
                    total_prompt_tokens: row.get(3)?,
                    total_completion_tokens: row.get(4)?,
                    last_used: row.get(5)?,
                    models: Vec::new(),
                    deleted: false,
                })
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        for row in &mut rows {
            row.deleted = !active.contains(&row.key_hash);
        }

        // Per-model breakdown for each key
        let mut model_stmt = conn
            .prepare(
                "SELECT model,
                        COUNT(*) as requests,
                        COALESCE(SUM(prompt_tokens), 0) as prompt_tokens,
                        COALESCE(SUM(completion_tokens), 0) as completion_tokens
                 FROM usage
                 WHERE key_hash = ?1
                 GROUP BY model
                 ORDER BY requests DESC",
            )
            .unwrap();

        for row in &mut rows {
            if let Ok(model_rows) = model_stmt.query_map(params![row.key_hash], |r| {
                Ok(ModelUsageRow {
                    model: r.get(0)?,
                    requests: r.get(1)?,
                    prompt_tokens: r.get(2)?,
                    completion_tokens: r.get(3)?,
                })
            }) {
                row.models = model_rows.filter_map(|r| r.ok()).collect();
            }
        }

        // Update key_name for previously-nameless keys
        for row in &mut rows {
            if row.key_name.is_empty() {
                row.key_name = format!("key-{}", &row.key_hash[..row.key_hash.len().min(8)]);
            }
        }

        rows
    }

    /// Return time-bucketed usage for the chart.
    ///
    /// `range` is one of `1d`, `7d`. 1d groups by 2-hour; 7d by day.
    /// `active` holds the hashes of keys that still exist; entries for
    /// revoked keys are kept but flagged `deleted`.
    pub fn timeline(&self, range: &str, active: &HashSet<String>) -> Vec<TimelineBucket> {
        use chrono::Timelike;
        let conn = self.conn.lock().unwrap();

        // All times in configured timezone.
        let tz_offset = &self.tz_sql;
        let (group_expr, since) = match range {
            "1d" => (
                format!("strftime('%Y-%m-%dT', created_at, '{tz_offset}') || printf('%02d', (CAST(strftime('%H', created_at, '{tz_offset}') AS INTEGER) / 2) * 2)"),
                "1 days",
            ),
            _ => (
                format!("strftime('%Y-%m-%d', created_at, '{tz_offset}')"),
                "7 days",
            ),
        };

        let sql = format!(
            "SELECT {group_expr} AS bucket,
                    key_hash,
                    key_name,
                    COUNT(*) AS requests
             FROM usage
             WHERE created_at >= datetime('now', '{tz_offset}', '-{since}')
             GROUP BY bucket, key_hash, key_name
             ORDER BY bucket ASC, requests DESC"
        );

        let mut stmt = conn.prepare(&sql).unwrap();
        let rows: Vec<(String, String, String, i64)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        // Group rows into buckets
        let mut buckets: Vec<TimelineBucket> = Vec::new();
        for (bucket, key_hash, key_name, requests) in rows {
            let entry = TimelineEntry {
                key_name,
                requests: requests as u64,
                deleted: !active.contains(&key_hash),
            };
            match buckets.last_mut() {
                Some(b) if b.time == bucket => b.keys.push(entry),
                _ => buckets.push(TimelineBucket {
                    time: bucket,
                    keys: vec![entry],
                }),
            }
        }

        // Pad empty buckets so the chart always shows the full range.
        let tz = chrono::FixedOffset::east_opt(self.tz_offset_secs).unwrap();
        let now = chrono::Utc::now().with_timezone(&tz);
        let all_times: Vec<String> = match range {
            "1d" => {
                // 12 two-hour buckets ending at the current time block.
                let cur_block = (now.hour() / 2) * 2;
                let base = now.date_naive().and_hms_opt(cur_block, 0, 0).unwrap()
                    .and_local_timezone(tz).unwrap();
                (0..12)
                    .rev()
                    .map(|i| (base - chrono::Duration::hours(i as i64 * 2))
                        .format("%Y-%m-%dT%H").to_string())
                    .collect()
            }
            _ => {
                (0..7i64)
                    .rev()
                    .map(|d| (now - chrono::Duration::days(d)).format("%Y-%m-%d").to_string())
                    .collect()
            }
        };

        let mut padded: Vec<TimelineBucket> = Vec::new();
        for t in all_times {
            match buckets.iter().find(|b| b.time == t) {
                Some(existing) => padded.push(existing.clone()),
                None => padded.push(TimelineBucket {
                    time: t,
                    keys: Vec::new(),
                }),
            }
        }

        padded
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TimelineBucket {
    pub time: String,
    pub keys: Vec<TimelineEntry>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TimelineEntry {
    pub key_name: String,
    pub requests: u64,
    /// True when the key has been revoked/deleted but usage rows remain.
    pub deleted: bool,
}

#[derive(Debug, Clone)]
pub struct KeyUsageRow {
    pub key_hash: String,
    pub key_name: String,
    pub total_requests: i64,
    pub total_prompt_tokens: i64,
    pub total_completion_tokens: i64,
    pub last_used: Option<String>,
    pub models: Vec<ModelUsageRow>,
    /// True when the key has been revoked/deleted but usage rows remain.
    pub deleted: bool,
}

#[derive(Debug, Clone)]
pub struct ModelUsageRow {
    pub model: String,
    pub requests: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_storage() -> Storage {
        Storage::open_with_tz(":memory:", 0, "+0 hours".to_string()).unwrap()
    }

    #[test]
    fn snapshot_flags_revoked_keys_as_deleted() {
        let s = test_storage();
        s.record("hash-alice", "alice", "gpt", 10, 5);
        s.record("hash-bob", "bob", "gpt", 3, 1);

        // Only alice still exists; bob was revoked.
        let active: HashSet<String> = ["hash-alice".into()].into_iter().collect();
        let rows = s.snapshot(&active);

        assert_eq!(rows.len(), 2);
        let by_name = |n: &str| rows.iter().find(|r| r.key_name == n).unwrap();
        assert!(
            !by_name("alice").deleted,
            "existing key must not be deleted"
        );
        assert!(
            by_name("bob").deleted,
            "revoked key must be flagged deleted"
        );
    }

    #[test]
    fn snapshot_flags_all_deleted_when_none_active() {
        let s = test_storage();
        s.record("hash-alice", "alice", "gpt", 10, 5);

        let active: HashSet<String> = HashSet::new();
        let rows = s.snapshot(&active);

        assert_eq!(rows.len(), 1);
        assert!(rows[0].deleted);
    }

    #[test]
    fn timeline_keeps_and_flags_revoked_keys() {
        let s = test_storage();
        s.record("hash-alice", "alice", "gpt", 10, 5);
        s.record("hash-bob", "bob", "gpt", 3, 1);

        let active: HashSet<String> = ["hash-alice".into()].into_iter().collect();
        let buckets = s.timeline("1d", &active);

        // Both keys must still appear in the chart, bob flagged deleted.
        let mut saw_alice = false;
        let mut saw_bob = false;
        for b in &buckets {
            for k in &b.keys {
                match k.key_name.as_str() {
                    "alice" => {
                        saw_alice = true;
                        assert!(!k.deleted);
                    }
                    "bob" => {
                        saw_bob = true;
                        assert!(k.deleted);
                    }
                    _ => {}
                }
            }
        }
        assert!(saw_alice, "active key missing from timeline");
        assert!(saw_bob, "revoked key must stay visible in timeline");
    }
}
