mod admin;
mod auth;
mod cli;
mod client;
mod config;
mod error;
mod handlers;
mod key_manager;
mod server;

use clap::Parser;
use cli::{Cli, CliAction, Command, KeyAction, key_table};
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
                let key_value = km.generate(&name).map_err(ProxyError::Internal)?;
                println!("API key generated (save it — shown only once!):");
                println!();
                println!("  {key_value}");
                println!();
                println!("Use: Authorization: Bearer ***");
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

        None => server::serve("config.toml", "keys.json", admin::DEFAULT_SOCKET).await,
    }
}
