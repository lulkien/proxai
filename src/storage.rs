use rusqlite::{Connection, params};
use std::sync::{Arc, Mutex};

/// Thin wrapper around SQLite for usage persistence.
#[derive(Clone)]
pub struct Storage {
    conn: Arc<Mutex<Connection>>,
}

impl Storage {
    pub fn open(path: &str) -> Result<Self, String> {
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
             CREATE INDEX IF NOT EXISTS idx_usage_model ON usage(key_hash, model);",
        )
        .map_err(|e| format!("migrate: {e}"))?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
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
        let mut stmt = conn
            .prepare(
                "SELECT key_hash, key_name,
                        COUNT(*) as total_requests,
                        COALESCE(SUM(prompt_tokens), 0) as total_prompt,
                        COALESCE(SUM(completion_tokens), 0) as total_completion,
                        MAX(created_at) as last_used
                 FROM usage
                 GROUP BY key_hash
                 ORDER BY last_used DESC",
            )
            .unwrap();

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

    /// Update the display name for all records matching a key hash.
    #[allow(dead_code)]
    pub fn set_key_name(&self, key_hash: &str, name: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "UPDATE usage SET key_name = ?1 WHERE key_hash = ?2 AND key_name = ''",
            params![name, key_hash],
        );
    }
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
