# ProxAI

OpenAI-compatible API proxy with multi-provider routing, key management, and a static web dashboard.

## Features

- **Multi-provider routing** -- route requests to DeepSeek, Kimi, OpenAI, etc. based on model name
- **API key auth** -- generate and manage client API keys (SHA-256 hashed, stored in SQLite)
- **Dynamic model discovery** -- auto-discovers models from upstream providers at startup
- **Rate limiting** -- 20 failed auth attempts per IP in 60s returns 429
- **Admin socket** -- Unix socket RPC for key management (no API key needed)
- **Usage tracking** -- per-key request counts persisted in SQLite
- **Static dashboard** -- plain HTML dashboard at `/dashboard` with stats and key management

## Quick start

```
# Build (requires sass CLI: npm install -g sass)
just all

# Edit config with your provider API key
cp pkg/config.example.toml config.toml
vim config.toml

# Generate a client key
./target/release/proxai key --key keys.db generate my-client

# Start the proxy
./target/release/proxai serve --config config.toml --key keys.db

# Use it
curl http://127.0.0.1:3000/v1/chat/completions \
  -H "Authorization: Bearer ***" \
  -H "Content-Type: application/json" \
  -d '{"model":"deepseek-v4-pro","messages":[{"role":"user","content":"hi"}]}'
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

[[providers]]
name = "deepseek"
url = "https://api.deepseek.com"
api_key = "sk-..."
```

## CLI

```
proxai serve --config config.toml --key keys.db [--socket /tmp/proxai.sock]

# Key management (offline)
proxai key --key keys.db generate <name>
proxai key --key keys.db list

# Key management (via admin socket)
proxai cli --socket /tmp/proxai.sock generate-key <name>
proxai cli --socket /tmp/proxai.sock list-keys
proxai cli --socket /tmp/proxai.sock revoke-key <name-or-id>
```

## Dashboard

Static HTML + SCSS. Two tabs:

- **Usage** -- request counts per key, expandable per-model breakdown
- **Keys** -- generate, list, and revoke keys with confirmation modal

Dashboard files live in `dashboard/` and are compiled into the server binary via `rust-embed`. No WASM, no separate deploy step.

SCSS is compiled at build time by the `css` just recipe:

```
just css           # sass dashboard/styles.scss -> dashboard/styles.css
just server        # compile server (embeds dashboard/)
just deb           # package as .deb
```

Or the one-liner:

```
just all deb
```

## Architecture

```
Client -> :3000/v1/* (API key auth) -> upstream provider
Client -> :3000/dashboard/* (optional password) -> static dashboard
Admin  -> Unix socket (local, no auth) -> key management RPC
```

| Module | Purpose |
|--------|---------|
| `server.rs` | Axum router, model discovery, serves embedded dashboard |
| `handlers.rs` | `/v1/chat/completions`, `/v1/responses`, `/v1/models` |
| `auth.rs` | API key middleware, rate limiter, AuthInfo injection |
| `key_manager.rs` | SQLite-backed key CRUD, auto-migration from keys.json |
| `storage.rs` | SQLite usage tracking (proxai.db) |
| `metrics.rs` | UsageTracker wrapper for storage |
| `webui.rs` | Dashboard API routes (stats, key CRUD) |
| `dashboard_assets.rs` | rust-embed: embeds dashboard HTML/CSS into binary |
| `admin.rs` | Unix socket bincode RPC |
| `config.rs` | TOML config deserialization |
| `cli.rs` | Clap CLI definitions |
| `client.rs` | Admin socket client |
| `dashboard/` | Static HTML + SCSS dashboard |

## Deployment

Single binary with everything baked in. Build:

```
just css            # compile SCSS
just server         # compile server (embeds dashboard)
just deb            # package as .deb
```

Or the one-liner:

```
just all deb
```

Installs to:

```
/usr/bin/proxai
/lib/systemd/system/proxai.service
/usr/share/doc/proxai/config.example.toml
```

Post-install creates `/var/lib/proxai/`, copies config to `/etc/proxai/` on first install.

No separate dashboard upload needed -- the HTML/CSS assets are compiled into the binary via `rust-embed`. One `scp + dpkg -i` deploys everything.

## Disclaimer

This project was written by AI (Hermes Agent / Claude). Use at your own discretion.

## License

Unlicense
