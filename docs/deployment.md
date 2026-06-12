# Deployment (docker-compose + host certbot)

This guide deploys the Rumble server, web admin, and Mumble bridge on a single
host using `docker-compose.yml`, with **certbot on the host** issuing and
renewing the TLS certificate.

The supporting files live in the repo root: `docker-compose.yml`, `.env.example`,
`docker/Dockerfile` (a shared multi-stage build — `target: server` and
`target: bridge` select each image), and `docker/certbot-deploy-hook.sh`.

## Architecture at a glance

- **rumble-server** terminates QUIC/TLS itself on UDP **5000** (public). QUIC
  can't be reverse-proxied like HTTP, so the cert lives in the server container.
- **web admin** is plain HTTP on **5001**, published to the host **loopback
  only**. You reach it over an SSH tunnel — it is never exposed publicly.
- **mumble-bridge** listens on **64738** (TCP+UDP) for Mumble clients and dials
  the rumble-server *by its public domain* over the internal docker network, so
  the Let's Encrypt cert validates by name and keeps working across renewals.
- **certbot** runs on the host. A deploy-hook copies each renewed cert into the
  container-readable `certs/` dir and signals the server to hot-reload it.

```
            Internet
   QUIC/UDP 5000 │           │ Mumble 64738 TCP/UDP
                 ▼           ▼
        ┌─────────────┐  ┌──────────────┐
        │ rumble-server│◄─┤ mumble-bridge│   (bridge dials ${RUMBLE_DOMAIN}:5000
        │  :5000 /udp  │  │  :64738      │    via the docker network alias)
        │  :5001 admin │  └──────────────┘
        └──────┬───────┘
   host loopback│ 127.0.0.1:5001  ◄── SSH tunnel for admin
```

## Prerequisites

- Docker + the compose plugin.
- A DNS A/AAAA record for your domain pointing at the host.
- certbot on the host with a cert issued for that domain, e.g.
  `sudo certbot certonly --standalone -d voice.example.com`
  (use whatever challenge plugin suits you; the server only needs the resulting
  PEM files).

## First-time setup

```bash
git clone <repo> /srv/rumble && cd /srv/rumble

cp .env.example .env
# edit .env: set RUMBLE_DOMAIN=voice.example.com

docker compose up -d --build
```

That's it for the containers: the image entrypoint creates and takes ownership
of the bind-mounted `certs/`, `data/`, and `bridge-data/` dirs itself (it
starts as root, fixes ownership for uid 10001, then drops privileges), and the
server generates a temporary self-signed cert so it can start before a real
one exists.

Then wire up the real cert (also handles all future renewals — see "TLS
renewal" below). The hook needs no editing; it finds the compose dir and the
domain on its own:

```bash
sudo ln -s /srv/rumble/docker/certbot-deploy-hook.sh \
    /etc/letsencrypt/renewal-hooks/deploy/rumble.sh
sudo /etc/letsencrypt/renewal-hooks/deploy/rumble.sh   # copy current cert into ./certs now
```

## First-run web-admin bootstrap

The web admin requires a one-time bootstrap to set the sudo password. The
server log prints a `FIRST-RUN SETUP` banner with these exact steps (check
`docker compose logs rumble-server`):

1. **Tunnel to the admin** from your workstation:
   ```bash
   ssh -L 5001:127.0.0.1:5001 you@host
   ```
   then open <http://127.0.0.1:5001/>.

2. **Get the setup token.** On the host:
   ```bash
   cat /srv/rumble/data/web-setup-token.txt
   ```
   (Alternatively, pin it ahead of time by setting `RUMBLE_WEB_SETUP_TOKEN` in
   the compose environment.)

3. **Complete bootstrap** in the browser: enter the token, choose a sudo
   password, and optionally paste your admin Ed25519 public key (base64) to be
   added to the `admin` group.

The token file is deleted automatically once bootstrap completes. After this,
log in at the same URL with the sudo password.

## Authorizing the Mumble bridge

The bridge has a stable identity in `bridge-data/bridge-identity.key` (generated
on first start). It must be authorized once as a controller.

1. Read its public key from the logs:
   ```bash
   docker compose logs mumble-bridge | grep "Bridge identity ready"
   ```
2. Authorize it — either through the **web admin** (add the key to the
   `controllers` group: Users → add by key), or from the host shell against the
   *running* server (applied live over its admin socket, no restart needed):
   ```bash
   docker compose exec rumble-server server add-controller '<base64-key>'
   ```

   The bridge retries with backoff and connects automatically once authorized.

## TLS renewal

The server **hot-reloads** its cert on `SIGHUP`, so renewals never drop
connections. The deploy-hook you installed above handles the whole cycle on each
certbot renewal:

1. certbot renews the cert under `/etc/letsencrypt/`.
2. `/etc/letsencrypt/renewal-hooks/deploy/rumble.sh` copies `fullchain.pem` and
   `privkey.pem` into `./certs/` (owned by uid 10001, so the non-root container
   can read them — this also resolves certbot's `live/ → archive/` symlinks).
3. It runs `docker compose kill -s HUP rumble-server`; tini forwards SIGHUP and
   the server re-reads the cert and applies it to new connections.

Verify it works without waiting 60 days:
```bash
sudo certbot renew --force-renewal
docker compose logs rumble-server | grep -i "certificate reloaded"
```

> A connection that is mid-handshake during the swap may need a single
> reconnect; established sessions are unaffected.

## Operations

- **Logs:** `docker compose logs -f rumble-server`
- **Update to a new build:** `git pull && docker compose up -d --build`
- **Backup:** the sled DB lives in `./data/rumble.db`. Stop the server for a
  consistent copy, or snapshot the volume.
- **Restart fallback** (if you prefer not to use SIGHUP): a deploy-hook running
  `docker compose restart rumble-server` also picks up a renewed cert, at the
  cost of dropping live connections.

### CLI subcommands and the admin socket

`server add-admin`, `server set-sudo-password`, `server add-controller`, and
`server set-participant-group` work whether or not the server is running:

- **Server running:** the running server exposes its full admin REST API on a
  unix socket at `<data_dir>/admin.sock` (mode `0600` — local file access *is*
  the credential). The subcommands detect it and apply the change live:
  ```bash
  docker compose exec rumble-server server set-sudo-password 'hunter2'
  ```
  Scripts can use the same socket directly; with the data dir bind-mounted it
  also works from the host without entering the container (as root — the
  socket is 0600 and owned by the container uid, and the server additionally
  verifies the peer's uid via `SO_PEERCRED`):
  ```bash
  sudo curl --unix-socket data/admin.sock http://localhost/api/state
  sudo curl --unix-socket data/admin.sock -X POST http://localhost/api/groups \
      -H 'Content-Type: application/json' -d '{"name":"vip","permissions":3}'
  ```
- **Server stopped:** the subcommands fall back to opening the sled database
  directly (it takes an exclusive lock, so the two paths can never collide).
  ```bash
  docker compose run --rm rumble-server set-sudo-password 'hunter2'
  ```

## Configuration reference

The server is configured entirely via environment variables in
`docker-compose.yml` (no config file needed):

| Variable | Compose value | Purpose |
|---|---|---|
| `RUMBLE_BIND` | `[::]:5000` | QUIC bind address (dual-stack). |
| `RUMBLE_DOMAIN` | from `.env` | Public domain; SAN for the self-signed fallback and the bridge's dial target. |
| `RUMBLE_CERT_DIR` | `/certs` | Where `fullchain.pem`/`privkey.pem` are read (and reloaded) from. |
| `RUMBLE_DATA_DIR` | `/data` | sled DB, web setup-token file, and the `admin.sock` admin socket. |
| `RUMBLE_WEB_ENABLED` | `1` | Web admin (on by default; set `0` to disable). |
| `RUMBLE_WEB_BIND` | `0.0.0.0:5001` | Admin bind *inside* the container (host exposure is restricted to loopback by the compose `ports` mapping). |
| `RUMBLE_WEB_SETUP_TOKEN` | _(unset)_ | Optionally pin the bootstrap token instead of reading the generated file. |
| `RUMBLE_LOG_LEVEL` | `info` | Log verbosity. |

Bridge flags (in the compose `command:`): `--rumble-addr ${RUMBLE_DOMAIN}:5000`,
`--identity-file /data/bridge-identity.key`, `--name Mumble`. For a purely
internal link you may instead pin the server cert with
`--rumble-cert-fingerprint <sha256>`; the network-alias approach above is
preferred because it survives renewals.
