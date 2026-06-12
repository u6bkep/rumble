#!/bin/sh
# certbot deploy-hook for the Rumble docker-compose deployment.
#
# Runs on the HOST (as root, the way certbot invokes hooks) after a successful
# issuance/renewal. It copies the renewed cert into the compose `certs/` bind
# mount with ownership the non-root container (uid 10001) can read, then
# signals the server to hot-reload the cert without dropping connections.
#
# Nothing to edit: the compose directory defaults to this script's repo
# checkout (its own grandparent directory) and the domain is read from the
# .env there. Install by symlinking into certbot's hook dir:
#
#   sudo ln -s /srv/rumble/docker/certbot-deploy-hook.sh \
#       /etc/letsencrypt/renewal-hooks/deploy/rumble.sh
#   sudo /etc/letsencrypt/renewal-hooks/deploy/rumble.sh   # seed certs/ now
#
# An explicit compose dir can be passed as $1 if the checkout is elsewhere.
set -eu

# Compose dir: $1, or the checkout containing this script (resolved through
# the symlink certbot invokes).
COMPOSE_DIR="${1:-$(dirname "$(dirname "$(readlink -f "$0")")")}"
CONTAINER_UID=10001               # matches the Dockerfile's `rumble` user

# Domain: certbot provides $RENEWED_LINEAGE during renewals; for a manual
# first run fall back to RUMBLE_DOMAIN from the deployment's .env.
if [ -z "${RENEWED_LINEAGE:-}" ]; then
    DOMAIN="$(sed -n 's/^RUMBLE_DOMAIN=//p' "$COMPOSE_DIR/.env" | tail -n1)"
    [ -n "$DOMAIN" ] || {
        echo "RUMBLE_DOMAIN not set in $COMPOSE_DIR/.env and \$RENEWED_LINEAGE unset" >&2
        exit 1
    }
fi
SRC="${RENEWED_LINEAGE:-/etc/letsencrypt/live/$DOMAIN}"
DEST="$COMPOSE_DIR/certs"

mkdir -p "$DEST"
# Copy the real files (resolving live/ -> archive/ symlinks) with perms the
# container user can read. privkey stays owner-only.
install -o "$CONTAINER_UID" -g "$CONTAINER_UID" -m 0644 "$SRC/fullchain.pem" "$DEST/fullchain.pem"
install -o "$CONTAINER_UID" -g "$CONTAINER_UID" -m 0600 "$SRC/privkey.pem" "$DEST/privkey.pem"

# Hot-reload: SIGHUP is forwarded by tini to the server, which re-reads the
# cert and applies it to new connections. Existing connections are unaffected.
cd "$COMPOSE_DIR"
docker compose kill -s HUP rumble-server
