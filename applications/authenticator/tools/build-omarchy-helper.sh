#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
APP_DIR="$(cd -- "$SCRIPT_DIR/.." && pwd)"
ROOT_DIR="$(cd -- "$APP_DIR/../.." && pwd)"
YARN_JS="$ROOT_DIR/.yarn/releases/yarn-4.18.0.cjs"
BINARY="$APP_DIR/src-tauri/target/release/proton-authenticator"
DIST_DIR="$APP_DIR/dist"
FINGERPRINT_ROOT="$APP_DIR/src-tauri/target/release/.fingerprint"
VERIFY_SCRIPT="$SCRIPT_DIR/verify-omarchy-helper-build.mjs"

if [[ "$(node -p 'process.versions.node')" != 24.18.* ]]; then
  printf 'Node 24.18.x is required for the pinned helper build\n' >&2
  exit 1
fi
if [[ ! -f "$YARN_JS" ]]; then
  printf 'pinned Yarn missing: %s\n' "$YARN_JS" >&2
  exit 1
fi

# The whole of `dist` is embedded verbatim in the release binary, so stale
# bundles from earlier builds and webpack's production source maps would ship
# to the user alongside the current one.
rm -rf "$DIST_DIR"
node "$YARN_JS" workspace proton-authenticator build:web
find "$DIST_DIR" -type f -name '*.map' -delete
(
  cd "$APP_DIR/src-tauri"
  cargo build --bins --features tauri/custom-protocol --release
)

if [[ ! -x "$BINARY" ]]; then
  printf 'helper build missing: %s\n' "$BINARY" >&2
  exit 1
fi
node "$VERIFY_SCRIPT" "$FINGERPRINT_ROOT" "$DIST_DIR" "$BINARY"
sha256sum "$BINARY"
