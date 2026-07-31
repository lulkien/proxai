#!/usr/bin/env bash
# Build the WASM dashboard and fix asset paths for /dashboard sub-path.
# SCSS -> CSS compilation is handled by build.rs (grass crate).
set -euo pipefail
cd "$(dirname "$0")"

dx build

DIST="../../target/dx/proxai-dashboard/debug/web/public"

# Fix absolute paths to be relative (respects <base href="/dashboard/">)
sed -i 's|/\./wasm/|./wasm/|g' "$DIST/index.html"
sed -i 's|"/\./wasm/|"./wasm/|g' "$DIST/wasm/proxai-dashboard.js"

# Copy compiled CSS to dist
cp styles.css "$DIST/"

echo "Dashboard built -> $DIST"
