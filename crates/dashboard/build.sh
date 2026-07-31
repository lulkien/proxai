#!/usr/bin/env bash
# Build the WASM dashboard and fix asset paths for /dashboard sub-path.
set -euo pipefail
cd "$(dirname "$0")"

# Compile SCSS to CSS
echo "Compiling SCSS..."
if command -v sass &>/dev/null; then
  sass styles.scss styles.css --no-source-map
elif command -v dart-sass &>/dev/null; then
  dart-sass styles.scss styles.css --no-source-map
else
  echo "WARNING: sass not found, using existing styles.css"
fi

dx build

DIST="../../target/dx/proxai-dashboard/debug/web/public"

# Fix absolute paths to be relative (respects <base href="/dashboard/">)
sed -i 's|/\./wasm/|./wasm/|g' "$DIST/index.html"
sed -i 's|"/\./wasm/|"./wasm/|g' "$DIST/wasm/proxai-dashboard.js"

# Copy compiled CSS to dist
cp styles.css "$DIST/"

echo "Dashboard built -> $DIST"
