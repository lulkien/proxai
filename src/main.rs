mod admin;
mod auth;
mod client;
mod config;
mod error;
mod handlers;
mod key_manager;
mod server;

use clap::{Parser, Subcommand};
use client::{cli_generate_key, cli_list_keys, cli_revoke_key};
use error::{ProxyError, Result};
use key_manager::KeyManager;

/// OpenAI-compatible API proxy with multi-provider routing.
#[derive(Parser)]
#[command(name = "proxai", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start the proxy server
    Serve {
        /// Path to TOML config file
        #[arg(long, default_value = "config.toml")]
        config: String,

        /// Path to keys.json file
        #[arg(long, default_value = "keys.json")]
        key: String,

        /// Path to admin Unix socket
        #[arg(long, default_value = admin::DEFAULT_SOCKET)]
        socket: String,
    },

    /// Client for admin operations over Unix socket
    Cli {
        /// Path to admin Unix socket
        #[arg(long, default_value = admin::DEFAULT_SOCKET)]
        socket: String,

        #[command(subcommand)]
        action: CliAction,
    },

    /// Local key management (direct filesystem, no server needed)
    Key {
        /// Path to keys.json file
        #[arg(long, default_value = "keys.json")]
        key: String,

        #[command(subcommand)]
        action: KeyAction,
    },
}

#[derive(Subcommand)]
enum CliAction {
    /// Generate a new API key
    GenerateKey {
        /// Name for the key
        name: String,
    },
    /// List all API keys
    ListKeys,
    /// Revoke an API key by name or ID
    RevokeKey {
        /// Key name or numeric ID
        target: String,
    },
}

#[derive(Subcommand)]
enum KeyAction {
    /// Generate a new API key (writes directly to keys.json)
    Generate {
        /// Name for the key
        name: String,
    },
    /// List all keys in keys.json
    List,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "proxai=info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Some(Command::Serve {
            config,
            key,
            socket,
        }) => server::serve(&config, &key, &socket).await,

        Some(Command::Cli { socket, action }) => match action {
            CliAction::GenerateKey { name } => cli_generate_key(&socket, &name).await,
            CliAction::ListKeys => cli_list_keys(&socket).await,
            CliAction::RevokeKey { target } => cli_revoke_key(&socket, &target).await,
        },

        Some(Command::Key { key, action }) => match action {
            KeyAction::Generate { name } => {
                let km = KeyManager::new(&key);
                let key = km.generate(&name).map_err(ProxyError::Internal)?;
                println!("API key generated (save it — shown only once!):");
                println!();
                println!("  {key}");
                println!();
                println!("Use: Authorization: Bearer {key}");
                Ok(())
            }
            KeyAction::List => {
                let km = KeyManager::new(&key);
                let keys = km.list().map_err(ProxyError::Internal)?;
                if keys.is_empty() {
                    println!("No keys in {key}");
                } else {
                    println!("Keys in {key}:");
                    key_table(&keys);
                }
                Ok(())
            }
        },

        // Default: start server (backward compat)
        None => server::serve("config.toml", "keys.json", admin::DEFAULT_SOCKET).await,
    }
}

fn key_table(keys: &[key_manager::KeyInfo]) {
    println!("{:<4} {:<20} {:<24} CREATED", "#", "NAME", "KEY");
    for k in keys {
        println!(
            "{:<4} {:<20} {:<24} {}",
            k.id, k.name, k.partial, k.created_at
        );
    }
}
