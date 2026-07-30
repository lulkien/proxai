use crate::{
    admin::{AdminRequest, AdminResponse},
    error::{ProxyError, Result},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub async fn cli_generate_key(socket_path: &str, name: &str) -> Result<()> {
    let response = send_admin_request(
        socket_path,
        &AdminRequest::GenerateKey {
            name: name.to_string(),
        },
    )
    .await?;

    match response {
        AdminResponse::KeyGenerated(resp) => {
            println!("API key generated (save it — shown only once!):");
            println!();
            println!("  {}", resp.key);
            println!();
            println!("Use: Authorization: Bearer {}", resp.key);
        }
        AdminResponse::Error(e) => eprintln!("Server error: {e}"),
        _ => eprintln!("Unexpected response"),
    }
    Ok(())
}

pub async fn cli_list_keys(socket_path: &str) -> Result<()> {
    let response = send_admin_request(socket_path, &AdminRequest::ListKeys).await?;

    match response {
        AdminResponse::KeyList(keys) => {
            if keys.is_empty() {
                println!("No keys.");
            } else {
                println!("{:<4} {:<20} {:<24} CREATED", "#", "NAME", "KEY");
                for k in &keys {
                    println!(
                        "{:<4} {:<20} {:<24} {}",
                        k.id, k.name, k.partial, k.created_at
                    );
                }
            }
        }
        AdminResponse::Error(e) => eprintln!("Server error: {e}"),
        _ => eprintln!("Unexpected response"),
    }
    Ok(())
}

pub async fn cli_revoke_key(socket_path: &str, target: &str) -> Result<()> {
    let response = send_admin_request(
        socket_path,
        &AdminRequest::RevokeKey {
            target: target.to_string(),
        },
    )
    .await?;

    match response {
        AdminResponse::KeyRevoked { id, name } => {
            println!("Key revoked: #{id} {name}");
        }
        AdminResponse::Error(e) => eprintln!("Server error: {e}"),
        _ => eprintln!("Unexpected response"),
    }
    Ok(())
}

pub async fn send_admin_request(
    socket_path: &str,
    request: &AdminRequest,
) -> std::result::Result<AdminResponse, ProxyError> {
    use tokio::net::UnixStream;

    let mut stream = UnixStream::connect(socket_path).await.map_err(|e| {
        ProxyError::Internal(format!(
            "cannot connect to admin socket {socket_path}: {e}\n\
             Is the server running? Start with: proxai serve --socket {socket_path}"
        ))
    })?;

    let payload = bincode::serialize(request).map_err(|e| ProxyError::Internal(e.to_string()))?;
    let len = (payload.len() as u32).to_le_bytes();

    stream.write_all(&len).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let resp_len = u32::from_le_bytes(len_buf) as usize;

    let mut resp_payload = vec![0u8; resp_len];
    stream.read_exact(&mut resp_payload).await?;

    bincode::deserialize(&resp_payload).map_err(|e| ProxyError::Internal(e.to_string()))
}
