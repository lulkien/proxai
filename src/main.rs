mod admin;
mod auth;
mod cli;
mod client;
mod config;
mod error;
mod handlers;
mod key_manager;
mod server;

use cli::{find_positional, parse_flag, print_key_table, print_usage};
use client::{cli_generate_key, cli_list_keys, cli_revoke_key};
use error::{ProxyError, Result};
use key_manager::KeyManager;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "proxai=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(|s| s.as_str()) {
        Some("serve") => {
            let config_path = parse_flag(&args, "--config", "config.toml");
            let key_path = parse_flag(&args, "--key", "keys.json");
            let socket_path = parse_flag(&args, "--socket", admin::DEFAULT_SOCKET);
            server::serve(&config_path, &key_path, &socket_path).await
        }

        Some("cli") => {
            let socket_path = parse_flag(&args, "--socket", admin::DEFAULT_SOCKET);
            let action = find_positional(&args, 2, &["--socket"]);
            match action.as_deref() {
                Some("generate-key") => {
                    let name = find_positional(&args, 2, &["--socket", "generate-key"])
                        .ok_or_else(|| {
                            ProxyError::InvalidRequest(
                                "usage: proxai cli --socket <path> generate-key <name>".into(),
                            )
                        })?;
                    cli_generate_key(&socket_path, &name).await
                }
                Some("list-keys") => cli_list_keys(&socket_path).await,
                Some("revoke-key") => {
                    let target = find_positional(&args, 2, &["--socket", "revoke-key"])
                        .ok_or_else(|| {
                            ProxyError::InvalidRequest(
                                "usage: proxai cli --socket <path> revoke-key <name-or-id>".into(),
                            )
                        })?;
                    cli_revoke_key(&socket_path, &target).await
                }
                _ => {
                    eprintln!(
                        "usage: proxai cli --socket <path> <generate-key|list-keys|revoke-key> [arg]"
                    );
                    Ok(())
                }
            }
        }

        Some("key") => {
            let key_path = parse_flag(&args, "--key", "keys.json");
            match args.get(2).map(|s| s.as_str()) {
                Some("generate") => {
                    let name = find_positional(&args, 3, &["--key"]).ok_or_else(|| {
                        ProxyError::InvalidRequest(
                            "usage: proxai key generate --key <path> <name>".into(),
                        )
                    })?;
                    let km = KeyManager::new(&key_path);
                    let key = km.generate(&name).map_err(ProxyError::Internal)?;
                    println!("API key generated (save it — shown only once!):");
                    println!();
                    println!("  {key}");
                    println!();
                    println!("Use: Authorization: Bearer {key}");
                    Ok(())
                }
                Some("list") => {
                    let km = KeyManager::new(&key_path);
                    let keys = km.list().map_err(ProxyError::Internal)?;
                    if keys.is_empty() {
                        println!("No keys in {key_path}");
                    } else {
                        println!("Keys in {key_path}:");
                        print_key_table(&keys);
                    }
                    Ok(())
                }
                _ => {
                    eprintln!("usage: proxai key <generate|list> --key <path> [name]");
                    Ok(())
                }
            }
        }

        Some("--help" | "-h") | None => {
            print_usage();
            Ok(())
        }

        _ => {
            let config_path = "config.toml".to_string();
            let key_path = "keys.json".to_string();
            server::serve(&config_path, &key_path, admin::DEFAULT_SOCKET).await
        }
    }
}
