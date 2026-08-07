# ProxAI build recipes (https://github.com/casey/just)

set positional-arguments := false

_default:
    @just --list

# Compile SCSS to CSS
css:
    sass dashboard/styles.scss dashboard/styles.css --no-source-map --style=compressed

# Build the server binary (static dashboard embedded from dashboard/)
server: css
    cargo build --release

# Build everything
all: server

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
    rm -f dashboard/styles.css

# Bump version (usage: just bump 1.3.0)
bump VERSION:
    sed -i 's/^version = ".*"/version = "{{VERSION}}"/' Cargo.toml
    git add Cargo.toml
    git commit -m "chore: bump version to {{VERSION}}"
    git tag v{{VERSION}}
    @echo "Bumped to {{VERSION}}. Run 'git push && git push origin v{{VERSION}}' to publish."
