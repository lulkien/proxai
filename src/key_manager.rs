use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Mutex};

const KEY_ID_LEN: usize = 4;
const KEY_STEM_LEN: usize = 32;
const KEY_PREFIX: &str = "sk-";

/// Compute SHA-256 hex digest of a raw API key.
pub fn hash_key(raw_key: &str) -> String {
    hex::encode(Sha256::digest(raw_key.as_bytes()))
}

/// Information shown by list-keys. Does not contain the full key.
#[derive(Debug, Serialize, Deserialize)]
pub struct KeyInfo {
    pub id: u64,
    pub name: String,
    pub partial: String,
    pub created_at: String,
}

pub struct KeyManager {
    conn: Mutex<rusqlite::Connection>,
    tz_offset_secs: i32,
}

impl KeyManager {
    /// Open (or create) keys.db at the given path. Migrates from keys.json
    /// automatically if the db is empty and keys.json exists.
    pub fn open(path: &str) -> Result<Self, String> {
        Self::open_with_tz(path, 0)
    }

    /// Open with a fixed timezone offset (in seconds) applied to `created_at`
    /// timestamps. The server passes the configured `timezone` offset so key
    /// timestamps and usage timestamps share one source of truth; offline
    /// callers default to UTC.
    pub fn open_with_tz(path: &str, tz_offset_secs: i32) -> Result<Self, String> {
        let conn = rusqlite::Connection::open(path).map_err(|e| format!("open {path}: {e}"))?;

        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS keys (
                 id INTEGER PRIMARY KEY,
                 name TEXT NOT NULL,
                 hash TEXT NOT NULL UNIQUE,
                 prefix TEXT NOT NULL,
                 suffix TEXT NOT NULL,
                 created_at TEXT NOT NULL
             );",
        )
        .map_err(|e| format!("migrate: {e}"))?;

        let km = Self {
            conn: Mutex::new(conn),
            tz_offset_secs,
        };

        // Auto-migrate from keys.json if db is empty
        if let Ok(true) = km.is_empty()
            && let Err(e) = km.try_migrate_json()
        {
            tracing::warn!("keys.json migration skipped: {e}");
        }

        Ok(km)
    }

    fn is_empty(&self) -> Result<bool, String> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM keys", [], |r| r.get(0))
            .map_err(|e| format!("count: {e}"))?;
        Ok(count == 0)
    }

    /// Import keys from keys.json if it exists and db is empty.
    fn try_migrate_json(&self) -> Result<(), String> {
        let json_path = "keys.json";
        if !Path::new(json_path).exists() {
            return Ok(());
        }
        let data =
            std::fs::read_to_string(json_path).map_err(|e| format!("read keys.json: {e}"))?;
        let store: serde_json::Value =
            serde_json::from_str(&data).map_err(|e| format!("parse keys.json: {e}"))?;

        let keys = store
            .get("keys")
            .and_then(|v| v.as_array())
            .ok_or("keys.json: missing 'keys' array")?;

        let mut imported = 0;
        for entry in keys {
            let id = entry.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let name = entry
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("migrated");
            let hash = entry.get("hash").and_then(|v| v.as_str()).unwrap_or("");
            let prefix = entry.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
            let suffix = entry.get("suffix").and_then(|v| v.as_str()).unwrap_or("");
            let created_at = entry
                .get("created_at")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            if hash.is_empty() {
                continue;
            }

            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT OR IGNORE INTO keys (id, name, hash, prefix, suffix, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![id, name, hash, prefix, suffix, created_at],
            )
            .map_err(|e| format!("insert key {id}: {e}"))?;
            imported += 1;
        }

        if imported > 0 {
            tracing::info!("Migrated {imported} keys from keys.json to SQLite");
        }
        Ok(())
    }

    /// Generate a new API key, returns the plaintext ONCE.
    pub fn generate(&self, name: &str) -> Result<String, String> {
        let mut raw = [0u8; KEY_STEM_LEN];
        rand::thread_rng().fill(&mut raw);
        let stem = hex::encode(raw);
        let full_key = format!("{KEY_PREFIX}{stem}");

        let hash = hash_key(&full_key);

        let prefix: String = full_key.chars().take(6).collect();
        let suffix: String = full_key
            .chars()
            .rev()
            .take(KEY_ID_LEN)
            .collect::<String>()
            .chars()
            .rev()
            .collect();

        let created_at = chrono_now(self.tz_offset_secs);

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO keys (name, hash, prefix, suffix, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![name, hash, prefix, suffix, created_at],
        )
        .map_err(|e| format!("insert key: {e}"))?;

        Ok(full_key)
    }

    /// Revoke a key by name or id. Returns (id, name) of revoked key, or None.
    pub fn revoke(&self, target: &str) -> Result<Option<(u64, String)>, String> {
        let conn = self.conn.lock().unwrap();

        // Find the key first
        let row = conn
            .query_row(
                "SELECT id, name FROM keys WHERE name = ?1 OR CAST(id AS TEXT) = ?1",
                rusqlite::params![target],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .ok();

        match row {
            Some((id, name)) => {
                conn.execute("DELETE FROM keys WHERE id = ?1", rusqlite::params![id])
                    .map_err(|e| format!("delete key: {e}"))?;
                Ok(Some((id as u64, name)))
            }
            None => Ok(None),
        }
    }

    /// List all keys.
    pub fn list(&self) -> Result<Vec<KeyInfo>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, name, prefix, suffix, created_at FROM keys ORDER BY id")
            .map_err(|e| format!("prepare: {e}"))?;

        let rows = stmt
            .query_map([], |r| {
                Ok(KeyInfo {
                    id: r.get::<_, i64>(0)? as u64,
                    name: r.get(1)?,
                    partial: format!("{}…{}", r.get::<_, String>(2)?, r.get::<_, String>(3)?),
                    created_at: r.get(4)?,
                })
            })
            .map_err(|e| format!("query: {e}"))?;

        let mut keys: Vec<KeyInfo> = rows.filter_map(|r| r.ok()).collect();
        keys.sort_by_key(|k| k.id);
        Ok(keys)
    }

    /// Validate a raw API key. Returns true if the key exists.
    pub fn validate(&self, raw_key: &str) -> Result<bool, String> {
        let hash = hash_key(raw_key);
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM keys WHERE hash = ?1",
                rusqlite::params![hash],
                |r| r.get(0),
            )
            .map_err(|e| format!("validate: {e}"))?;
        Ok(count > 0)
    }

    /// Look up the display name for a raw API key.
    pub fn lookup_name(&self, raw_key: &str) -> Option<String> {
        let hash = hash_key(raw_key);
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT name FROM keys WHERE hash = ?1",
            rusqlite::params![hash],
            |r| r.get(0),
        )
        .ok()
    }
}

/// ISO-8601 timestamp in the configured fixed offset (e.g. "2026-08-17T10:30:00+07:00").
/// Falls back to UTC if the offset is out of range.
fn chrono_now(offset_secs: i32) -> String {
    let tz = chrono::FixedOffset::east_opt(offset_secs)
        .unwrap_or_else(|| chrono::FixedOffset::east_opt(0).unwrap());
    chrono::Utc::now()
        .with_timezone(&tz)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
