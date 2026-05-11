#!/usr/bin/env bash
#
# Run cargo against the local vendor/aetna checkout instead of the
# git-pinned rev. Useful while iterating on aetna alongside rumble.
#
# Wraps `cargo --config .cargo/local-aetna.toml <args>` and backs up /
# restores Cargo.lock around the call, so the on-disk lockfile keeps
# referencing the git pin. CI and other machines are unaffected — they
# never see the gitignored `.cargo/local-aetna.toml`.
#
# Usage:
#   scripts/aetna-local.sh build -p rumble-aetna
#   scripts/aetna-local.sh run   -p rumble-aetna
#   scripts/aetna-local.sh test  -p rumble-aetna
#
# Prerequisites:
#   - vendor/aetna/ exists and contains the aetna workspace
#   - .cargo/local-aetna.toml exists (template lives in this commit; the
#     real file is gitignored so each dev can tweak paths if needed)

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
overlay="$root/.cargo/local-aetna.toml"
lockfile="$root/Cargo.lock"

if [[ ! -f "$overlay" ]]; then
    echo "error: $overlay not found" >&2
    echo "create it with a [patch.\"https://github.com/computer-whisperer/aetna\"] block" >&2
    echo "pointing at vendor/aetna/crates/<aetna-core,aetna-wgpu,aetna-winit-wgpu>." >&2
    exit 1
fi

if [[ ! -d "$root/vendor/aetna" ]]; then
    echo "error: $root/vendor/aetna not found" >&2
    exit 1
fi

backup=""
if [[ -f "$lockfile" ]]; then
    backup="$(mktemp "${lockfile}.aetna-local.XXXXXX")"
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
