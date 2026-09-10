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

/// A revoked key stays visible in usage stats (flagged `deleted`) for this
/// many days after its last request, then its rows are folded into the
/// `deleted_usage` rollup (per-model totals preserved) and physically
/// deleted. Mirrors the chart's maximum range (7d). Active keys are always
/// kept regardless of age.
const STALE_DELETED_KEY_DAYS: i64 = 7;

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
             CREATE TABLE IF NOT EXISTS deleted_usage (
                 model TEXT PRIMARY KEY,
                 requests INTEGER NOT NULL DEFAULT 0,
                 prompt_tokens INTEGER NOT NULL DEFAULT 0,
                 completion_tokens INTEGER NOT NULL DEFAULT 0,
                 merged_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS usage_totals (
                 key_hash TEXT NOT NULL,
                 key_name TEXT NOT NULL DEFAULT '',
                 model TEXT NOT NULL,
                 requests INTEGER NOT NULL DEFAULT 0,
                 prompt_tokens INTEGER NOT NULL DEFAULT 0,
                 completion_tokens INTEGER NOT NULL DEFAULT 0,
                 last_used TEXT NOT NULL,
                 PRIMARY KEY (key_hash, model)
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

    /// Fold usage rows of revoked keys that have been idle longer than
    /// `STALE_DELETED_KEY_DAYS` into the `deleted_usage` rollup (per-model
    /// totals preserved) and physically delete the original rows — from
    /// both `usage` and `usage_totals` (a long-lived key may already have
    /// aged counters by the time it is revoked).
    ///
    /// `active` holds the hashes of keys that still exist. Returns the
    /// number of keys folded. Idempotent: keys already folded have no rows
    /// left, so a second run folds nothing.
    pub fn consolidate_deleted(&self, active: &HashSet<String>) -> Result<usize, String> {
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(STALE_DELETED_KEY_DAYS))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().map_err(|e| format!("begin: {e}"))?;

        // Revoked keys whose newest activity (raw row or aged counter)
        // predates the retention window.
        let mut stmt = tx
            .prepare(
                "SELECT key_hash, MAX(ts) FROM (
                    SELECT key_hash, created_at AS ts FROM usage
                    UNION ALL
                    SELECT key_hash, last_used AS ts FROM usage_totals
                 ) GROUP BY key_hash",
            )
            .map_err(|e| format!("prepare: {e}"))?;
        let stale: Vec<String> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| format!("query: {e}"))?
            .filter_map(|r| r.ok())
            .filter(|(hash, last)| !active.contains(hash) && last.as_str() < cutoff.as_str())
            .map(|(hash, _)| hash)
            .collect();
        drop(stmt);

        if stale.is_empty() {
            return Ok(0);
        }

        let mut fold_stmt = tx
            .prepare(
                "INSERT INTO deleted_usage (model, requests, prompt_tokens, completion_tokens, merged_at)
                 VALUES (?1, ?2, ?3, ?4, datetime('now'))
                 ON CONFLICT(model) DO UPDATE SET
                     requests = requests + excluded.requests,
                     prompt_tokens = prompt_tokens + excluded.prompt_tokens,
                     completion_tokens = completion_tokens + excluded.completion_tokens,
                     merged_at = excluded.merged_at",
            )
            .map_err(|e| format!("prepare fold: {e}"))?;
        let mut del_stmt = tx
            .prepare("DELETE FROM usage WHERE key_hash = ?1")
            .map_err(|e| format!("prepare delete: {e}"))?;
        let mut del_tot_stmt = tx
            .prepare("DELETE FROM usage_totals WHERE key_hash = ?1")
            .map_err(|e| format!("prepare delete totals: {e}"))?;
        let mut model_stmt = tx
            .prepare(
                "SELECT model, COUNT(*), COALESCE(SUM(prompt_tokens), 0), COALESCE(SUM(completion_tokens), 0)
                 FROM usage WHERE key_hash = ?1 GROUP BY model",
            )
            .map_err(|e| format!("prepare model: {e}"))?;
        let mut tot_stmt = tx
            .prepare(
                "SELECT model, requests, prompt_tokens, completion_tokens
                 FROM usage_totals WHERE key_hash = ?1",
            )
            .map_err(|e| format!("prepare totals: {e}"))?;

        let mut folded = 0usize;
        for hash in &stale {
            let mut models: Vec<(String, i64, i64, i64)> = model_stmt
                .query_map(params![hash], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })
                .map_err(|e| format!("query models: {e}"))?
                .filter_map(|r| r.ok())
                .collect();
            let tot_models: Vec<(String, i64, i64, i64)> = tot_stmt
                .query_map(params![hash], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })
                .map_err(|e| format!("query totals: {e}"))?
                .filter_map(|r| r.ok())
                .collect();
            models.extend(tot_models);
            for (model, req, pt, ct) in models {
                fold_stmt
                    .execute(params![model, req, pt, ct])
                    .map_err(|e| format!("fold {model}: {e}"))?;
            }
            del_stmt
                .execute(params![hash])
                .map_err(|e| format!("delete {hash}: {e}"))?;
            del_tot_stmt
                .execute(params![hash])
                .map_err(|e| format!("delete totals {hash}: {e}"))?;
            folded += 1;
        }
        drop(tot_stmt);
        drop(model_stmt);
        drop(del_tot_stmt);
        drop(del_stmt);
        drop(fold_stmt);

        tx.commit().map_err(|e| format!("commit: {e}"))?;
        Ok(folded)
    }

    /// Fold raw `usage` rows older than `retention_days` into the
    /// per-(key, model) cumulative `usage_totals` counters, then delete the
    /// raw rows. Applies only to currently-active keys; revoked keys are
    /// handled wholesale by `consolidate_deleted`.
    ///
    /// This bounds the raw table to roughly `retention_days` of traffic no
    /// matter how long a key lives. Returns the number of raw rows folded.
    /// Idempotent: already-folded rows are gone.
    pub fn consolidate_aged(
        &self,
        active: &HashSet<String>,
        retention_days: u64,
    ) -> Result<usize, String> {
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(retention_days as i64))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().map_err(|e| format!("begin: {e}"))?;

        // Active keys that still have rows older than the cutoff.
        let mut stmt = tx
            .prepare("SELECT DISTINCT key_hash FROM usage WHERE created_at < ?1")
            .map_err(|e| format!("prepare: {e}"))?;
        let candidates: Vec<String> = stmt
            .query_map(params![&cutoff], |r| r.get::<_, String>(0))
            .map_err(|e| format!("query: {e}"))?
            .filter_map(|r| r.ok())
            .filter(|h| active.contains(h))
            .collect();
        drop(stmt);

        if candidates.is_empty() {
            return Ok(0);
        }

        // Per (key, model): roll the old rows into counters.
        let mut agg_stmt = tx
            .prepare(
                "SELECT key_name, model, COUNT(*), COALESCE(SUM(prompt_tokens), 0), COALESCE(SUM(completion_tokens), 0), MAX(created_at)
                 FROM usage WHERE key_hash = ?1 AND created_at < ?2 GROUP BY key_name, model",
            )
            .map_err(|e| format!("prepare agg: {e}"))?;
        let mut upsert_stmt = tx
            .prepare(
                "INSERT INTO usage_totals (key_hash, key_name, model, requests, prompt_tokens, completion_tokens, last_used)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(key_hash, model) DO UPDATE SET
                     key_name = excluded.key_name,
                     requests = requests + excluded.requests,
                     prompt_tokens = prompt_tokens + excluded.prompt_tokens,
                     completion_tokens = completion_tokens + excluded.completion_tokens,
                     last_used = MAX(usage_totals.last_used, excluded.last_used)",
            )
            .map_err(|e| format!("prepare upsert: {e}"))?;
        let mut del_stmt = tx
            .prepare("DELETE FROM usage WHERE key_hash = ?1 AND created_at < ?2")
            .map_err(|e| format!("prepare delete: {e}"))?;

        let mut folded = 0usize;
        for hash in &candidates {
            let groups: Vec<(String, String, i64, i64, i64, String)> = agg_stmt
                .query_map(params![hash, &cutoff], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                })
                .map_err(|e| format!("query agg: {e}"))?
                .filter_map(|r| r.ok())
                .collect();
            let mut rows_here = 0usize;
            for (name, model, req, pt, ct, last) in &groups {
                upsert_stmt
                    .execute(params![hash, name, model, req, pt, ct, last])
                    .map_err(|e| format!("upsert {model}: {e}"))?;
                rows_here += *req as usize;
            }
            if !groups.is_empty() {
                let deleted = del_stmt
                    .execute(params![hash, &cutoff])
                    .map_err(|e| format!("delete {hash}: {e}"))?;
                debug_assert_eq!(deleted, rows_here);
                folded += rows_here;
            }
        }
        drop(del_stmt);
        drop(upsert_stmt);
        drop(agg_stmt);

        tx.commit().map_err(|e| format!("commit: {e}"))?;
        Ok(folded)
    }

    /// Return aggregated usage per key, with per-model breakdown.
    ///
    /// `active` holds the hashes of keys that still exist; rows for revoked
    /// keys are kept but flagged `deleted` until `consolidate_deleted`
    /// physically folds them into the `deleted_usage` rollup, after which
    /// the key disappears from the table entirely (its totals live on only
    /// in per-model stats via `deleted_usage_rows`). Active keys are always
    /// kept (all-time totals).
    pub fn snapshot(&self, active: &HashSet<String>) -> Vec<KeyUsageRow> {
        let conn = self.conn.lock().unwrap();

        // Per-key aggregates: raw rows (recent, per-request) + aged
        // cumulative counters (usage_totals) both count toward all-time
        // totals. Both timestamps are SQLite UTC 'YYYY-MM-DD HH:MM:SS' so
        // MAX() compares correctly across the union.
        let tz = &self.tz_sql;
        let sql = format!(
            "SELECT key_hash,
                    MAX(key_name) as key_name,
                    SUM(requests) as total_requests,
                    SUM(prompt_tokens) as total_prompt,
                    SUM(completion_tokens) as total_completion,
                    datetime(MAX(last), '{tz}') as last_used
             FROM (
                 SELECT key_hash, key_name, 1 as requests, prompt_tokens, completion_tokens,
                        created_at as last
                 FROM usage
                 UNION ALL
                 SELECT key_hash, key_name, requests, prompt_tokens, completion_tokens,
                        last_used as last
                 FROM usage_totals
             )
             GROUP BY key_hash
             ORDER BY MAX(last) DESC"
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

        // No in-memory stale filtering here: a revoked key idle past the
        // retention window stays listed (flagged deleted) until
        // `consolidate_deleted` physically folds it into the rollup, so
        // totals never undercount between consolidation runs.

        // Per-model breakdown for each key: same union of raw + counters.
        let mut model_stmt = conn
            .prepare(
                "SELECT model,
                        SUM(requests) as requests,
                        SUM(prompt_tokens) as prompt_tokens,
                        SUM(completion_tokens) as completion_tokens
                 FROM (
                     SELECT model, 1 as requests, prompt_tokens, completion_tokens
                     FROM usage
                     WHERE key_hash = ?1
                     UNION ALL
                     SELECT model, requests, prompt_tokens, completion_tokens
                     FROM usage_totals
                     WHERE key_hash = ?1
                 )
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

    /// Per-model totals of keys folded by `consolidate_deleted`. Feeds
    /// per-model stats only — the keys themselves are gone from
    /// `snapshot()` (see its doc).
    pub fn deleted_usage_rows(&self) -> Vec<ModelUsageRow> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT model, requests, prompt_tokens, completion_tokens
                 FROM deleted_usage
                 ORDER BY requests DESC",
            )
            .unwrap();
        stmt.query_map([], |r| {
            Ok(ModelUsageRow {
                model: r.get(0)?,
                requests: r.get(1)?,
                prompt_tokens: r.get(2)?,
                completion_tokens: r.get(3)?,
            })
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    }

    /// All-time token totals across every table: raw rows, aged counters,
    /// and the deleted-keys rollup. Independent of which models are
    /// currently advertised, so rotated-out models keep counting.
    pub fn total_token_usage(&self) -> (u64, u64) {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COALESCE(SUM(prompt_tokens), 0), COALESCE(SUM(completion_tokens), 0)
             FROM (
                 SELECT prompt_tokens, completion_tokens FROM usage
                 UNION ALL
                 SELECT prompt_tokens, completion_tokens FROM usage_totals
                 UNION ALL
                 SELECT prompt_tokens, completion_tokens FROM deleted_usage
             )",
            [],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
        )
        .unwrap_or((0, 0))
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
                format!(
                    "strftime('%Y-%m-%dT', created_at, '{tz_offset}') || printf('%02d', (CAST(strftime('%H', created_at, '{tz_offset}') AS INTEGER) / 2) * 2)"
                ),
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
                let base = now
                    .date_naive()
                    .and_hms_opt(cur_block, 0, 0)
                    .unwrap()
                    .and_local_timezone(tz)
                    .unwrap();
                (0..12)
                    .rev()
                    .map(|i| {
                        (base - chrono::Duration::hours(i as i64 * 2))
                            .format("%Y-%m-%dT%H")
                            .to_string()
                    })
                    .collect()
            }
            _ => (0..7i64)
                .rev()
                .map(|d| {
                    (now - chrono::Duration::days(d))
                        .format("%Y-%m-%d")
                        .to_string()
                })
                .collect(),
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
    fn snapshot_keeps_stale_deleted_keys_until_consolidation() {
        let s = test_storage();
        // A revoked key whose last request predates the retention window
        // (7 days) stays listed (flagged deleted) — consolidation, not
        // snapshot, is what reclaims it.
        s.conn.lock().unwrap()
            .execute(
                "INSERT INTO usage (key_hash, key_name, model, prompt_tokens, completion_tokens, created_at)
                 VALUES ('hash-old', 'old', 'gpt', 1, 1, '2020-01-01 00:00:00')",
                [],
            )
            .unwrap();
        // A recently-active revoked key stays, still flagged deleted.
        s.record("hash-recent", "recent", "gpt", 3, 1);

        let rows = s.snapshot(&HashSet::new());

        let names: Vec<&str> = rows.iter().map(|r| r.key_name.as_str()).collect();
        assert_eq!(
            names,
            vec!["recent", "old"],
            "stale deleted keys must stay visible until consolidated"
        );
        assert!(rows.iter().all(|r| r.deleted));
    }

    #[test]
    fn snapshot_keeps_stale_active_keys() {
        // Retention only applies to revoked keys: an existing key with old
        // usage must still show all-time totals.
        let s = test_storage();
        s.conn.lock().unwrap()
            .execute(
                "INSERT INTO usage (key_hash, key_name, model, prompt_tokens, completion_tokens, created_at)
                 VALUES ('hash-alice', 'alice', 'gpt', 10, 5, '2020-01-01 00:00:00')",
                [],
            )
            .unwrap();

        let active: HashSet<String> = ["hash-alice".into()].into_iter().collect();
        let rows = s.snapshot(&active);

        assert_eq!(rows.len(), 1);
        assert!(!rows[0].deleted);
        assert_eq!(rows[0].total_requests, 1);
    }

    #[test]
    fn consolidate_folds_stale_revoked_key_and_preserves_totals() {
        let s = test_storage();
        // Active key keeps all-time usage.
        s.record("hash-active", "alice", "gpt", 10, 5);
        // Revoked key with stale rows across two models.
        {
            let conn = s.conn.lock().unwrap();
            for (model, pt, ct, ts) in [
                ("gpt", 100, 20, "2020-01-01 00:00:00"),
                ("claude", 50, 10, "2020-01-02 00:00:00"),
                ("gpt", 7, 3, "2020-01-03 00:00:00"),
            ] {
                conn.execute(
                    "INSERT INTO usage (key_hash, key_name, model, prompt_tokens, completion_tokens, created_at)
                     VALUES ('hash-old', 'old-key', ?1, ?2, ?3, ?4)",
                    params![model, pt, ct, ts],
                )
                .unwrap();
            }
        }

        let active: HashSet<String> = ["hash-active".into()].into_iter().collect();
        let folded = s.consolidate_deleted(&active).unwrap();
        assert_eq!(folded, 1, "exactly the one stale revoked key folds");

        // Original rows physically deleted; active key untouched.
        let conn = s.conn.lock().unwrap();
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1, "only the active key's row remains");

        // Folded key disappears from the snapshot entirely...
        drop(conn);
        let rows = s.snapshot(&active);
        let names: Vec<&str> = rows.iter().map(|r| r.key_name.as_str()).collect();
        assert_eq!(
            names,
            vec!["alice"],
            "folded key must not appear in snapshot"
        );

        // ...while its per-model totals live on in the rollup.
        let rollup = s.deleted_usage_rows();
        assert_eq!(rollup.len(), 2);
        let gpt = rollup.iter().find(|m| m.model == "gpt").unwrap();
        assert_eq!(
            (gpt.requests, gpt.prompt_tokens, gpt.completion_tokens),
            (2, 107, 23)
        );
        let claude = rollup.iter().find(|m| m.model == "claude").unwrap();
        assert_eq!(
            (
                claude.requests,
                claude.prompt_tokens,
                claude.completion_tokens
            ),
            (1, 50, 10)
        );
    }

    #[test]
    fn consolidate_skips_active_and_recently_revoked_keys() {
        let s = test_storage();
        s.record("hash-active", "alice", "gpt", 10, 5); // active, fresh
        s.record("hash-fresh-revoked", "bob", "gpt", 3, 1); // revoked but recent
        s.record("hash-old", "old", "gpt", 1, 1); // old, but active

        // Make hash-old stale while keeping it in the active set.
        s.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE usage SET created_at = '2020-01-01 00:00:00' WHERE key_hash = 'hash-old'",
                [],
            )
            .unwrap();

        let active: HashSet<String> = ["hash-active".into(), "hash-old".into()]
            .into_iter()
            .collect();
        let folded = s.consolidate_deleted(&active).unwrap();
        assert_eq!(folded, 0, "active keys never fold, even when stale");

        // bob is revoked but recent: stays until idle past the window.
        let active2: HashSet<String> = ["hash-active".into(), "hash-old".into()]
            .into_iter()
            .collect();
        let folded2 = s.consolidate_deleted(&active2).unwrap();
        assert_eq!(folded2, 0);

        let conn = s.conn.lock().unwrap();
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 3);
    }

    #[test]
    fn consolidate_aged_folds_only_rows_older_than_retention() {
        let s = test_storage();
        // Active key with a mix of fresh and old rows.
        let conn = s.conn.lock().unwrap();
        for (model, pt, ct, ts) in [
            ("gpt", 100, 20, "2020-01-01 00:00:00"),
            ("gpt", 50, 10, "2020-01-02 00:00:00"),
            ("claude", 7, 3, "2020-01-03 00:00:00"),
        ] {
            conn.execute(
                "INSERT INTO usage (key_hash, key_name, model, prompt_tokens, completion_tokens, created_at)
                 VALUES ('hash-alice', 'alice', ?1, ?2, ?3, ?4)",
                params![model, pt, ct, ts],
            )
            .unwrap();
        }
        // A fresh row that must survive (within retention).
        drop(conn);
        s.record("hash-alice", "alice", "gpt", 1, 1);

        let active: HashSet<String> = ["hash-alice".into()].into_iter().collect();
        let folded = s.consolidate_aged(&active, 14).unwrap();
        assert_eq!(folded, 3, "three old rows fold, fresh row stays");

        // Only the fresh row remains raw.
        let conn = s.conn.lock().unwrap();
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
        drop(conn);

        // Counters hold the folded per-model totals.
        let stats = s.snapshot(&active);
        assert_eq!(stats.len(), 1);
        let alice = &stats[0];
        assert!(!alice.deleted);
        assert_eq!(alice.total_requests, 4, "folded 3 + fresh 1 all count");
        assert_eq!(alice.total_prompt_tokens, 158);
        assert_eq!(alice.total_completion_tokens, 34);
        let gpt = alice.models.iter().find(|m| m.model == "gpt").unwrap();
        assert_eq!(
            (gpt.requests, gpt.prompt_tokens, gpt.completion_tokens),
            (3, 151, 31)
        );
        let claude = alice.models.iter().find(|m| m.model == "claude").unwrap();
        assert_eq!(
            (
                claude.requests,
                claude.prompt_tokens,
                claude.completion_tokens
            ),
            (1, 7, 3)
        );
    }

    #[test]
    fn consolidate_aged_is_idempotent_and_skips_revoked() {
        let s = test_storage();
        // Old row for an ACTIVE key and an old row for a REVOKED key.
        for (hash, name, model) in [
            ("hash-active", "alice", "gpt"),
            ("hash-revoked", "bob", "claude"),
        ] {
            s.conn.lock().unwrap()
                .execute(
                    "INSERT INTO usage (key_hash, key_name, model, prompt_tokens, completion_tokens, created_at)
                     VALUES (?1, ?2, ?3, 5, 2, '2020-01-01 00:00:00')",
                    params![hash, name, model],
                )
                .unwrap();
        }

        let active: HashSet<String> = ["hash-active".into()].into_iter().collect();
        let folded = s.consolidate_aged(&active, 14).unwrap();
        assert_eq!(folded, 1, "only the active key's row folds");

        // Second run folds nothing.
        let folded_again = s.consolidate_aged(&active, 14).unwrap();
        assert_eq!(folded_again, 0);

        let conn = s.conn.lock().unwrap();
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            remaining, 1,
            "revoked key's raw row is left for deleted-fold"
        );
        drop(conn);

        // Revoked key with aged counters still consolidates fully later.
        let folded_deleted = s.consolidate_deleted(&active).unwrap();
        assert_eq!(folded_deleted, 1);
    }

    #[test]
    fn consolidate_deleted_folds_aged_counters_too() {
        let s = test_storage();
        // Simulate a long-lived key: old raw rows plus aged counters, then
        // revoked and idle long enough for deleted-fold to claim both.
        s.conn.lock().unwrap()
            .execute(
                "INSERT INTO usage (key_hash, key_name, model, prompt_tokens, completion_tokens, created_at)
                 VALUES ('hash-gone', 'work', 'gpt', 10, 5, '2020-01-02 00:00:00')",
                [],
            )
            .unwrap();
        s.conn.lock().unwrap()
            .execute(
                "INSERT INTO usage_totals (key_hash, key_name, model, requests, prompt_tokens, completion_tokens, last_used)
                 VALUES ('hash-gone', 'work', 'gpt', 500, 9000, 300, '2020-01-01 00:00:00')",
                [],
            )
            .unwrap();

        // Revoked and idle: deleted-fold must absorb BOTH raw and counters.
        let folded = s.consolidate_deleted(&HashSet::new()).unwrap();
        assert_eq!(folded, 1);

        let conn = s.conn.lock().unwrap();
        let raw: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage", [], |r| r.get(0))
            .unwrap();
        assert_eq!(raw, 0);
        let totals: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_totals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(totals, 0, "aged counters must not outlive the key");
        drop(conn);

        // Key gone from snapshot; rollup keeps raw + counter totals.
        assert!(s.snapshot(&HashSet::new()).is_empty());
        let rollup = s.deleted_usage_rows();
        assert_eq!(rollup.len(), 1);
        assert_eq!(
            (
                rollup[0].requests,
                rollup[0].prompt_tokens,
                rollup[0].completion_tokens
            ),
            (501, 9010, 305)
        );
    }

    #[test]
    fn consolidate_is_idempotent_and_merges_keys() {
        let s = test_storage();
        for (hash, name, ts) in [
            ("hash-a", "a", "2020-01-01 00:00:00"),
            ("hash-b", "b", "2020-01-02 00:00:00"),
        ] {
            s.conn.lock().unwrap()
                .execute(
                    "INSERT INTO usage (key_hash, key_name, model, prompt_tokens, completion_tokens, created_at)
                     VALUES (?1, ?2, 'gpt', 5, 2, ?3)",
                    params![hash, name, ts],
                )
                .unwrap();
        }

        let folded = s.consolidate_deleted(&HashSet::new()).unwrap();
        assert_eq!(folded, 2);
        // Second run folds nothing; rollup does not double-count.
        let folded_again = s.consolidate_deleted(&HashSet::new()).unwrap();
        assert_eq!(folded_again, 0);

        // Keys vanish from snapshot; rollup merges both into one model row.
        assert!(s.snapshot(&HashSet::new()).is_empty());
        let rollup = s.deleted_usage_rows();
        assert_eq!(rollup.len(), 1);
        assert_eq!(
            (
                rollup[0].requests,
                rollup[0].prompt_tokens,
                rollup[0].completion_tokens
            ),
            (2, 10, 4)
        );
    }

    #[test]
    fn total_token_usage_spans_all_tables_and_models() {
        let s = test_storage();
        // Raw row (recent, advertised or not — total is model-agnostic).
        s.record("hash-a", "alice", "rotated/model", 100, 20);
        // Aged counter for another model.
        s.conn.lock().unwrap()
            .execute(
                "INSERT INTO usage_totals (key_hash, key_name, model, requests, prompt_tokens, completion_tokens, last_used)
                 VALUES ('hash-b', 'bob', 'aged/model', 5, 900, 90, datetime('now'))",
                [],
            )
            .unwrap();
        // Deleted-keys rollup for a third model.
        s.conn.lock().unwrap()
            .execute(
                "INSERT INTO deleted_usage (model, requests, prompt_tokens, completion_tokens, merged_at)
                 VALUES ('gone/model', 3, 7, 4, datetime('now'))",
                [],
            )
            .unwrap();

        let (p, c) = s.total_token_usage();
        assert_eq!((p, c), (1007, 114), "raw + counters + rollup all count");
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
