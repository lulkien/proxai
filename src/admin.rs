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

/// Default admin socket name (bound in the abstract namespace as `@proxai`).
pub const DEFAULT_SOCKET: &str = "proxai";

/// Build an abstract-namespace address for `name` (Linux). The name is used
/// verbatim — no leading NUL — and shows up as `@name`.
pub fn abstract_addr(name: &str) -> std::io::Result<std::os::unix::net::SocketAddr> {
    use std::os::linux::net::SocketAddrExt;
    std::os::unix::net::SocketAddr::from_abstract_name(name)
}

#[derive(Debug, Serialize, Deserialize)]
pub enum AdminRequest {
    GenerateKey { name: String },
    ListKeys,
    RevokeKey { target: String },
    GetStats,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GenerateKeyResponse {
    pub id: String,
    pub name: String,
    pub key: String,
    pub partial: String,
    pub created_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum AdminResponse {
    KeyGenerated(GenerateKeyResponse),
    KeyList(Vec<KeyInfo>),
    KeyRevoked { id: String, name: String },
    Stats(UsageSnapshot),
    Error(String),
}

/// Bind the admin listener on the abstract-namespace socket `@{name}`.
/// Returns the bound listener so the caller can abort startup if binding
/// fails.
///
/// Abstract sockets have no filesystem path: there is no stale file to
/// remove and the kernel drops the name automatically when the listener
/// closes. The trade-off vs. the old filesystem socket is that abstract
/// sockets ignore file permissions — any local process that knows the name
/// can connect, so the previous 0600 owner-only guarantee no longer applies.
/// (Peer authorization could be restored with an SO_PEERCRED check on
/// accept if the host ever runs untrusted local processes.)
pub fn bind(name: &str) -> std::io::Result<UnixListener> {
    use std::os::unix::net::UnixListener as StdUnixListener;

    let addr = abstract_addr(name)?;
    let std_listener = StdUnixListener::bind_addr(&addr)?;
    std_listener.set_nonblocking(true)?;
    let listener = UnixListener::from_std(std_listener)?;

    info!("Admin socket listening on @{name} (abstract, local)");

    Ok(listener)
}

/// Run the Unix socket admin server on an already-bound listener. Blocks until error.
pub async fn run(listener: UnixListener, key_manager: Arc<KeyManager>, tracker: Arc<UsageTracker>) {
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
            Ok(new_key) => AdminResponse::KeyGenerated(GenerateKeyResponse {
                id: new_key.id,
                name: new_key.name,
                key: new_key.key,
                partial: new_key.partial,
                created_at: new_key.created_at,
            }),
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
        AdminRequest::GetStats => match km.active_hashes() {
            Ok(active) => AdminResponse::Stats(tracker.snapshot(&active)),
            Err(e) => AdminResponse::Error(e),
        },
    }
}
