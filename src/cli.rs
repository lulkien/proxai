use crate::key_manager::KeyInfo;

/// Parse a `--flag <value>` pair from argv. Returns `default` if not found.
pub fn parse_flag(args: &[String], flag: &str, default: &str) -> String {
    for i in 0..args.len() {
        if args[i] == flag
            && let Some(val) = args.get(i + 1)
            && !val.starts_with('-')
        {
            return val.clone();
        }
    }
    default.to_string()
}

/// Extract the first positional argument starting at `start`,
/// skipping known flags and their values.
pub fn find_positional(args: &[String], start: usize, skip_flags: &[&str]) -> Option<String> {
    let mut i = start;
    while i < args.len() {
        if skip_flags.contains(&args[i].as_str()) {
            if args[i].starts_with("--") {
                i += 2; // --flag <value>
            } else {
                i += 1; // bare positional marker
            }
        } else {
            return Some(args[i].clone());
        }
    }
    None
}

pub fn print_key_table(keys: &[KeyInfo]) {
    println!("{:<4} {:<20} {:<24} CREATED", "#", "NAME", "KEY");
    for k in keys {
        println!(
            "{:<4} {:<20} {:<24} {}",
            k.id, k.name, k.partial, k.created_at
        );
    }
}

pub fn print_usage() {
    println!("proxai — OpenAI-compatible proxy");
    println!();
    println!("Server:");
    println!("  proxai serve --config <path> --key <path> [--socket <path>]");
    println!();
    println!("Client (talks to server via Unix socket):");
    println!("  proxai cli --socket <path> generate-key <name>");
    println!("  proxai cli --socket <path> list-keys");
    println!("  proxai cli --socket <path> revoke-key <name-or-id>");
    println!();
    println!("Local bootstrap (direct filesystem):");
    println!("  proxai key generate --key <path> <name>");
    println!("  proxai key list --key <path>");
    println!();
    println!("  proxai (no args)  Start server (config.toml, keys.json, /tmp/proxai.sock)");
}
