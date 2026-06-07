#!/bin/sh
# certbot deploy-hook for the Rumble docker-compose deployment.
#
# Runs on the HOST (as root, the way certbot invokes hooks) after a successful
# issuance/renewal. It copies the renewed cert into the compose `certs/` bind
# mount with ownership the non-root container (uid 10001) can read, then signals
# the server to hot-reload the cert without dropping connections.
#
# Install once, e.g.:
#   sudo cp docker/certbot-deploy-hook.sh /etc/letsencrypt/renewal-hooks/deploy/rumble.sh
#   sudo chmod +x /etc/letsencrypt/renewal-hooks/deploy/rumble.sh
#   # edit DOMAIN and COMPOSE_DIR below
#
# certbot sets $RENEWED_LINEAGE to the live/<domain> dir during renewals; we
# fall back to DOMAIN for a manual first run.
set -eu

DOMAIN="voice.example.com"        # must match RUMBLE_DOMAIN in .env
COMPOSE_DIR="/srv/rumble"         # directory containing docker-compose.yml
CONTAINER_UID=10001               # matches the Dockerfile's `rumble` user

SRC="${RENEWED_LINEAGE:-/etc/letsencrypt/live/$DOMAIN}"
DEST="$COMPOSE_DIR/certs"

mkdir -p "$DEST"
# Copy the real files (resolving live/ -> archive/ symlinks) with perms the
# container user can read. privkey stays owner-only.
install -o "$CONTAINER_UID" -g "$CONTAINER_UID" -m 0644 "$SRC/fullchain.pem" "$DEST/fullchain.pem"
install -o "$CONTAINER_UID" -g "$CONTAINER_UID" -m 0600 "$SRC/privkey.pem" "$DEST/privkey.pem"

# Hot-reload: SIGHUP is forwarded by tini to the server, which re-reads the cert
# and applies it to new connections. Existing connections are unaffected.
cd "$COMPOSE_DIR"
docker compose kill -s HUP rumble-server
