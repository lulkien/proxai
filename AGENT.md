# AGENT.md — ProxAI

Guidance for AI coding agents working in this repository. Read this before
modifying code. It captures the architecture, invariants, and conventions of
the project; the README covers user-facing usage.

## Project overview

ProxAI is an OpenAI-compatible API proxy with multi-provider routing, client
API-key management, SQLite usage tracking, a Unix-socket admin RPC, and a
static HTML/SCSS dashboard embedded in the binary.

- Single binary crate, no `lib.rs`, no `src/bin/`. Modules are flat files in
  `src/` (main.rs is thin: mod declarations + clap dispatch).
- `edition = "2024"` (let-chains and other 2024 syntax are fair game). Rust
  toolchain in use is 1.96.
- The `[workspace] members = []` block in Cargo.toml is intentional-ish
  scaffolding: treat this as a single crate; do not add crate subdirectories
  without registering them as members.
- Key deps: tokio (full), axum 0.8, reqwest (default-features off,
  **rustls** — no OpenSSL, deliberate), rusqlite (bundled), serde/serde_json,
  toml, clap (derive), thiserror (declared but currently unused — error.rs
  hand-rolls), tracing/tracing-subscriber (env-filter), rust-embed,
  bincode, sha2, rand, hex, chrono, futures.

## Commands

Everything goes through `just` (see justfile):

- `just css` — compile `dashboard/styles.scss` -> `dashboard/styles.css` via
  grass (`cargo install grass`). `styles.css` is gitignored/generated.
- `just server` / `just all` — `css` + `cargo build --release`.
- `just deb` — cargo-deb package (metadata in `Cargo.toml`, scripts in `pkg/`).
- `just check` — `cargo fmt -- --check`, `cargo clippy -- -D warnings`,
  `cargo test`. Keep this green; CI runs it.
- `just bump <ver>` — bumps version, commits, tags locally (no push).

Gotchas:

- `build.rs` **fails the build** when `dashboard/styles.css` is missing — run
  `just css` first. `cargo build` alone will not regenerate it.
- Only files you changed get rustfmt'd (project convention); `cargo fmt`
  reads edition 2024 from the manifest, so let-chains format correctly.

## Runtime architecture

```
Client -> :3000/v1/*       (Bearer API key, 20 fails/IP/60s -> 429) -> upstream provider
Client -> :3000/dashboard  (optional Bearer dashboard_password)     -> static assets + JSON API
Admin  -> abstract socket @proxai (no auth)                 -> key RPC
```

### Module map (src/)

| Module | Responsibility |
|---|---|
| `main.rs` | mod decls, tracing init (`proxai=info` default), clap dispatch. No subcommand = `serve config.toml keys.db proxai` (@proxai). |
| `server.rs` | `ProxyState` (reqwest Client, Arc<Config>, Arc<HashMap<model_id, provider>>, Arc<UsageTracker>), axum router, model discovery, embedded-asset serving, MIME mapping. |
| `handlers.rs` | `list_models`, `chat_completions` (streaming + non-streaming paths). |
| `auth.rs` | `require_api_key` middleware, per-IP rate limiter, injects `AuthInfo {key_hash, key_name}` extension. |
| `key_manager.rs` | keys.db CRUD, SHA-256 hashing, keys.json auto-migration. Errors are `Result<_, String>`. |
| `storage.rs` | usage.db schema (usage + usage_totals + deleted_usage), `record()`, `snapshot()` (per-key aggregates + per-model breakdown over raw + counters, incl. aggregated "deleted keys" rollup row), `timeline()` (time-bucketed chart data), `consolidate_aged()` (folds raw rows past retention into counters), `consolidate_deleted()` (folds stale revoked keys into rollup + deletes rows). Errors `Result<_, String>`. |
| `metrics.rs` | `UsageTracker` (Arc<Storage> wrapper), serde snapshot structs served to dashboard/admin. `model_stats()` builds the Models tab rows (token fields serialize as JSON strings — BigInt-safe, see `token_as_string`). |
| `webui.rs` | `/dashboard/api/*` routes: stats, stats/models, timeline, key list/generate/revoke. |
| `admin.rs` | Unix-socket bincode RPC server (`AdminRequest`/`AdminResponse`), `bind()` + `run()`. |
| `client.rs` | CLI side of the admin socket (generate/list/revoke key). |
| `cli.rs` | clap types: `serve`, `cli` (socket), `key` (offline direct-db). |
| `config.rs` | TOML config + `timezone_offset()` parsing. |
| `error.rs` | `ProxyError` enum -> OpenAI-style JSON error envelope. |
| `dashboard_assets.rs` | rust-embed of `dashboard/`. |

### Data stores

Two independent SQLite DBs (both WAL, `synchronous=NORMAL`, std
`Mutex<Connection>`):

- **keys.db** (`--key`, default `keys.db`): `keys(id, name, hash, prefix,
  suffix, created_at)`. `id` is a random 8-byte hex string (16 chars, TEXT
  PRIMARY KEY) — never a sequential integer. `hash` = SHA-256 hex of the
  full `sk-` key; `prefix`/`suffix` are 6-char/4-char display fragments.
  Plaintext keys are printed **exactly once** at generation and never stored
  or logged. Databases created before text ids (legacy `id INTEGER PRIMARY
  KEY`) are rebuilt automatically on open, each row getting a fresh random
  id.
- **usage.db** (`config.db_path`, default `proxai.db`): per-request rows
  (`key_hash, key_name, model, prompt_tokens, completion_tokens,
  created_at`), `created_at` written by SQLite `datetime('now')` (UTC) and
  shifted to the configured timezone in queries. Raw rows older than
  `usage_retention_days` are folded into the `usage_totals` cumulative
  table (one row per key+model). Revoked keys idle >7d fold into the
  `deleted_usage` rollup (per-model totals, aggregated "deleted keys" row
  in stats). See invariants 2 and 3.

## Invariants and gotchas (project knowledge)

1. **Model ids are namespaced `provider/model`.** Discovered from each
   provider's `/models` at startup and stored as `HashMap<namespaced_id,
   provider_name>` (see `ModelDiscovery` — discovery also returns the
   allowlist-filtered ids, counted as "deactivated" on the dashboard).
   Requests must use the namespaced id; the prefix is
   stripped before forwarding upstream. Route resolution: `models` map ->
   provider config by name. Usage rows are recorded under the namespaced id.
   A provider's optional `models` array (config, `#[serde(default)]` empty)
   restricts advertising to those upstream ids **that the provider actually
   offers** — missing preferred ids are skipped with a warning, discovery
   still runs either way.
2. **Revoke keeps stats; consolidation reclaims rows.** Usage rows survive
   key revocation; `deleted` flags are derived per query by comparing
   against `KeyManager::active_hashes()`. The dashboard shows revoked keys
   with a "(deleted)" marker — do not filter them out of `snapshot`/
   `timeline`. A revoked key idle longer than `STALE_DELETED_KEY_DAYS` (7d)
   is folded by `Storage::consolidate_deleted` (run at startup + daily in
   `serve`) into the `deleted_usage` rollup table (per-model totals) and
   its original rows are **physically deleted** — from both `usage` and
   `usage_totals`. `snapshot()` then surfaces the rollup as ONE aggregated
   "deleted keys" row (flagged deleted) so all-time totals and per-model
   spend never shrink after a revoke.
3. **Raw rows age out via retention fold.** Raw per-request rows older than
   `config.usage_retention_days` (default 14, clamped >= 7 to cover the
   chart's max range) are folded by `Storage::consolidate_aged` into
   per-(key, model) cumulative counters in `usage_totals`, then deleted —
   so the `usage` table stays bounded (~retention window of traffic) no
   matter how long a key lives. `snapshot()` and the per-model breakdown
   UNION `usage` + `usage_totals`; `timeline()` reads only `usage` (raw
   rows inside 1d/7d windows, never older than retention). Revoked keys
   are skipped by the aged fold and handled wholesale by the deleted fold.
4. **Streaming token counting.** `handlers.rs` forces
   `stream_options.include_usage=true` upstream, tees the response body
   through a bounded `mpsc` channel (capacity 16) while a spawned task keeps
   only the trailing 64 KiB (UTF-8-safe via `append_tail`) of the SSE text,
   then parses the final `usage` chunk with `sse_usage_tokens`. Status,
   `content-type`, and `transfer-encoding` are passed through; the client
   body must stay byte-transparent.
5. **Admin socket is unauthenticated** — it lives in the Linux abstract
   namespace as `@proxai` (default; override with `--socket <name>`).
   Abstract sockets have no filesystem path, so there is no stale-file
   cleanup and no 0600-style permission model: any local process that
   knows the name can connect. Never add auth-optional remote exposure.
   (If the host runs untrusted local processes, restore peer authorization
   with an SO_PEERCRED check on accept.) Bind pattern mirrors the sgc
   daemon: `SocketAddrExt::from_abstract_name` -> std `bind_addr` ->
   `set_nonblocking` -> tokio `UnixListener::from_std` (client side does
   the same with `connect_addr`). Framing: 4-byte little-endian u32 length
   + bincode payload, max request 1 MiB. The systemd unit passes
   `--socket proxai`.
6. **Dashboard auth is optional and plaintext.** `dashboard_password` unset =
   open dashboard. It is compared with `==` against the Bearer token — no
   constant-time compare, acceptable because this is a convenience gate, not
   key auth. New dashboard endpoints must call `check_auth`.
7. **Timezone config.** `timezone` is a fixed-offset string (`"+07:00"`,
   default `+00:00`) parsed by `Config::timezone_offset()` into (seconds,
   SQL modifier). Known asymmetry: the SQL modifier only carries whole hours
   (`"+7 hours"`), so minute offsets shift chart bucketing imprecisely, and
   IANA names fall back to UTC. Keep this behavior or fix both sides
   together (tests in config.rs pin it).
8. **Rate limiter is process-global.** `static LazyLock<RateLimiter>` in
   auth.rs: 20 failed auth attempts per IP per 60 s -> 429. Reset on success.
9. **Sync SQLite behind std `Mutex` in async code** (KeyManager, Storage) is
   deliberate: operations are short and serialized. Never hold these locks
   across `.await`; never add long queries to request paths.
10. **HTTP error shape** is the OpenAI-style envelope
   `{"error": {"message", "type", "code"}}` produced by `ProxyError`
   (`IntoResponse`). Errors crossing the HTTP boundary map to `ProxyError`;
   SQLite-internal layers keep `Result<_, String>`. `thiserror` is available
   if a structured error ever needs deriving, but match existing hand-rolled
   style unless there is a reason.
11. **`/v1/responses` was removed** (commit 0dff06b) — do not resurrect.
    Only `/v1/models` and `/v1/chat/completions` exist behind auth.
12. **Dashboard is static HTML/JS** (the old WASM build is gone). Files in
    `dashboard/` are embedded at compile time via rust-embed — changes
    require a rebuild. `content_type()` in server.rs still lists `.wasm`
    (harmless leftover, unused).
13. **XSS rule for the dashboard:** every user/DB-derived string (key names,
    model names) interpolated into `innerHTML` must go through `esc()` in
    dashboard/app.js (added in e10c6ef). Never bypass it for new renderings.
14. **Legacy migration:** KeyManager auto-imports `keys.json` (cwd-relative)
    into an empty keys.db. Do not remove; do not rely on it for new keys.
15. **Secrets hygiene:** real provider `api_key`s and client keys must never
    be printed in server logs, committed, or written into docs. Config files
    with real keys are gitignored (`config.toml`, `keys.json`, `*.db`,
    `Cargo.lock` is also ignored — deliberate).
16. **reqwest client**: rustls (no OpenSSL), 120 s timeout, JSON + stream
    features. The same `Client` in `ProxyState` serves both discovery and
    proxying.

## Change checklists

- **New admin RPC op:** touch, in parallel: `admin.rs` (`AdminRequest`
  variant + `process_request` arm + response type), `client.rs` (CLI
  wrapper), `cli.rs` (`CliAction` variant), `main.rs` dispatch. Generate-key
  responses come from re-listing keys (server-side pattern in admin.rs —
  generate returns the plaintext once).
- **New dashboard endpoint:** add route + handler in `webui.rs`
  (`dashboard_api_router`), add the fetch + rendering in `dashboard/app.js`,
  escape interpolated strings, rebuild (`just css server` if styles change).
- **New provider feature:** model discovery + namespace handling live in
  `server.rs`; request rewrite/proxying in `handlers.rs`; usage counting in
  the same place the request is fulfilled. Remember both streaming and
  non-streaming paths.

## Rust conventions

- **Gates:** code must pass `cargo fmt --check`, `cargo clippy -- -D
  warnings`, `cargo test`. Fix lints rather than allowing them.
- **Style:** idiomatic, modern Rust (channels/OwnedFd/RAII over
  callbacks/raw fds); thin entry files; single-file modules; `let ... else`,
  if-let chains, `?` propagation, entry API, iterator chains — prefer these
  over manual indexing/unwrap-heavy code.
- **Error handling:** typed errors only where call sites repeat (the
  `ProxyError` boundary); one-off/internal failures stay string/context
  errors. Preserve the source chain; start messages lowercase, no trailing
  punctuation. `.expect()` only for bug-level invariants, never user input.
- **Concurrency:** `std::sync::Mutex` for short synchronous critical
  sections (SQLite); `tokio::sync::Mutex` only for async-shared state;
  bounded channels (`mpsc`) for async boundaries; clone `Arc`s before
  `tokio::spawn`; never hold a lock across `.await`; don't block the async
  runtime with CPU work.
- **Panics:** no `unwrap()`/panic on request paths. `unwrap()` in existing
  code is confined to guaranteed invariants (static-SQL `prepare`,
  infallible conversions) — keep new code equally disciplined or propagate.
- **Logging:** `tracing` with structured fields (`info!`, `warn!`,
  `error!`); log each error exactly once with its chain; `RUST_LOG` filters
  at runtime (default `proxai=info`). `println!` belongs only in CLI
  commands (main.rs/client.rs output paths).
- **Tests:** unit tests in `#[cfg(test)] mod tests` per module with
  `use super::*;`; `#[tokio::test]` for async; SQLite tests on `:memory:`;
  descriptive names; keep Arrange/Act/Assert clear. New behavior ships with
  tests (see config.rs, storage.rs, auth.rs, handlers.rs, server.rs).
- **Serde:** derive on wire types; `#[serde(default)]` for optional config
  fields (config.rs is the model).
- **Docs:** `///` doc comments on public items, `//!` for module docs where
  the module has non-obvious contracts. Keep README + this file honest when
  behavior changes.

## Docs and history conventions

- Root README is intentionally minimal and links outward; user docs belong
  under `docs/` (none exist yet — create it if a doc needs a home).
- This file records the current design only — no rejected-alternative or
  decision-change history.
- Work happens on feature branches, one objective at a time; commit only on
  explicit "ok commit"; never push without instruction.
