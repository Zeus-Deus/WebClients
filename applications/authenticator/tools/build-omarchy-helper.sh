#!/usr/bin/env bash
set -euo pipefail

# Builds the Omarchy helper and verifies the artifact before anything installs
# it. Runs from a git checkout or from Proton's release tarball with the
# Omarchy patch applied (the AUR package path). Installing is the package
# manager's job; this script never touches the system.
#
# Source commit: from a git checkout it is `git rev-parse HEAD` and the tree
# must be clean. From a tarball the packager exports OMARCHY_HELPER_SOURCE_COMMIT
# (the fork commit the patch was generated from), which build.rs embeds and the
# verifier then requires in the binary.

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
APP_DIR="$(cd -- "$SCRIPT_DIR/.." && pwd)"
ROOT_DIR="$(cd -- "$APP_DIR/../.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$APP_DIR/src-tauri/target}"
BINARY="$TARGET_DIR/release/proton-authenticator"
FINGERPRINT_ROOT="$TARGET_DIR/release/.fingerprint"
DIST_DIR="$APP_DIR/dist"
VERIFY_SCRIPT="$SCRIPT_DIR/verify-omarchy-helper-build.mjs"

for argument in "$@"; do
  printf 'unknown argument: %s\n' "$argument" >&2
  exit 2
done

# Proton's 1.1.6 workspace declares node ">= 22.14.0 <23.6.0".
NODE_VERSION="$(node -p 'process.versions.node')"
case "$NODE_VERSION" in
  22.*) ;;
  *) printf 'Node 22.x is required by the Proton workspace (found %s)\n' "$NODE_VERSION" >&2; exit 1 ;;
esac

YARN_PATH="$(sed -n 's/^yarnPath: *//p' "$ROOT_DIR/.yarnrc.yml")"
YARN_JS="$ROOT_DIR/$YARN_PATH"
if [[ -z "$YARN_PATH" || ! -f "$YARN_JS" ]]; then
  printf 'pinned Yarn missing: %s\n' "$YARN_JS" >&2
  exit 1
fi

if git -C "$ROOT_DIR" rev-parse --is-inside-work-tree >/dev/null 2>&1 \
  && [[ "$(git -C "$ROOT_DIR" rev-parse --show-toplevel)" == "$ROOT_DIR" ]]; then
  SOURCE_COMMIT="$(git -C "$ROOT_DIR" rev-parse HEAD)"
  if [[ -n "$(git -C "$ROOT_DIR" status --porcelain --untracked-files=no -- applications/authenticator)" ]]; then
    printf 'refusing to build the helper from a dirty applications/authenticator tree\n' >&2
    git -C "$ROOT_DIR" status --short --untracked-files=no -- applications/authenticator >&2
    exit 1
  fi
else
  SOURCE_COMMIT="${OMARCHY_HELPER_SOURCE_COMMIT:-}"
fi
if [[ ! "$SOURCE_COMMIT" =~ ^[0-9a-f]{40}$ ]]; then
  printf 'OMARCHY_HELPER_SOURCE_COMMIT must name the 40-hex patched source commit\n' >&2
  exit 1
fi
export OMARCHY_HELPER_SOURCE_COMMIT="$SOURCE_COMMIT"

# The whole of `dist` is embedded verbatim in the release binary, so stale
# bundles and webpack's production source maps would ship with it. QA_BUILD=false
# is load-bearing: Proton's own tools/build.sh enables devtools and QA hooks
# whenever it is unset.
export QA_BUILD=false NODE_ENV=production API_ENV=proton.me
rm -rf "$DIST_DIR"
node "$YARN_JS" workspace proton-authenticator build:web
find "$DIST_DIR" -type f -name '*.map' -delete
(
  cd "$APP_DIR/src-tauri"
  cargo build --frozen --bins --features tauri/custom-protocol --release
)

if [[ ! -x "$BINARY" ]]; then
  printf 'helper build missing: %s\n' "$BINARY" >&2
  exit 1
fi
node "$VERIFY_SCRIPT" "$FINGERPRINT_ROOT" "$DIST_DIR" "$BINARY" "$SOURCE_COMMIT"
printf 'built  %s  %s  %s\n' "$(sha256sum "$BINARY" | cut -d' ' -f1)" "$SOURCE_COMMIT" "$BINARY"
