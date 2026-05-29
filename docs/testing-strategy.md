# Testing Strategy

How rumble is tested today. This replaces the old `docs/test-harness.md` (which
described a GUI screenshot harness that no longer exists). There are two
substantive test pillars on top of `cargo test`:

1. **rumble-aetna bundle/lint pipeline** — a headless renderer that drives the
   real GUI `App` against a mock backend and dumps per-scene artifacts
   (`crates/rumble-aetna/src/bin/dump_bundles.rs`).
2. **Server integration tests** — spin up a real QUIC server process plus a real
   client and assert on observed state/audio (`crates/server/tests/`).

Baseline checks (`cargo test`, `cargo +nightly fmt`, clippy via `scripts/ci.sh`)
back both pillars.

---

## Baseline checks

- `cargo test` — runs unit + integration tests across the workspace. The server
  integration tests (below) are part of this.
- `cargo +nightly fmt` — required before committing. `rustfmt.toml` sets
  `imports_granularity = "Crate"`; CI rejects unformatted code.
- `scripts/ci.sh` — local pre-flight that mirrors `.github/workflows/ci.yml`. It
  runs `cargo +nightly fmt --all -- --check` then
  `cargo clippy --workspace --all-targets --locked -- -D warnings`. Subcommands:
  `scripts/ci.sh fmt` and `scripts/ci.sh clippy` run a single check. Note: the
  CI job runs **only fmt + clippy** — it does not run `cargo test`, so run the
  test suite yourself before relying on it.

---

## Pillar 1 — rumble-aetna bundle/lint pipeline

`dump_bundles` (`crates/rumble-aetna/src/bin/dump_bundles.rs`) is the GUI's
regression harness. It renders every canonical UI scene **without a GPU or
window**, so layout/visual regressions are visible in CI-friendly text + SVG.

### What it does

For each `Scene` it:

1. Builds a `MockBackend` (`dump_bundles.rs`) that returns a canned
   `rumble_client::State` from `state()` and discards every `Command` from
   `send()`. It also serves canned `meter()`, `stats()`, and `transfers()`
   snapshots. It implements the real `rumble_aetna::backend::UiBackend` trait, so
   the app sees exactly the surface the production client exposes.
2. Constructs a real `RumbleApp::new(...)` over that backend.
3. Drives any per-scene local UI state through the **real `App` event path** via
   `*_for_test` helpers in `drive_setup` (e.g. `app.open_settings_for_test`,
   `app.open_lightbox_for_test`). The rendered scene is what a user would see
   after the same interaction — there is no fixture-only shortcut that
   production code can drift away from.
4. Calls `app.build(&cx)` to produce the element tree, then
   `render_bundle(&mut tree, viewport)` (viewport is fixed at 1280×800 to match
   the real window) and `write_bundle(...)` from the aetna-core prelude.

Portal init is disabled (`RUMBLE_DISABLE_PORTAL=1`, set in `main` before any
thread spawns) so repeated `RumbleApp` construction doesn't hang on the XDG
GlobalShortcuts portal. Identity/`SettingsStore` are pointed at a temp scratch
dir.

### Artifacts

Output lands in `crates/rumble-aetna/out/` (gitignored), one set per scene named
`rumble_<scene>.<ext>`:

| File                  | Contents                                                        |
|-----------------------|-----------------------------------------------------------------|
| `.svg`                | Rendered scene. The SVG fallback renders the **same draw-op stream** the wgpu Runner would, so layout regressions show up faithfully without a device. |
| `.tree.txt`           | The element tree.                                               |
| `.draw_ops.txt`       | The flat draw-op list.                                          |
| `.lint.txt`           | Lint findings (see below).                                      |
| `.shader_manifest.txt`| Shader manifest for the scene.                                  |

### Lint findings

`bundle.lint.findings` flags things like raw (non-theme) colors, overflow, weak
focus indication, and scrollbar overlap. They are written to `<scene>.lint.txt`
and also echoed to stderr at the end of a run. **Review `lint.txt` before
declaring a UI change done.**

### Commands

```bash
cargo run -p rumble-aetna --bin dump_bundles                       # dump every scene
cargo run -p rumble-aetna --bin dump_bundles -- connected cert_pending  # specific scenes by slug
```

Scene names are matched case-insensitively against each `Scene::slug()` (e.g.
`connected`, `cert_pending`, `settings_admin`, `image_lightbox_zoomed`). An
unknown name panics with `unknown scene \`...\``.

> Iterating against `vendor/aetna`: run through `scripts/aetna-local.sh` instead
> of bare `cargo` so the local aetna overlay is applied (see project CLAUDE.md).

### Adding a scene

In `dump_bundles.rs`:

1. Add a variant to the `Scene` enum (with a doc comment describing the state it
   exercises).
2. Add it to the `Scene::ALL` slice and give it a slug in `Scene::slug()`.
3. Return the canned `State` from `Scene::build_state()` (reuse helpers like
   `connected_state()`, `admin_state()`, `room_acl_state()` where possible). If
   the scene needs meter/stats/transfer data, extend `build_meter()`,
   `build_stats()`, and/or `build_transfers()`.
4. If the scene needs interactive UI state (an open modal/dropdown/etc.), drive
   it through the real event path in `drive_setup()` using the app's
   `*_for_test` helpers. Add to `keeps_first_run()` if the scene is itself the
   first-run/unlock flow (otherwise the wizard is suppressed via
   `suppress_first_run_for_test()`).

---

## Pillar 2 — Server integration tests

Located in `crates/server/tests/`:

- `backend_handle_integration.rs` — exercises `rumble_client::handle::BackendHandle`
  (over `rumble_desktop::NativePlatform`) against a real server: connect/
  disconnect, room CRUD, multi-client visibility, and the full
  transmission-mode (PTT/Continuous/mute) state machine.
- `voice_relay.rs` — end-to-end voice: two backends on a `TestPlatform` (real
  Quinn transport + real Opus codec, but `MockAudioBackend` instead of cpal).
  Injects a sine into one client's mock mic and asserts the other client's mock
  speaker emits non-trivial energy; also covers PTT gating and live input
  reconfiguration.
- `backend_integration.rs` — server-side behavior driven by a minimal hand-rolled
  QUIC test client (raw `quinn::Endpoint` + protobuf framing from
  `rumble_protocol`), rather than through `BackendHandle`.

### Shared harness shape

Each file defines its own `ServerGuard` / `start_server(port)` (they are copies,
not a shared crate). `start_server`:

- Creates a `TempDir` with `certs/` and `data/` subdirs.
- Spawns the **real server binary** (`env!("CARGO_BIN_EXE_server")`) with
  `RUMBLE_NO_CONFIG=1`, `RUMBLE_PORT`, `RUMBLE_CERT_DIR`, `RUMBLE_DATA_DIR` env
  vars, piping stdout/stderr to the test log prefixed with the port.
- Waits (up to 5s) for the server to write `certs/fullchain.pem`, which it then
  hands to the client as the trusted cert.
- `ServerGuard::Drop` kills + reaps the child and drops the temp dir.

```
TempDir/                 spawn server binary        client BackendHandle
  certs/  <── fullchain.pem ──> RUMBLE_CERT_DIR ──> ConnectConfig::with_cert
  data/                         RUMBLE_DATA_DIR
                                RUMBLE_PORT=<unique>
```

### Port isolation

Each file has a `static PORT_COUNTER: AtomicU16` (seeded 57000 in
`backend_handle_integration.rs`, 58000 in `voice_relay.rs`) and
`next_test_port()` does `fetch_add(1)`. Every test gets a unique port so server
instances don't collide when tests run in parallel.

### Test client + signer

- Tests build a `BackendHandle` with `ConnectConfig::new().with_cert(...)` and a
  `TestSigner` — an in-memory `ed25519_dalek::SigningKey` wrapped in the
  `rumble_client_traits::KeySigning` trait. The handle carries the matching
  public key; auth uses anonymous (unregistered) identities.
- `wait_for(handle, timeout, condition)` polls `handle.state()` every 25–50ms
  until the closure returns true or the timeout elapses. Assertions are written
  against this poll result, not against fixed sleeps.

### Concurrency / flakiness

These tests start real OS processes and real QUIC connections and lean on
polling deadlines (typically 2–5s). They pass reliably **in isolation**, but
running the full suite concurrently can surface timing flakes under load (slow
connect, slow state propagation) that trip a `wait_for` deadline. If a server
test fails, re-run it on its own before treating the failure as a real
regression.

Some tests are gated out of the default run:

- `test_backend_connection_lost_on_server_shutdown` is `#[ignore]`d — it waits
  on the ~40s QUIC idle timeout. Run with `cargo test -- --ignored`.
