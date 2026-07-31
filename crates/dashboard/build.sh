#!/usr/bin/env bash
# Build the WASM dashboard and fix asset paths for /dashboard sub-path.
set -euo pipefail
cd "$(dirname "$0")"

dx build

DIST="../../target/dx/proxai-dashboard/debug/web/public"

# Fix absolute paths to be relative (respects <base href="/dashboard/">)
sed -i 's|/\./wasm/|./wasm/|g' "$DIST/index.html"
sed -i 's|"/\./wasm/|"./wasm/|g' "$DIST/wasm/proxai-dashboard.js"

echo "Dashboard built -> $DIST"
