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

# Crates excluded from CI lints. These are reference-only per CLAUDE.md
# ("Do not add features to them unless explicitly asked"); we keep them
# building but don't gate CI on their clippy output.
DEPRECATED_CRATES=(
    rumble-egui
    rumble-next
    rumble-widgets
    harness-cli
)

run_clippy() {
    local exclude_args=()
    for crate in "${DEPRECATED_CRATES[@]}"; do
        exclude_args+=(--exclude "$crate")
    done

    echo "==> clippy --all-targets -D warnings (excluding ${DEPRECATED_CRATES[*]})"
    cargo clippy --workspace --all-targets --locked \
        "${exclude_args[@]}" \
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
