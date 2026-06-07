# syntax=docker/dockerfile:1.7

# Minimal Alpine build of the rumble server.
# Server is pure Rust (no system audio libs), so we statically link with musl
# and ship the binary on a tiny alpine base.

FROM rust:alpine AS builder

RUN apk add --no-cache \
        musl-dev \
        protobuf-dev \
        binutils

WORKDIR /src
COPY . .

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
