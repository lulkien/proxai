# ProxAI build recipes (https://github.com/casey/just)

set positional-arguments := false

_default:
    @just --list

# Build the WASM dashboard only
dashboard:
    cd crates/dashboard && dx build
    sed -i 's|/\./wasm/|./wasm/|g' target/dx/proxai-dashboard/debug/web/public/index.html
    sed -i 's|"/\./wasm/|"./wasm/|g' target/dx/proxai-dashboard/debug/web/public/wasm/proxai-dashboard.js
    rm -rf pkg/dashboard-dist
    cp -r target/dx/proxai-dashboard/debug/web/public pkg/dashboard-dist
    @echo "Dashboard assets ready for embed"

# Build the server binary only
server: dashboard
    cargo build --release

# Build everything (dashboard + server)
all: dashboard
    cargo build --release

# Build the Debian package
deb: all
    cargo deb
    @echo "-> target/debian/proxai_*.deb"

# Run all checks
check:
    cargo fmt -- --check
    cargo clippy -- -D warnings
    cargo test

# Full CI pipeline
ci: check all deb

# Clean all build artifacts
clean:
    cargo clean
    rm -rf pkg/dashboard-dist
    rm -rf crates/dashboard/styles.css

# Bump version (usage: just bump 1.3.0)
bump VERSION:
    sed -i 's/^version = ".*"/version = "{{VERSION}}"/' Cargo.toml
    git add Cargo.toml
    git commit -m "chore: bump version to {{VERSION}}"
    git tag v{{VERSION}}
    @echo "Bumped to {{VERSION}}. Run 'git push && git push origin v{{VERSION}}' to publish."
