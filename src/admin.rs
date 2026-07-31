use crate::{
    key_manager::{KeyInfo, KeyManager},
    metrics::{UsageSnapshot, UsageTracker},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixListener,
};
use tracing::{error, info};

/// Default admin socket path.
pub const DEFAULT_SOCKET: &str = "/tmp/proxai.sock";

#[derive(Debug, Serialize, Deserialize)]
pub enum AdminRequest {
    GenerateKey { name: String },
    ListKeys,
    RevokeKey { target: String },
    GetStats,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GenerateKeyResponse {
    pub id: u64,
    pub name: String,
    pub key: String,
    pub partial: String,
    pub created_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum AdminResponse {
    KeyGenerated(GenerateKeyResponse),
    KeyList(Vec<KeyInfo>),
    KeyRevoked { id: u64, name: String },
    Stats(UsageSnapshot),
    Error(String),
}

/// Run the Unix socket admin server. Blocks until error.
pub async fn run(socket_path: &str, key_manager: Arc<KeyManager>, tracker: Arc<UsageTracker>) {
    // Remove stale socket if it exists
    let _ = std::fs::remove_file(socket_path);

    let listener = match UnixListener::bind(socket_path) {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to bind admin socket {socket_path}: {e}");
            return;
        }
    };

    info!("Admin socket listening on {socket_path}");

    loop {
        match listener.accept().await {
            Ok((mut stream, _)) => {
                let km = key_manager.clone();
                let tr = tracker.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(&mut stream, &km, &tr).await {
                        error!("Admin connection error: {e}");
                    }
                });
            }
            Err(e) => {
                error!("Admin socket accept error: {e}");
            }
        }
    }
}

async fn handle_connection(
    stream: &mut (impl AsyncReadExt + AsyncWriteExt + Unpin),
    km: &KeyManager,
    tracker: &UsageTracker,
) -> std::io::Result<()> {
    // Read 4-byte length prefix (little-endian u32)
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf) as usize;

    // Sanity check
    if len == 0 || len > 1024 * 1024 {
        return Err(std::io::Error::other("invalid request length"));
    }

    // Read payload
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;

    // Deserialize request
    let request: AdminRequest = bincode::deserialize(&payload)
        .map_err(|e| std::io::Error::other(format!("deserialize: {e}")))?;

    // Process
    let response = process_request(request, km, tracker);

    // Serialize response
    let resp_bytes = bincode::serialize(&response)
        .map_err(|e| std::io::Error::other(format!("serialize: {e}")))?;

    // Write 4-byte length prefix + payload
    let resp_len = (resp_bytes.len() as u32).to_le_bytes();
    stream.write_all(&resp_len).await?;
    stream.write_all(&resp_bytes).await?;
    stream.flush().await?;

    Ok(())
}

fn process_request(
    request: AdminRequest,
    km: &KeyManager,
    tracker: &UsageTracker,
) -> AdminResponse {
    match request {
        AdminRequest::GenerateKey { name } => match km.generate(&name) {
            Ok(key) => {
                // Re-read metadata for the response
                match km.list() {
                    Ok(keys) => {
                        if let Some(entry) = keys.last() {
                            AdminResponse::KeyGenerated(GenerateKeyResponse {
                                id: entry.id,
                                name: entry.name.clone(),
                                key,
                                partial: entry.partial.clone(),
                                created_at: entry.created_at.clone(),
                            })
                        } else {
                            AdminResponse::Error("key not persisted".into())
                        }
                    }
                    Err(e) => AdminResponse::Error(e),
                }
            }
            Err(e) => AdminResponse::Error(e),
        },
        AdminRequest::ListKeys => match km.list() {
            Ok(keys) => AdminResponse::KeyList(keys),
            Err(e) => AdminResponse::Error(e),
        },
        AdminRequest::RevokeKey { target } => match km.revoke(&target) {
            Ok(Some((id, name))) => AdminResponse::KeyRevoked { id, name },
            Ok(None) => AdminResponse::Error(format!("key not found: {target}")),
            Err(e) => AdminResponse::Error(e),
        },
        AdminRequest::GetStats => AdminResponse::Stats(tracker.snapshot()),
    }
}
