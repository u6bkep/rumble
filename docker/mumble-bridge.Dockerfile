# syntax=docker/dockerfile:1.7

# Minimal Alpine build of the Mumble<->Rumble bridge.
# The bridge consumes `rumble-desktop` with `default-features = false`, which
# disables the `audio`, `codec`, and `pulse` features. That strips cpal, opus,
# and libpulse from the dep tree — the bridge only needs quinn/rustls. Result:
# a server-side daemon binary with zero audio C deps.

FROM rust:alpine AS builder

RUN apk add --no-cache \
        musl-dev \
        protobuf-dev \
        binutils

WORKDIR /src
COPY . .

RUN cargo build --release -p mumble-bridge --bin mumble-bridge \
 && strip target/release/mumble-bridge

FROM alpine:3

RUN apk add --no-cache ca-certificates tini \
 && adduser -D -H -u 10001 rumble

COPY --from=builder /src/target/release/mumble-bridge /usr/local/bin/mumble-bridge

USER rumble
EXPOSE 64738/tcp 64738/udp
ENTRYPOINT ["/sbin/tini", "--", "/usr/local/bin/mumble-bridge"]
