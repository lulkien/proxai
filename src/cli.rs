use clap::{Parser, Subcommand};

/// OpenAI-compatible API proxy with multi-provider routing.
#[derive(Parser)]
#[command(name = "proxai", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Start the proxy server
    Serve {
        /// Path to TOML config file
        #[arg(long, default_value = "config.toml")]
        config: String,

        /// Path to keys SQLite database
        #[arg(long, default_value = "keys.db")]
        key: String,

        /// Path to admin Unix socket
        #[arg(long, default_value = crate::admin::DEFAULT_SOCKET)]
        socket: String,

        /// Path to dashboard WASM dist directory
        #[arg(long)]
        dashboard_dist: Option<String>,
    },

    /// Client for admin operations over Unix socket
    Cli {
        /// Path to admin Unix socket
        #[arg(long, default_value = crate::admin::DEFAULT_SOCKET)]
        socket: String,

        #[command(subcommand)]
        action: CliAction,
    },

    /// Local key management (direct SQLite, no server needed)
    Key {
        /// Path to keys SQLite database
        #[arg(long, default_value = "keys.db")]
        key: String,

        #[command(subcommand)]
        action: KeyAction,
    },
}

#[derive(Subcommand)]
pub enum CliAction {
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
pub enum KeyAction {
    /// Generate a new API key (writes directly to keys.db)
    Generate {
        /// Name for the key
        name: String,
    },
    /// List all keys in keys.db
    List,
}

pub fn key_table(keys: &[crate::key_manager::KeyInfo]) {
    println!("{:<4} {:<20} {:<24} CREATED", "#", "NAME", "KEY");
    for k in keys {
        println!(
            "{:<4} {:<20} {:<24} {}",
            k.id, k.name, k.partial, k.created_at
        );
    }
}
