#!/usr/bin/env bash
#
# Run cargo against the local vendor/damascene checkout instead of the
# git-pinned rev. Useful while iterating on damascene alongside rumble.
#
# Wraps `cargo --config .cargo/local-damascene.toml <args>` and backs up /
# restores Cargo.lock around the call, so the on-disk lockfile keeps
# referencing the git pin. CI and other machines are unaffected — they
# never see the gitignored `.cargo/local-damascene.toml`.
#
# Usage:
#   scripts/damascene-local.sh build -p rumble-damascene
#   scripts/damascene-local.sh run   -p rumble-damascene
#   scripts/damascene-local.sh test  -p rumble-damascene
#
# Prerequisites:
#   - vendor/damascene/ exists and contains the damascene workspace
#   - .cargo/local-damascene.toml exists (template lives in this commit; the
#     real file is gitignored so each dev can tweak paths if needed)

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
overlay="$root/.cargo/local-damascene.toml"
lockfile="$root/Cargo.lock"

if [[ ! -f "$overlay" ]]; then
    echo "error: $overlay not found" >&2
    echo "create it with a [patch.\"https://github.com/computer-whisperer/damascene\"] block" >&2
    echo "pointing at vendor/damascene/crates/<damascene-core,damascene-wgpu,damascene-winit-wgpu>." >&2
    exit 1
fi

if [[ ! -d "$root/vendor/damascene" ]]; then
    echo "error: $root/vendor/damascene not found" >&2
    exit 1
fi

backup=""
if [[ -f "$lockfile" ]]; then
    backup="$(mktemp "${lockfile}.damascene-local.XXXXXX")"
    cp -p "$lockfile" "$backup"
fi

restore() {
    if [[ -n "$backup" && -f "$backup" ]]; then
        mv "$backup" "$lockfile"
    fi
}
trap restore EXIT

cd "$root"
cargo --config "$overlay" "$@"
