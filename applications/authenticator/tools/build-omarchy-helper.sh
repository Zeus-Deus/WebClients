#!/usr/bin/env bash
set -euo pipefail

# Builds the Omarchy helper from the pinned toolchain, verifies the artifact,
# and — with `--install` — installs it to the path the systemd unit runs and
# restarts the unit. Every step that inspects the artifact runs against the
# file that will actually execute, so a stale install cannot pass silently.

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
APP_DIR="$(cd -- "$SCRIPT_DIR/.." && pwd)"
ROOT_DIR="$(cd -- "$APP_DIR/../.." && pwd)"
YARN_JS="$ROOT_DIR/.yarn/releases/yarn-4.18.0.cjs"
BINARY="$APP_DIR/src-tauri/target/release/proton-authenticator"
DIST_DIR="$APP_DIR/dist"
FINGERPRINT_ROOT="$APP_DIR/src-tauri/target/release/.fingerprint"
VERIFY_SCRIPT="$SCRIPT_DIR/verify-omarchy-helper-build.mjs"
INSTALL_DIR="${OMARCHY_HELPER_INSTALL_DIR:-$HOME/.local/opt/proton-authenticator-omarchy-helper}"
INSTALL_BINARY="$INSTALL_DIR/proton-authenticator"
UNIT="proton-authenticator-omarchy-helper.service"

INSTALL=0
for argument in "$@"; do
  case "$argument" in
    --install) INSTALL=1 ;;
    *) printf 'unknown argument: %s\n' "$argument" >&2; exit 2 ;;
  esac
done

if [[ "$(node -p 'process.versions.node')" != 24.18.* ]]; then
  printf 'Node 24.18.x is required for the pinned helper build\n' >&2
  exit 1
fi
if [[ ! -f "$YARN_JS" ]]; then
  printf 'pinned Yarn missing: %s\n' "$YARN_JS" >&2
  exit 1
fi

SOURCE_COMMIT="$(git -C "$ROOT_DIR" rev-parse HEAD)"
if [[ -n "$(git -C "$ROOT_DIR" status --porcelain --untracked-files=no -- applications/authenticator)" ]]; then
  printf 'refusing to build the helper from a dirty applications/authenticator tree\n' >&2
  git -C "$ROOT_DIR" status --short --untracked-files=no -- applications/authenticator >&2
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
node "$VERIFY_SCRIPT" "$FINGERPRINT_ROOT" "$DIST_DIR" "$BINARY" "$SOURCE_COMMIT"
BUILT_SHA="$(sha256sum "$BINARY" | cut -d' ' -f1)"
printf 'built  %s  %s  %s\n' "$BUILT_SHA" "$SOURCE_COMMIT" "$BINARY"

if [[ "$INSTALL" -eq 0 ]]; then
  printf 'not installed; rerun with --install to replace %s\n' "$INSTALL_BINARY"
  exit 0
fi

# Install atomically next to the running binary, then re-verify the installed
# file itself rather than trusting the copy.
mkdir -p "$INSTALL_DIR"
chmod 0700 "$INSTALL_DIR"
STAGED="$INSTALL_DIR/.proton-authenticator.new"
install -m 0755 "$BINARY" "$STAGED"
mv -f "$STAGED" "$INSTALL_BINARY"
node "$VERIFY_SCRIPT" "$FINGERPRINT_ROOT" "$DIST_DIR" "$INSTALL_BINARY" "$SOURCE_COMMIT"
INSTALLED_SHA="$(sha256sum "$INSTALL_BINARY" | cut -d' ' -f1)"
if [[ "$INSTALLED_SHA" != "$BUILT_SHA" ]]; then
  printf 'installed binary hash %s does not match build %s\n' "$INSTALLED_SHA" "$BUILT_SHA" >&2
  exit 1
fi
printf '%s %s\n' "$SOURCE_COMMIT" "$INSTALLED_SHA" > "$INSTALL_DIR/PROVENANCE"
chmod 0600 "$INSTALL_DIR/PROVENANCE"
printf 'installed  %s  %s  %s\n' "$INSTALLED_SHA" "$SOURCE_COMMIT" "$INSTALL_BINARY"

if systemctl --user is-enabled --quiet "$UNIT" 2>/dev/null || systemctl --user is-active --quiet "$UNIT" 2>/dev/null; then
  systemctl --user restart "$UNIT"
  sleep 2
  systemctl --user --no-pager --lines=0 status "$UNIT" || true
  PID="$(systemctl --user show -p MainPID --value "$UNIT")"
  if [[ -z "$PID" || "$PID" == 0 ]]; then
    printf 'helper unit did not come up after install\n' >&2
    exit 1
  fi
  RUNNING="$(readlink "/proc/$PID/exe")"
  if [[ "$RUNNING" != "$INSTALL_BINARY" ]]; then
    printf 'running helper is %s, expected %s\n' "$RUNNING" "$INSTALL_BINARY" >&2
    exit 1
  fi
  printf 'running    pid %s  %s\n' "$PID" "$RUNNING"
fi
