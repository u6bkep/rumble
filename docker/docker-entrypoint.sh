#!/bin/sh
# Entrypoint for the rumble-server and mumble-bridge images.
#
# Containers run as the non-root `rumble` user (uid 10001), but bind-mounted
# volumes (/data, /certs) arrive owned by whoever created them on the host —
# historically forcing a manual `sudo chown -R 10001:10001` before first boot.
# Instead, start as root just long enough to take ownership of the mounts,
# then drop privileges and exec the real binary.
#
# If the container is started as a non-root user (compose `user:`, k8s
# securityContext), the chown is skipped entirely and we exec directly — the
# operator has opted into managing volume ownership themselves.
set -eu

RUMBLE_UID="${RUMBLE_UID:-10001}"
RUMBLE_GID="${RUMBLE_GID:-10001}"

if [ "$(id -u)" = "0" ]; then
    for dir in /data /certs; do
        [ -d "$dir" ] || continue
        # One traversal, chowning only offenders: cheap on every boot, and it
        # also repairs root-owned files dropped in later (e.g. certs copied by
        # hand instead of via the deploy-hook).
        find "$dir" ! -user "$RUMBLE_UID" -exec chown "$RUMBLE_UID:$RUMBLE_GID" {} + 2>/dev/null ||
            echo "[entrypoint] warning: could not take ownership of $dir" >&2
    done
    exec su-exec "$RUMBLE_UID:$RUMBLE_GID" "$@"
fi

exec "$@"
