#!/usr/bin/env bash
# Build the WASM dashboard and fix asset paths for /dashboard sub-path.
# SCSS -> CSS compilation is handled by build.rs (grass crate).
set -euo pipefail
cd "$(dirname "$0")"

dx build

DIST="../../target/dx/proxai-dashboard/debug/web/public"
PKG="../../pkg/dashboard-dist"

# Fix absolute paths to be relative (respects <base href="/dashboard/">)
sed -i 's|/\./wasm/|./wasm/|g' "$DIST/index.html"
sed -i 's|"/\./wasm/|"./wasm/|g' "$DIST/wasm/proxai-dashboard.js"

# Copy to pkg/ for deb packaging
rm -rf "$PKG"
cp -r "$DIST" "$PKG"

echo "Dashboard built -> $DIST"
echo "Copied to pkg -> $PKG"
