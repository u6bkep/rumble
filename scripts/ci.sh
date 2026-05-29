#!/usr/bin/env bash
# Local pre-flight check that mirrors .github/workflows/ci.yml.
# Run this before pushing to catch CI failures locally.
#
# Usage:
#   scripts/ci.sh                # run every check
#   scripts/ci.sh fmt            # only rustfmt
#   scripts/ci.sh clippy         # only clippy
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

case "${1:-all}" in
    all)
        run_fmt
        run_clippy
        ;;
    fmt)
        run_fmt
        ;;
    clippy)
        run_clippy
        ;;
    *)
        echo "Unknown check: $1" >&2
        echo "Usage: $0 [all|fmt|clippy]" >&2
        exit 2
        ;;
esac

echo "==> ok"
