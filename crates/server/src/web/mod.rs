//! HTTP web admin control-plane.
//!
//! An axum server, spawned as a background task inside the running server
//! process (so it shares `Arc<ServerState>` and `Arc<Persistence>`). It exposes
//! a JSON REST API for runtime administration — groups, per-room ACLs, rooms,
//! user moderation, registration — plus live monitoring and first-run
//! bootstrap. Every mutating endpoint authorizes via an admin session (the
//! sudo password) and then delegates to the shared [`crate::ops`] cores, so the
//! web path and the QUIC protocol path share one mutation/broadcast
//! implementation.
//!
//! The wasm admin UI ([`rumble-admin-web`]) is served as static assets by the
//! same server (see [`assets`]).

mod api;
mod assets;
mod auth;
mod monitor;

use crate::{config::WebSettings, persistence::Persistence, state::ServerState};
use axum::{
    Json, Router,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use rumble_web_types::ApiError;
use std::sync::Arc;
use tracing::{info, warn};

pub use auth::Sessions;

/// Shared application state for the web server, cloned into every handler.
#[derive(Clone)]
pub struct WebState {
    pub state: Arc<ServerState>,
    pub persistence: Arc<Persistence>,
    pub sessions: Arc<Sessions>,
    /// Per-IP login throttle (lockout after repeated failed sudo-password
    /// attempts), so the deliberately-expensive bcrypt verify can't be abused
    /// to brute-force the password or soak CPU.
    pub login_throttle: Arc<auth::LoginThrottle>,
    /// One-time bootstrap token, valid only while no sudo password is set.
    pub setup_token: Arc<String>,
    /// Where the bootstrap token was written (when not pinned via env), so a
    /// successful bootstrap can clean it up — the token is dead afterwards
    /// anyway, and leaving secrets lying around is operator homework we can do
    /// ourselves.
    pub setup_token_file: Arc<Option<std::path::PathBuf>>,
}

/// A JSON error response carrying an HTTP status and a user-facing message.
pub struct ApiErrorResponse {
    pub status: StatusCode,
    pub message: String,
}

impl ApiErrorResponse {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "Not authenticated")
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }
    pub fn too_many_requests(message: impl Into<String>) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for ApiErrorResponse {
    fn into_response(self) -> Response {
        (self.status, Json(ApiError { error: self.message })).into_response()
    }
}

/// Result alias for JSON API handlers.
pub type ApiResult<T> = Result<Json<T>, ApiErrorResponse>;

/// Write the one-time bootstrap token to `path`, owner-read/write only (`0600`)
/// on Unix so the secret is not exposed to other local users.
fn write_token_file(path: &std::path::Path, token: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        writeln!(file, "{token}")
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, format!("{token}\n"))
    }
}

/// Spawn the admin control-plane: the HTTP web admin (when `settings` is set)
/// and the local unix admin socket (when `admin_socket` is set), as background
/// tasks sharing one router and state.
///
/// Generates a one-time setup token and, when the server still needs bootstrap
/// (no sudo password configured), prints a setup banner so an operator can
/// complete first-run setup from the browser.
pub fn spawn(
    state: Arc<ServerState>,
    persistence: Arc<Persistence>,
    settings: Option<WebSettings>,
    admin_socket: Option<std::path::PathBuf>,
) {
    // Operators can pin the one-time bootstrap token (e.g. for automated
    // provisioning) via RUMBLE_WEB_SETUP_TOKEN; otherwise it is random.
    let token_is_pinned = std::env::var("RUMBLE_WEB_SETUP_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .is_some();
    let setup_token = std::env::var("RUMBLE_WEB_SETUP_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(auth::generate_token);

    let needs_bootstrap = persistence.get_sudo_password().is_none();
    // Never log the token itself — it grants first-run admin. When the
    // operator pinned it via env they already have it; otherwise write it to a
    // 0600 file in the data dir and log only the (absolute) path. Only the
    // browser flow needs the token at all, so skip it when HTTP is disabled.
    let mut setup_token_file = None;
    if needs_bootstrap
        && !token_is_pinned
        && let Some(settings) = &settings
    {
        let token_path = absolute(&settings.data_dir.join("web-setup-token.txt"));
        match write_token_file(&token_path, &setup_token) {
            Ok(()) => setup_token_file = Some(token_path),
            Err(e) => warn!(
                "web admin: failed to write setup token to {}: {e} — the data directory must be writable by the \
                 server's user. Set RUMBLE_WEB_SETUP_TOKEN to provide a token explicitly instead.",
                token_path.display()
            ),
        }
    }

    let web_state = WebState {
        state,
        persistence,
        sessions: Arc::new(Sessions::new()),
        login_throttle: Arc::new(auth::LoginThrottle::new()),
        setup_token: Arc::new(setup_token),
        setup_token_file: Arc::new(setup_token_file.clone()),
    };

    if let Some(settings) = settings {
        let app = router(web_state.clone(), settings.assets_dir.clone());
        let bind = settings.bind;
        tokio::spawn(async move {
            let listener = match tokio::net::TcpListener::bind(bind).await {
                Ok(l) => l,
                Err(e) => {
                    warn!("web admin: failed to bind {bind}: {e} — web admin disabled");
                    return;
                }
            };
            info!("web admin listening on http://{bind}");
            // The bootstrap banner goes out only once the listener is actually
            // up, so the URL it advertises is real.
            if needs_bootstrap {
                log_bootstrap_banner(bind, token_is_pinned, setup_token_file.as_deref());
            }
            // Serve with connection info so the login handler can read the peer
            // address for per-IP throttling.
            let make_service = app.into_make_service_with_connect_info::<std::net::SocketAddr>();
            if let Err(e) = axum::serve(listener, make_service).await {
                warn!("web admin server error: {e}");
            }
        });
    }

    #[cfg(unix)]
    if let Some(sock_path) = admin_socket {
        spawn_admin_socket(web_state, sock_path);
    }
    #[cfg(not(unix))]
    let _ = admin_socket;
}

/// Serve the same admin API over a unix socket in the data directory, for the
/// `server` CLI subcommands and local scripting (`curl --unix-socket`). Every
/// request over it is implicitly authorized ([`auth::LocalSocket`]): the socket
/// is `0600`, so reaching it at all requires owning the server's files.
#[cfg(unix)]
fn spawn_admin_socket(web_state: WebState, sock_path: std::path::PathBuf) {
    // No assets and the LocalSocket marker on every request.
    let app = router(web_state, None).layer(axum::Extension(auth::LocalSocket));
    tokio::spawn(async move {
        // A pre-existing socket file is necessarily stale: we already hold the
        // database's exclusive lock, so no other live server owns this data dir.
        if sock_path.exists()
            && let Err(e) = std::fs::remove_file(&sock_path)
        {
            warn!(
                "admin socket: failed to remove stale socket {}: {e} — admin socket disabled",
                sock_path.display()
            );
            return;
        }
        let listener = match tokio::net::UnixListener::bind(&sock_path) {
            Ok(l) => l,
            Err(e) => {
                warn!(
                    "admin socket: failed to bind {}: {e} — admin socket disabled",
                    sock_path.display()
                );
                return;
            }
        };
        // Owner-only: connecting to this socket is full admin.
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600)) {
            warn!(
                "admin socket: failed to set 0600 on {}: {e} — admin socket disabled",
                sock_path.display()
            );
            return;
        }
        info!("admin socket listening at {}", sock_path.display());
        if let Err(e) = axum::serve(listener, app.into_make_service()).await {
            warn!("admin socket server error: {e}");
        }
    });
}

/// Resolve a possibly-relative path against the cwd so logs and error messages
/// name the real location.
fn absolute(path: &std::path::Path) -> std::path::PathBuf {
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Print the first-run setup banner: one copy-pasteable block with everything
/// the operator needs (where the token is, where to paste it). Deliberately
/// loud — until bootstrap completes the server has no admin at all.
fn log_bootstrap_banner(bind: std::net::SocketAddr, token_is_pinned: bool, token_file: Option<&std::path::Path>) {
    // An unspecified bind (0.0.0.0 / [::]) is not a dialable URL; show the
    // loopback equivalent, which always works from the server's own host.
    let url_host = if bind.ip().is_unspecified() {
        "127.0.0.1".to_string()
    } else {
        bind.ip().to_string()
    };
    let token_line = if token_is_pinned {
        "2. Use the setup token you pinned via RUMBLE_WEB_SETUP_TOKEN.".to_string()
    } else if let Some(path) = token_file {
        format!(
            "2. Read the one-time setup token:  cat {}\n   (the file is deleted automatically once setup completes)",
            path.display()
        )
    } else {
        "2. No setup token is available (writing it failed — see the warning above).".to_string()
    };
    info!(
        "\n==================== FIRST-RUN SETUP ====================\n\
         No sudo password is set — this server has no admin yet.\n\
         1. Open the web admin:  http://{url_host}:{port}/\n\
         {token_line}\n\
         3. Enter the token, choose a sudo password, and optionally\n   \
         paste your Ed25519 public key to become admin.\n\
         (Remote host? Tunnel first:  ssh -L {port}:127.0.0.1:{port} <host>)\n\
         =========================================================",
        port = bind.port(),
    );
}

/// Build the axum router for the web admin API + static UI.
fn router(web_state: WebState, assets_dir: Option<std::path::PathBuf>) -> Router {
    let api = Router::new()
        // --- auth & bootstrap (no session required) ---
        .route("/api/session", get(auth::session_info))
        .route("/api/login", post(auth::login))
        .route("/api/logout", post(auth::logout))
        .route("/api/bootstrap", post(auth::bootstrap))
        // --- monitoring ---
        .route("/api/state", get(monitor::state_snapshot))
        // --- groups ---
        .route("/api/groups", get(monitor::list_groups).post(api::create_group))
        .route(
            "/api/groups/{name}",
            axum::routing::patch(api::modify_group).delete(api::delete_group),
        )
        .route("/api/groups/{name}/permissions", post(api::toggle_group_permission))
        // --- rooms ---
        .route("/api/rooms", get(monitor::list_rooms).post(api::create_room))
        .route("/api/rooms/{uuid}", axum::routing::delete(api::delete_room))
        .route("/api/rooms/{uuid}/acl", put(api::set_room_acl))
        // --- user moderation & registration ---
        .route("/api/users/{id}/kick", post(api::kick_user))
        .route("/api/users/{id}/ban", post(api::ban_user))
        .route(
            "/api/users/{id}/register",
            post(api::register_user).delete(api::unregister_user),
        )
        .route("/api/users/{id}/groups", post(api::set_user_group))
        .route(
            "/api/registered-users/{key}/groups",
            post(api::set_registered_user_group),
        )
        // --- server administration ---
        .route("/api/sudo-password", post(api::set_sudo_password))
        .route("/api/controllers", post(api::add_controller))
        .route(
            "/api/controllers/{key}/participant-group",
            put(api::set_participant_group),
        )
        // Compress JSON API responses on the fly (gzip). Static assets are
        // served precompressed (see `assets`), so this layer only meaningfully
        // applies to the dynamic `/api/*` bodies — tower-http skips responses
        // that already carry a `Content-Encoding`.
        .layer(tower_http::compression::CompressionLayer::new().gzip(true))
        .with_state(web_state);

    api.merge(assets::router(assets_dir))
}
