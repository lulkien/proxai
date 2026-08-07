use rusqlite::{Connection, params};
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
    pub fn snapshot(&self) -> Vec<KeyUsageRow> {
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
                })
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

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
    pub fn timeline(&self, range: &str) -> Vec<TimelineBucket> {
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
                    key_name,
                    COUNT(*) AS requests
             FROM usage
             WHERE created_at >= datetime('now', '{tz_offset}', '-{since}')
             GROUP BY bucket, key_name
             ORDER BY bucket ASC, requests DESC"
        );

        let mut stmt = conn.prepare(&sql).unwrap();
        let rows: Vec<(String, String, i64)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        // Group rows into buckets
        let mut buckets: Vec<TimelineBucket> = Vec::new();
        for (bucket, key_name, requests) in rows {
            match buckets.last_mut() {
                Some(b) if b.time == bucket => b.keys.push(TimelineEntry {
                    key_name,
                    requests: requests as u64,
                }),
                _ => buckets.push(TimelineBucket {
                    time: bucket,
                    keys: vec![TimelineEntry {
                        key_name,
                        requests: requests as u64,
                    }],
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
}

#[derive(Debug, Clone)]
pub struct ModelUsageRow {
    pub model: String,
    pub requests: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
}
