# syntax=docker/dockerfile:1.7

# Combined build for both server-side docker images: the rumble server and the
# Mumble<->Rumble bridge. Both are pure-Rust, statically linked against musl and
# shipped on a tiny alpine base.
#
# One Dockerfile with a shared `builder` stage so the large overlapping musl
# dependency graph (quinn/rustls/prost/tokio/…) compiles ONCE for both binaries
# instead of twice in two independent builds. Select which image to produce with
# `--target server` or `--target bridge`; CI builds both against a single shared
# BuildKit/gha cache, so the second target reuses the first's compiled `builder`
# stage. `docker compose` picks the target per service (see docker-compose.yml).
#
# The server embeds the wasm admin UI (rust-embed bakes crates/server/web-dist/
# into the binary at compile time — see crates/server/src/web/assets.rs). Those
# assets are .gitignore'd (only the index.html shell is tracked), so the
# `web-builder` stage compiles them rather than relying on a pre-populated build
# context. It runs on Debian, where wasm-pack's auto-downloaded wasm-opt/
# wasm-bindgen are glibc binaries that Just Work; the server stays static musl.

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

# ---- builder: static musl build of both server-side binaries ----
FROM rust:alpine AS builder

RUN apk add --no-cache \
        musl-dev \
        protobuf-dev \
        binutils

WORKDIR /src
COPY . .
# Overlay the freshly built UI bundle so rust-embed bakes the real assets
# (not just the tracked index.html placeholder) into the server binary.
COPY --from=web-builder /src/crates/server/web-dist/ crates/server/web-dist/

# Two invocations sharing one target dir: the overlapping dependency graph
# compiles once and the bridge build reuses it. Kept as separate `-p` builds
# (not a single `-p server -p mumble-bridge`) so each binary keeps its own
# feature resolution — the bridge pulls rumble-desktop with default-features
# off (no audio C deps), which a unified resolve could perturb.
RUN cargo build --release -p server --bin server \
 && cargo build --release -p mumble-bridge --bin mumble-bridge \
 && strip target/release/server target/release/mumble-bridge

# ---- server image ----
FROM alpine:3 AS server

RUN apk add --no-cache ca-certificates tini \
 && adduser -D -H -u 10001 rumble

COPY --from=builder /src/target/release/server /usr/local/bin/server

USER rumble
# QUIC voice/control (UDP) + web admin (TCP). tini (PID 1) forwards SIGHUP to
# the server so a certbot deploy-hook can trigger a live cert reload.
EXPOSE 5000/udp 5001/tcp
ENTRYPOINT ["/sbin/tini", "--", "/usr/local/bin/server"]

# ---- bridge image ----
FROM alpine:3 AS bridge

RUN apk add --no-cache ca-certificates tini \
 && adduser -D -H -u 10001 rumble

COPY --from=builder /src/target/release/mumble-bridge /usr/local/bin/mumble-bridge

USER rumble
EXPOSE 64738/tcp 64738/udp
ENTRYPOINT ["/sbin/tini", "--", "/usr/local/bin/mumble-bridge"]
