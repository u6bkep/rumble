#!/usr/bin/env bash
# Local pre-flight check that mirrors .github/workflows/ci.yml.
# Run this before pushing to catch CI failures locally.
#
# Usage:
#   scripts/ci.sh                # run every check
#   scripts/ci.sh fmt            # only rustfmt
#   scripts/ci.sh clippy         # only clippy
#   scripts/ci.sh admin-lint     # only the rumble-admin-web UI lint gate
#
# Requires: stable toolchain with clippy, nightly toolchain with rustfmt.
#   rustup toolchain install stable --component clippy
#   rustup toolchain install nightly --component rustfmt

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

run_fmt() {
    echo "==> rustfmt --check (nightly)"
    cargo +nightly fmt --all -- --check
}

run_clippy() {
    echo "==> clippy --all-targets -D warnings"
    cargo clippy --workspace --all-targets --locked \
        -- -D warnings
}

run_admin_lint() {
    echo "==> rumble-admin-web UI lint gate"
    cargo run --locked -p rumble-admin-web --bin lint
}

case "${1:-all}" in
    all)
        run_fmt
        run_clippy
        run_admin_lint
        ;;
    fmt)
        run_fmt
        ;;
    clippy)
        run_clippy
        ;;
    admin-lint)
        run_admin_lint
        ;;
    *)
        echo "Unknown check: $1" >&2
        echo "Usage: $0 [all|fmt|clippy|admin-lint]" >&2
        exit 2
        ;;
esac

echo "==> ok"
