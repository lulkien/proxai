# ProxAI

OpenAI-compatible API proxy with multi-provider routing, key management, and a WASM dashboard.

## Features

- **Multi-provider routing** -- route requests to DeepSeek, Kimi, OpenAI, etc. based on model name
- **API key auth** -- generate and manage client API keys (SHA-256 hashed, stored in SQLite)
- **Dynamic model discovery** -- auto-discovers models from upstream providers at startup
- **Rate limiting** -- 20 failed auth attempts per IP in 60s returns 429
- **Admin socket** -- Unix socket RPC for key management (no API key needed)
- **Usage tracking** -- per-key request counts persisted in SQLite
- **WASM dashboard** -- Dioxus web UI at `/dashboard` with stats and key management

## Quick start

```
# Build
cargo build --release

# Edit config with your provider API key
cp pkg/config.example.toml config.toml
vim config.toml

# Generate a client key
./target/release/proxai key --key keys.db generate my-client

# Start the proxy
./target/release/proxai serve --config config.toml --key keys.db

# Use it
curl http://127.0.0.1:3000/v1/chat/completions \
  -H "Authorization: Bearer sk-..." \
  -H "Content-Type: application/json" \
  -d '{"model":"deepseek-chat","messages":[{"role":"user","content":"hi"}]}'
```

Dashboard at `http://127.0.0.1:3000/dashboard`.

## Configuration

```toml
# config.toml
bind = "127.0.0.1:3000"

# Optional: dashboard auth
# dashboard_password = "changeme"

# Optional: SQLite paths
# db_path = "/var/lib/proxai/proxai.db"
# dashboard_dist = "/var/lib/proxai/dashboard-dist"

[[providers]]
name = "deepseek"
url = "https://api.deepseek.com/v1"
api_key = "sk-..."
```

## CLI

```
proxai serve --config config.toml --key keys.db [--socket /tmp/proxai.sock] [--dashboard-dist path]

# Key management (offline)
proxai key --key keys.db generate <name>
proxai key --key keys.db list

# Key management (via admin socket)
proxai cli --socket /tmp/proxai.sock generate-key <name>
proxai cli --socket /tmp/proxai.sock list-keys
proxai cli --socket /tmp/proxai.sock revoke-key <name-or-id>
```

## Dashboard

Built with Dioxus 0.6 (WASM). Two tabs:

- **Usage** -- request counts per key, expandable per-model breakdown
- **Keys** -- generate, list, and revoke keys with confirmation modal

The dashboard files are compiled separately:

```
cd crates/dashboard
bash build.sh    # runs `dx build` + path fix
```

The server serves them from a directory specified via `--dashboard-dist`.

## Architecture

```
Client -> :3000/v1/* (API key auth) -> upstream provider
Client -> :3000/dashboard/* (optional password) -> WASM dashboard
Admin  -> Unix socket (local, no auth) -> key management RPC
```

| Module | Purpose |
|--------|---------|
| `server.rs` | Axum router, model discovery, static file serving |
| `handlers.rs` | `/v1/chat/completions`, `/v1/responses`, `/v1/models` |
| `auth.rs` | API key middleware, rate limiter, AuthInfo injection |
| `key_manager.rs` | SQLite-backed key CRUD, auto-migration from keys.json |
| `storage.rs` | SQLite usage tracking (proxai.db) |
| `metrics.rs` | UsageTracker wrapper for storage |
| `webui.rs` | Dashboard API routes (stats, key CRUD) |
| `admin.rs` | Unix socket bincode RPC |
| `config.rs` | TOML config deserialization |
| `cli.rs` | Clap CLI definitions |
| `client.rs` | Admin socket client |
| `crates/dashboard/` | Dioxus WASM dashboard app |

## Deployment

Debian package:

```
cargo build --release
cargo deb
# produces target/debian/proxai_0.1.0-1_amd64.deb
```

Installs to:
```
/usr/bin/proxai
/lib/systemd/system/proxai.service
/usr/share/doc/proxai/config.example.toml
```

Post-install creates `/var/lib/proxai/`, copies config to `/etc/proxai/` on first install.

Service file expects dashboard dist at `/var/lib/proxai/dashboard-dist/`. Upload separately:

```
cd crates/dashboard && bash build.sh
scp -r target/dx/proxai-dashboard/debug/web/public/* root@host:/var/lib/proxai/dashboard-dist/
```

## Disclaimer

This project was written by AI (Hermes Agent / Claude). Use at your own discretion.

## License

MIT
