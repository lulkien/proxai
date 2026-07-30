use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

const KEY_ID_LEN: usize = 4;
const KEY_STEM_LEN: usize = 32; // raw bytes
const KEY_PREFIX: &str = "sk-";

/// How long a key stays cached in memory after last successful use.
const CACHE_TTL_SECS: u64 = 300; // 5 minutes

/// Structure of the keys.json file on disk.
#[derive(Debug, Serialize, Deserialize)]
struct KeyStore {
    next_id: u64,
    keys: Vec<KeyEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct KeyEntry {
    id: u64,
    name: String,
    /// SHA-256 hex digest of the full API key.
    hash: String,
    /// First 6 chars of the plaintext key (for display).
    prefix: String,
    /// Last 4 chars of the plaintext key (for display).
    suffix: String,
    created_at: String,
}

/// Cached key entry in memory.
#[derive(Debug, Clone)]
struct CachedKey {
    id: u64,
    name: String,
    prefix: String,
    suffix: String,
    created_at: String,
    /// Expiration time. Refreshed on each successful auth.
    expires_at: Instant,
}

/// Information shown by `--list-keys`. Does not contain the full key.
#[derive(Debug, Serialize, Deserialize)]
pub struct KeyInfo {
    pub id: u64,
    pub name: String,
    pub partial: String,
    pub created_at: String,
}

pub struct KeyManager {
    path: String,
    /// In-memory cache: hash -> CachedKey
    cache: Mutex<HashMap<String, CachedKey>>,
}

impl KeyManager {
    pub fn new(path: &str) -> Self {
        let km = Self {
            path: path.to_string(),
            cache: Mutex::new(HashMap::new()),
        };
        // Load existing keys into cache on construction
        if let Err(e) = km.warm_cache() {
            tracing::warn!("Failed to load keys into cache on startup: {e}");
        }
        km
    }

    /// Generate a new API key, store its hash + metadata, return the plaintext
    /// key ONCE. The caller should display it and then drop it.
    pub fn generate(&self, name: &str) -> Result<String, String> {
        let mut store = self.load_or_init()?;

        let mut raw = [0u8; KEY_STEM_LEN];
        rand::thread_rng().fill(&mut raw);
        let stem = hex::encode(raw);
        let full_key = format!("{KEY_PREFIX}{stem}");

        let hash = hex::encode(Sha256::digest(full_key.as_bytes()));

        let prefix = full_key.chars().take(6).collect::<String>();
        let suffix = full_key
            .chars()
            .rev()
            .take(KEY_ID_LEN)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();

        let id = store.next_id;
        store.next_id += 1;

        let entry = KeyEntry {
            id,
            name: name.to_string(),
            hash: hash.clone(),
            prefix: prefix.clone(),
            suffix: suffix.clone(),
            created_at: chrono_now(),
        };
        store.keys.push(entry);

        // Persist to disk
        self.save(&store)?;

        // Add to in-memory cache immediately
        let mut cache = self.cache.lock().unwrap();
        cache.insert(
            hash.clone(),
            CachedKey {
                id,
                name: name.to_string(),
                prefix,
                suffix,
                created_at: chrono_now(),
                expires_at: Instant::now() + std::time::Duration::from_secs(CACHE_TTL_SECS),
            },
        );

        Ok(full_key)
    }

    /// List all keys (from cache — fast, no I/O).
    pub fn list(&self) -> Result<Vec<KeyInfo>, String> {
        let mut cache = self.cache.lock().unwrap();
        self.evict_expired_locked(&mut cache);

        let mut keys: Vec<KeyInfo> = cache
            .values()
            .map(|k| KeyInfo {
                id: k.id,
                name: k.name.clone(),
                partial: format!("{}…{}", k.prefix, k.suffix),
                created_at: k.created_at.clone(),
            })
            .collect();
        keys.sort_by_key(|k| k.id);
        Ok(keys)
    }

    /// Validate a raw API key. Returns Ok(true) if the key is known.
    /// Checks in-memory cache first (fast path), falls back to disk on miss.
    pub fn validate(&self, raw_key: &str) -> Result<bool, String> {
        let hash = hex::encode(Sha256::digest(raw_key.as_bytes()));

        // Fast path: check in-memory cache
        {
            let mut cache = self.cache.lock().unwrap();
            if let Some(entry) = cache.get_mut(&hash) {
                if entry.expires_at > Instant::now() {
                    // Refresh TTL on successful auth
                    entry.expires_at =
                        Instant::now() + std::time::Duration::from_secs(CACHE_TTL_SECS);
                    return Ok(true);
                }
                // Expired — remove from cache, fall through to disk
                cache.remove(&hash);
            }
        }

        // Slow path: read from disk
        let store = self.load_or_init()?;
        if let Some(entry) = store.keys.iter().find(|k| k.hash == hash) {
            // Add to cache for next time
            let mut cache = self.cache.lock().unwrap();
            cache.insert(
                hash,
                CachedKey {
                    id: entry.id,
                    name: entry.name.clone(),
                    prefix: entry.prefix.clone(),
                    suffix: entry.suffix.clone(),
                    created_at: entry.created_at.clone(),
                    expires_at: Instant::now() + std::time::Duration::from_secs(CACHE_TTL_SECS),
                },
            );
            return Ok(true);
        }

        Ok(false)
    }

    // ── internal helpers ──

    /// Load all keys from disk into the in-memory cache.
    fn warm_cache(&self) -> Result<(), String> {
        let store = self.load_or_init()?;
        let mut cache = self.cache.lock().unwrap();
        let expires_at = Instant::now() + std::time::Duration::from_secs(CACHE_TTL_SECS);
        for entry in &store.keys {
            cache.insert(
                entry.hash.clone(),
                CachedKey {
                    id: entry.id,
                    name: entry.name.clone(),
                    prefix: entry.prefix.clone(),
                    suffix: entry.suffix.clone(),
                    created_at: entry.created_at.clone(),
                    expires_at,
                },
            );
        }
        Ok(())
    }

    /// Remove expired entries from the cache. Caller must hold the lock.
    fn evict_expired_locked(&self, cache: &mut HashMap<String, CachedKey>) {
        let now = Instant::now();
        cache.retain(|_, v| v.expires_at > now);
    }

    fn load_or_init(&self) -> Result<KeyStore, String> {
        if Path::new(&self.path).exists() {
            let data =
                std::fs::read_to_string(&self.path).map_err(|e| format!("read keys: {e}"))?;
            serde_json::from_str(&data).map_err(|e| format!("parse keys: {e}"))
        } else {
            Ok(KeyStore {
                next_id: 1,
                keys: vec![],
            })
        }
    }

    fn save(&self, store: &KeyStore) -> Result<(), String> {
        let data = serde_json::to_string_pretty(store).map_err(|e| format!("serialize: {e}"))?;
        std::fs::write(&self.path, data).map_err(|e| format!("write keys: {e}"))
    }
}

/// Simple ISO-8601 timestamp without pulling in chrono crate.
fn chrono_now() -> String {
    use std::time::SystemTime;
    let t = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = t.as_secs();
    let offset_secs = 7 * 3600;
    let local = secs + offset_secs;
    let days = local / 86400;
    let rem = local % 86400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;

    let (y, mo, d) = days_to_ymd(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}+07:00")
}

fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
