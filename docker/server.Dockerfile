# syntax=docker/dockerfile:1.7

# Minimal Alpine build of the rumble server.
# Server is pure Rust (no system audio libs), so we statically link with musl
# and ship the binary on a tiny alpine base.
#
# The web admin UI is a wasm bundle that rust-embed bakes into the server binary
# at compile time from `crates/server/web-dist/` (see crates/server/src/web/
# assets.rs). Those assets are .gitignore'd — only the index.html shell is
# tracked — so we build them here in a dedicated stage rather than relying on a
# pre-populated build context. This keeps the image correct by construction for
# both CI (ghcr) and a local `docker compose up --build`. The wasm stage runs on
# Debian, where wasm-pack's auto-downloaded wasm-opt/wasm-bindgen are glibc
# binaries that Just Work; the server stage stays alpine/musl.

# ---- web-builder: compile + stage the admin UI wasm bundle ----
FROM rust:bookworm AS web-builder

ARG WASM_PACK_VERSION=0.13.1

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        protobuf-compiler \
        brotli \
        gzip \
        curl \
        ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && rustup target add wasm32-unknown-unknown \
 && curl -fsSL \
        "https://github.com/rustwasm/wasm-pack/releases/download/v${WASM_PACK_VERSION}/wasm-pack-v${WASM_PACK_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
      | tar -xz --strip-components=1 -C /usr/local/bin \
        "wasm-pack-v${WASM_PACK_VERSION}-x86_64-unknown-linux-musl/wasm-pack"

WORKDIR /src
COPY . .

# Produces crates/server/web-dist/{index.html, pkg/*, *.br, *.gz}.
RUN tools/build_admin_web.sh

# ---- server-builder: static musl build of the server binary ----
FROM rust:alpine AS builder

RUN apk add --no-cache \
        musl-dev \
        protobuf-dev \
        binutils

WORKDIR /src
COPY . .
# Overlay the freshly built UI bundle so rust-embed bakes the real assets
# (not just the tracked index.html placeholder) into the binary.
COPY --from=web-builder /src/crates/server/web-dist/ crates/server/web-dist/

RUN cargo build --release -p server --bin server \
 && strip target/release/server

FROM alpine:3

RUN apk add --no-cache ca-certificates tini \
 && adduser -D -H -u 10001 rumble

COPY --from=builder /src/target/release/server /usr/local/bin/server

USER rumble
# QUIC voice/control (UDP) + web admin (TCP). tini (PID 1) forwards SIGHUP to
# the server so a certbot deploy-hook can trigger a live cert reload.
EXPOSE 5000/udp 5001/tcp
ENTRYPOINT ["/sbin/tini", "--", "/usr/local/bin/server"]
