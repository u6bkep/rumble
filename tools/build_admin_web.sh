#!/usr/bin/env bash
#
# Build the rumble-admin-web wasm bundle and stage it where the server's
# embedded asset handler (rust-embed `web-dist/`) and dev `assets_dir`
# serving expect it.
#
# Output layout (under crates/server/web-dist/):
#   index.html                       <- the page shell (copied from the crate)
#   pkg/rumble_admin_web.js          <- wasm-bindgen JS glue
#   pkg/rumble_admin_web_bg.wasm     <- the compiled module
#
# index.html imports `./pkg/rumble_admin_web.js` (relative), so the same
# file works whether served embedded or from disk.
#
# Usage:
#   tools/build_admin_web.sh           # release build (default)
#   tools/build_admin_web.sh --dev     # unoptimized (faster compile, slower run)
#
# Requires: wasm-pack (cargo install wasm-pack).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

PROFILE_FLAG="--release"
for arg in "$@"; do
    case "$arg" in
        --release) PROFILE_FLAG="--release" ;;
        --dev)     PROFILE_FLAG="--dev" ;;
        -h|--help)
            sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "unknown arg: $arg (try --help)" >&2; exit 2 ;;
    esac
done

CRATE_DIR="$REPO_ROOT/crates/rumble-admin-web"
DIST_DIR="$REPO_ROOT/crates/server/web-dist"

if ! command -v wasm-pack >/dev/null 2>&1; then
    echo "error: wasm-pack not found. Install it with: cargo install wasm-pack" >&2
    exit 1
fi

echo "==> building rumble-admin-web (wasm, $PROFILE_FLAG)"
cd "$REPO_ROOT"
# `--target web` emits ES-module glue whose default export is the init
# Promise that index.html imports and calls.
wasm-pack build "$CRATE_DIR" --target web "$PROFILE_FLAG" --out-dir "$DIST_DIR/pkg"

echo "==> staging index.html"
cp "$CRATE_DIR/index.html" "$DIST_DIR/index.html"

echo
echo "==> bundle staged in $DIST_DIR/"
echo "    Serve it by enabling [web] in the server config, then open the bind address."
