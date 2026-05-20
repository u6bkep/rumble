use std::time::Duration;

use aetna_core::Rect;
use aetna_winit_wgpu::HostConfig;
use rumble_aetna::{Identity, NativeUiBackend, RumbleApp};
use rumble_client::{ConnectConfig, handle::BackendHandle};
use rumble_desktop_shell::SettingsStore;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,rumble_aetna=info,rumble_client=info")),
        )
        .init();

    let config_dir = config_dir();
    let identity = Identity::load(config_dir.clone())?;

    // Share the desktop-shell settings store with rumble-egui /
    // rumble-next so a cert the user already approved in one client
    // is honored here too.
    let settings = SettingsStore::load_from_path(Some(config_dir.join("desktop-shell.json")));
    let connect_config = build_connect_config(&settings);

    // Aetna's host doesn't expose an event-loop wakeup hook today, so
    // we use the host's `redraw_interval` to poll the backend's
    // `State` ~30fps. When state actually changes the next frame
    // picks it up; idle frames are cheap (no input events, animations
    // settle, GPU stays idle).
    let signer = identity.signer();
    let backend = BackendHandle::<rumble_desktop::NativePlatform>::with_config(|| {}, connect_config, signer);

    // Optional Unix-socket RPC server for external process control —
    // notably the "system DE shortcut → rumble-ctl command" path on
    // Wayland. Opt-in via `--rpc-server [path]`; if no path is given
    // we use `$XDG_RUNTIME_DIR/rumble/aetna.sock` so we don't clobber
    // rumble-egui's default socket when both are installed. Held in a
    // local until `run_host_app_with_config` returns so the listener
    // outlives the GUI.
    #[cfg(unix)]
    let _rpc_server = match rpc_server_socket_path() {
        Some(path) => match backend.start_rpc_server(path) {
            Ok(server) => {
                tracing::info!("RPC server started");
                Some(server)
            }
            Err(e) => {
                tracing::warn!("RPC server failed to start: {e}");
                None
            }
        },
        None => None,
    };

    let backend = NativeUiBackend::new(backend);

    // Owned tokio runtime for ssh-agent ops fired from the wizard. Kept
    // separate from the BackendHandle's internal runtime so the App can
    // freely block_on the wizard's join handles without re-entering the
    // backend's reactor.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()?;

    let app = RumbleApp::new(backend, identity, settings, runtime);
    let viewport = Rect::new(0.0, 0.0, 1280.0, 800.0);
    // Mailbox present so window content tracks the cursor during
    // interactive resize on Wayland/Mesa instead of trailing in slow
    // motion as the swapchain queue drains at vsync. The 33ms redraw
    // cadence still bounds idle work; animation frames render at GPU
    // speed during transitions, which is what we want here.
    let host_config = HostConfig::default()
        .with_redraw_interval(Duration::from_millis(33))
        .with_low_latency_present(true)
        // Reverse-DNS app id (matches our `ProjectDirs` qualifier/org/app)
        // so the Wayland `xdg_toplevel.app_id` / X11 `WM_CLASS` we present
        // to the compositor and the XDG GlobalShortcuts portal is a stable
        // "com.rumble.Rumble" instead of a generic `surface-transient`
        // placeholder. This is what the system hotkeys UI groups us under.
        .with_app_id("com.rumble.Rumble");
    aetna_winit_wgpu::run_host_app_with_config("Rumble", viewport, app, host_config)?;
    Ok(())
}

fn build_connect_config(settings: &SettingsStore) -> ConnectConfig {
    let mut config = ConnectConfig::new();
    // Trust the repo dev cert when running from the project root so
    // `cargo run -p rumble-aetna` against `cargo run --bin server`
    // works without an approval round-trip.
    for candidate in ["dev-certs/server-cert.der", "certs/fullchain.pem"] {
        if std::path::Path::new(candidate).exists() {
            config = config.with_cert(candidate);
        }
    }
    if let Ok(cert_path) = std::env::var("RUMBLE_SERVER_CERT_PATH") {
        config = config.with_cert(cert_path);
    }
    for entry in &settings.settings().accepted_certificates {
        match entry.der_bytes() {
            Some(der) => config.accepted_certs.push(der),
            None => tracing::warn!(
                "settings: accepted cert for {} has invalid base64 — ignored",
                entry.server_name
            ),
        }
    }
    if let Some(dir) = settings.settings().file_transfer.download_dir.clone() {
        config = config.with_download_dir(dir);
    }
    config
}

/// Parse `--rpc-server [path]` out of the process arguments. Returns
/// `Some(path)` when the flag is present (using the explicit path or
/// the aetna default), or `None` when the user hasn't opted in.
///
/// Default socket: `$XDG_RUNTIME_DIR/rumble/aetna.sock` (falling back to
/// `/tmp/rumble/aetna.sock`). Distinct from rumble-egui's `rpc.sock` so
/// both clients can run side-by-side without one clobbering the other.
#[cfg(unix)]
fn rpc_server_socket_path() -> Option<std::path::PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--rpc-server" {
            // Next token is an optional explicit socket path.
            return match args.next() {
                Some(p) if !p.starts_with("--") => Some(std::path::PathBuf::from(p)),
                _ => Some(default_aetna_rpc_socket()),
            };
        }
        if let Some(p) = arg.strip_prefix("--rpc-server=") {
            return Some(std::path::PathBuf::from(p));
        }
    }
    None
}

#[cfg(unix)]
fn default_aetna_rpc_socket() -> std::path::PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));
    base.join("rumble").join("aetna.sock")
}

fn config_dir() -> std::path::PathBuf {
    if let Ok(override_dir) = std::env::var("RUMBLE_AETNA_CONFIG_DIR") {
        return std::path::PathBuf::from(override_dir);
    }
    if let Some(dirs) = directories::ProjectDirs::from("com", "rumble", "Rumble") {
        dirs.config_dir().to_path_buf()
    } else {
        std::path::PathBuf::from("./config")
    }
}
