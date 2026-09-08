use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, path::Path, sync::Mutex};

const KEY_ID_BYTES: usize = 8;
const KEY_SUFFIX_LEN: usize = 4;
const KEY_PREFIX: &str = "sk-";

/// Compute SHA-256 hex digest of a raw API key.
pub fn hash_key(raw_key: &str) -> String {
    hex::encode(Sha256::digest(raw_key.as_bytes()))
}

/// A freshly generated key: plaintext `key` plus the metadata persisted
/// alongside it. Returned from a single locked section so callers never
/// have to re-query to find the row they just inserted.
#[derive(Debug)]
pub struct NewKey {
    pub id: String,
    pub name: String,
    pub key: String,
    pub partial: String,
    pub created_at: String,
}

/// Information shown by list-keys. Does not contain the full key.
#[derive(Debug, Serialize, Deserialize)]
pub struct KeyInfo {
    pub id: String,
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
                 id TEXT PRIMARY KEY NOT NULL,
                 name TEXT NOT NULL,
                 hash TEXT NOT NULL UNIQUE,
                 prefix TEXT NOT NULL,
                 suffix TEXT NOT NULL,
                 created_at TEXT NOT NULL
             );",
        )
        .map_err(|e| format!("migrate: {e}"))?;

        migrate_legacy_integer_ids(&conn)?;

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
            // Legacy keys.json had sequential integer ids; ignore them and
            // assign a fresh random text id per migrated key.
            let id = {
                let mut id_raw = [0u8; KEY_ID_BYTES];
                rand::thread_rng().fill(&mut id_raw);
                hex::encode(id_raw)
            };
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

    /// Generate a new API key, returns the plaintext ONCE alongside the
    /// persisted metadata (random text id, partial, created_at).
    pub fn generate(&self, name: &str) -> Result<NewKey, String> {
        let mut raw = [0u8; 32];
        rand::thread_rng().fill(&mut raw);
        let stem = hex::encode(raw);
        let full_key = format!("{KEY_PREFIX}{stem}");

        let hash = hash_key(&full_key);

        let prefix: String = full_key.chars().take(6).collect();
        let suffix: String = full_key
            .chars()
            .rev()
            .take(KEY_SUFFIX_LEN)
            .collect::<String>()
            .chars()
            .rev()
            .collect();

        // Random 8-byte id (16 hex chars): non-sequential, no key-count
        // leakage, collision odds negligible at this scale.
        let mut id_raw = [0u8; KEY_ID_BYTES];
        rand::thread_rng().fill(&mut id_raw);
        let id = hex::encode(id_raw);

        let created_at = chrono_now(self.tz_offset_secs);

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO keys (id, name, hash, prefix, suffix, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, name, hash, prefix, suffix, created_at],
        )
        .map_err(|e| format!("insert key: {e}"))?;

        Ok(NewKey {
            id,
            name: name.to_string(),
            key: full_key,
            partial: format!("{prefix}…{suffix}"),
            created_at,
        })
    }

    /// Revoke a key by name or id. Returns (id, name) of revoked key, or None.
    pub fn revoke(&self, target: &str) -> Result<Option<(String, String)>, String> {
        let conn = self.conn.lock().unwrap();

        // Find the key first
        let row = conn
            .query_row(
                "SELECT id, name FROM keys WHERE name = ?1 OR id = ?1",
                rusqlite::params![target],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .ok();

        match row {
            Some((id, name)) => {
                conn.execute("DELETE FROM keys WHERE id = ?1", rusqlite::params![id])
                    .map_err(|e| format!("delete key: {e}"))?;
                Ok(Some((id, name)))
            }
            None => Ok(None),
        }
    }

    /// Hashes of all keys that currently exist (i.e. not revoked).
    /// Used to distinguish active keys from deleted ones in usage stats,
    /// since usage rows are kept after a key is revoked.
    pub fn active_hashes(&self) -> Result<HashSet<String>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT hash FROM keys")
            .map_err(|e| format!("prepare: {e}"))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| format!("query: {e}"))?;
        let mut active = HashSet::new();
        for hash in rows.flatten() {
            active.insert(hash);
        }
        Ok(active)
    }

    /// List all keys, oldest first.
    pub fn list(&self) -> Result<Vec<KeyInfo>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, name, prefix, suffix, created_at FROM keys ORDER BY rowid")
            .map_err(|e| format!("prepare: {e}"))?;

        let rows = stmt
            .query_map([], |r| {
                Ok(KeyInfo {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    partial: format!("{}…{}", r.get::<_, String>(2)?, r.get::<_, String>(3)?),
                    created_at: r.get(4)?,
                })
            })
            .map_err(|e| format!("query: {e}"))?;

        let keys: Vec<KeyInfo> = rows.filter_map(|r| r.ok()).collect();
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

/// Rebuild `keys` when it still uses the pre-uuid `id INTEGER PRIMARY KEY`
/// schema, backfilling a random 8-byte hex id per row. Runs once on open of
/// a legacy database; new databases already use TEXT ids and are untouched.
fn migrate_legacy_integer_ids(conn: &rusqlite::Connection) -> Result<(), String> {
    let id_type: Option<String> = conn
        .prepare("SELECT type FROM pragma_table_info('keys') WHERE name = 'id'")
        .map_err(|e| format!("schema check: {e}"))?
        .query_row([], |r| r.get(0))
        .ok();

    if id_type.as_deref() != Some("INTEGER") {
        return Ok(());
    }

    tracing::info!("keys table uses legacy integer ids — migrating to random text ids");
    conn.execute_batch(
        "BEGIN;
         ALTER TABLE keys RENAME TO keys_old;
         CREATE TABLE keys (
             id TEXT PRIMARY KEY NOT NULL,
             name TEXT NOT NULL,
             hash TEXT NOT NULL UNIQUE,
             prefix TEXT NOT NULL,
             suffix TEXT NOT NULL,
             created_at TEXT NOT NULL
         );
         INSERT INTO keys (id, name, hash, prefix, suffix, created_at)
             SELECT lower(hex(randomblob(8))), name, hash, prefix, suffix, created_at
             FROM keys_old;
         DROP TABLE keys_old;
         COMMIT;",
    )
    .map_err(|e| format!("migrate integer ids: {e}"))?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn is_hex16(s: &str) -> bool {
        s.len() == 16 && s.chars().all(|c| c.is_ascii_hexdigit())
    }

    #[test]
    fn generate_returns_random_text_ids() {
        let km = KeyManager::open(":memory:").unwrap();
        let alice = km.generate("alice").unwrap();
        let bob = km.generate("bob").unwrap();

        assert!(is_hex16(&alice.id), "id must be 16 hex chars");
        assert!(is_hex16(&bob.id));
        assert_ne!(alice.id, bob.id, "ids must not repeat");
        assert!(alice.key.starts_with("sk-"));

        // List preserves insertion order and carries the same ids.
        let keys = km.list().unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].id, alice.id);
        assert_eq!(keys[1].id, bob.id);

        // Revoke by the text id works and returns it.
        let revoked = km.revoke(&alice.id).unwrap();
        assert_eq!(revoked, Some((alice.id.clone(), "alice".to_string())));
        assert!(!km.validate(&alice.key).unwrap());
        assert!(km.validate(&bob.key).unwrap());
    }

    #[test]
    fn revoke_by_name_still_works() {
        let km = KeyManager::open(":memory:").unwrap();
        let key = km.generate("carol").unwrap();
        let revoked = km.revoke("carol").unwrap();
        assert_eq!(revoked, Some((key.id.clone(), "carol".to_string())));
    }

    #[test]
    fn legacy_integer_id_db_migrates_to_text_ids() {
        let raw = "sk-legacy-raw-key-for-test";
        let path =
            std::env::temp_dir().join(format!("proxai-km-legacy-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // Build a database with the old INTEGER id schema.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE keys (
                     id INTEGER PRIMARY KEY,
                     name TEXT NOT NULL,
                     hash TEXT NOT NULL UNIQUE,
                     prefix TEXT NOT NULL,
                     suffix TEXT NOT NULL,
                     created_at TEXT NOT NULL
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO keys (name, hash, prefix, suffix, created_at)
                     VALUES ('legacy', ?1, 'sk-abc', 'wxyz', '2020-01-01T00:00:00+00:00')",
                rusqlite::params![hash_key(raw)],
            )
            .unwrap();
        }

        // Opening migrates: integer ids become random 16-hex text ids.
        let path_str = path.to_str().unwrap();
        let km = KeyManager::open_with_tz(path_str, 0).unwrap();
        let keys = km.list().unwrap();
        assert_eq!(keys.len(), 1);
        assert!(is_hex16(&keys[0].id));
        assert_eq!(keys[0].name, "legacy");
        assert_eq!(keys[0].created_at, "2020-01-01T00:00:00+00:00");
        assert!(km.validate(raw).unwrap(), "key must survive migration");

        // Reopening must not migrate twice or duplicate rows.
        let km2 = KeyManager::open_with_tz(path_str, 0).unwrap();
        assert_eq!(km2.list().unwrap().len(), 1);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}.wal", path.display()));
        let _ = std::fs::remove_file(format!("{}.shm", path.display()));
    }
}
