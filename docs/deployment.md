# Deployment (docker-compose + host certbot)

This guide deploys the Rumble server, web admin, and Mumble bridge on a single
host using `docker-compose.yml`, with **certbot on the host** issuing and
renewing the TLS certificate.

The supporting files live in the repo root: `docker-compose.yml`, `.env.example`,
`docker/server.Dockerfile`, `docker/mumble-bridge.Dockerfile`, and
`docker/certbot-deploy-hook.sh`.

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

mkdir -p certs data bridge-data
# Containers run as uid 10001 (non-root); the bind mounts must be writable by it.
sudo chown -R 10001:10001 certs data bridge-data

# Seed the cert (also wires up renewals — see "TLS renewal" below).
sudo cp docker/certbot-deploy-hook.sh /etc/letsencrypt/renewal-hooks/deploy/rumble.sh
sudo sed -i 's/voice.example.com/YOUR_DOMAIN/; s#/srv/rumble#/srv/rumble#' \
    /etc/letsencrypt/renewal-hooks/deploy/rumble.sh
sudo chmod +x /etc/letsencrypt/renewal-hooks/deploy/rumble.sh
sudo /etc/letsencrypt/renewal-hooks/deploy/rumble.sh   # copy current cert into ./certs now

docker compose up -d --build
```

If `certs/` is still empty on first boot (you haven't issued a cert yet), the
server generates a temporary self-signed cert so it can start; run the
deploy-hook once after issuing the real cert to replace it.

## First-run web-admin bootstrap

The web admin requires a one-time bootstrap to set the sudo password.

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

4. **Delete the token file** once bootstrapped:
   ```bash
   rm /srv/rumble/data/web-setup-token.txt
   ```

After this, log in at the same URL with the sudo password.

## Authorizing the Mumble bridge

The bridge has a stable identity in `bridge-data/bridge-identity.key` (generated
on first start). It must be authorized once as a controller.

1. Read its public key from the logs:
   ```bash
   docker compose logs mumble-bridge | grep "Bridge identity ready"
   ```
2. Authorize it. Easiest is via the **web admin**: add that public key to the
   `controllers` group (Users → add by key). The bridge retries with backoff and
   connects automatically once authorized.

   The CLI subcommand `server add-controller <base64-key>` does the same, but see
   the caveat below — it needs the server stopped.

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

### Caveat: offline CLI subcommands

`server add-admin`, `server set-sudo-password`, `server add-controller`, and
`server set-participant-group` open the sled database directly, which takes an
**exclusive lock**. They cannot run while the server is up. Either:

- do the equivalent through the **web admin** while the server runs (preferred:
  bootstrap sets the sudo password and admin key; group membership covers
  controllers), or
- stop the server first:
  ```bash
  docker compose stop rumble-server
  docker compose run --rm rumble-server set-sudo-password 'hunter2'
  docker compose start rumble-server
  ```

## Configuration reference

The server is configured entirely via environment variables in
`docker-compose.yml` (no config file needed):

| Variable | Compose value | Purpose |
|---|---|---|
| `RUMBLE_BIND` | `[::]:5000` | QUIC bind address (dual-stack). |
| `RUMBLE_DOMAIN` | from `.env` | Public domain; SAN for the self-signed fallback and the bridge's dial target. |
| `RUMBLE_CERT_DIR` | `/certs` | Where `fullchain.pem`/`privkey.pem` are read (and reloaded) from. |
| `RUMBLE_DATA_DIR` | `/data` | sled DB + web setup-token file. |
| `RUMBLE_WEB_ENABLED` | `1` | Enable the web admin. |
| `RUMBLE_WEB_BIND` | `0.0.0.0:5001` | Admin bind *inside* the container (host exposure is restricted to loopback by the compose `ports` mapping). |
| `RUMBLE_WEB_SETUP_TOKEN` | _(unset)_ | Optionally pin the bootstrap token instead of reading the generated file. |
| `RUMBLE_LOG_LEVEL` | `info` | Log verbosity. |

Bridge flags (in the compose `command:`): `--rumble-addr ${RUMBLE_DOMAIN}:5000`,
`--identity-file /data/bridge-identity.key`, `--name Mumble`. For a purely
internal link you may instead pin the server cert with
`--rumble-cert-fingerprint <sha256>`; the network-alias approach above is
preferred because it survives renewals.
