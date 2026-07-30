use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
const KEY_ID_LEN: usize = 4;
const KEY_STEM_LEN: usize = 32; // raw bytes
const KEY_PREFIX: &str = "sk-";

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
}

impl KeyManager {
    pub fn new(path: &str) -> Self {
        Self {
            path: path.to_string(),
        }
    }

    /// Generate a new API key, store its hash + metadata, return the plaintext
    /// key ONCE. The caller should display it and then drop it.
    pub fn generate(&self, name: &str) -> Result<String, String> {
        let mut store = self.load_or_init()?;

        // Generate key: sk-<64 hex chars> (32 random bytes)
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

        store.keys.push(KeyEntry {
            id,
            name: name.to_string(),
            hash,
            prefix,
            suffix,
            created_at: chrono_now(),
        });

        self.save(&store)?;
        Ok(full_key)
    }

    /// List all keys (without the secret).
    pub fn list(&self) -> Result<Vec<KeyInfo>, String> {
        let store = self.load_or_init()?;
        Ok(store
            .keys
            .iter()
            .map(|k| KeyInfo {
                id: k.id,
                name: k.name.clone(),
                partial: format!("{}…{}", k.prefix, k.suffix),
                created_at: k.created_at.clone(),
            })
            .collect())
    }

    /// Validate a raw API key. Returns Ok(true) if the key is known.
    pub fn validate(&self, raw_key: &str) -> Result<bool, String> {
        let store = self.load_or_init()?;
        let hash = hex::encode(Sha256::digest(raw_key.as_bytes()));
        Ok(store.keys.iter().any(|k| k.hash == hash))
    }

    // ── internal helpers ──

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
    // Manual offset calculation for +07
    let offset_secs = 7 * 3600;
    let local = secs + offset_secs;
    let days = local / 86400;
    let rem = local % 86400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;

    // Approximate Y/M/D from days since epoch
    let (y, mo, d) = days_to_ymd(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}+07:00")
}

fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
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
