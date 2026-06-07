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
use tokio::task::JoinHandle;
use tracing::{info, warn};

pub use auth::Sessions;

/// Shared application state for the web server, cloned into every handler.
#[derive(Clone)]
pub struct WebState {
    pub state: Arc<ServerState>,
    pub persistence: Option<Arc<Persistence>>,
    pub sessions: Arc<Sessions>,
    /// One-time bootstrap token, valid only while no sudo password is set.
    pub setup_token: Arc<String>,
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

/// Spawn the web admin server as a background task. Returns the join handle.
///
/// Generates a one-time setup token and, when the server still needs bootstrap
/// (no sudo password configured), prints it to the log so an operator can
/// complete first-run setup from the browser.
pub fn spawn(state: Arc<ServerState>, persistence: Option<Arc<Persistence>>, settings: WebSettings) -> JoinHandle<()> {
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

    let needs_bootstrap = persistence
        .as_ref()
        .map(|p| p.get_sudo_password().is_none())
        .unwrap_or(false);
    if needs_bootstrap {
        // Never log the token itself — it grants first-run admin. When the
        // operator pinned it via env they already have it; otherwise write it to
        // a 0600 file in the data dir and log only the path.
        if token_is_pinned {
            info!(
                "web admin: no sudo password set — first-run bootstrap is open. Using setup token from \
                 RUMBLE_WEB_SETUP_TOKEN. Complete setup at http://{}/",
                settings.bind
            );
        } else {
            let token_path = settings.data_dir.join("web-setup-token.txt");
            match write_token_file(&token_path, &setup_token) {
                Ok(()) => info!(
                    "web admin: no sudo password set — first-run bootstrap is open. Setup token written to {} \
                     (delete after setup). Complete setup at http://{}/",
                    token_path.display(),
                    settings.bind
                ),
                Err(e) => warn!(
                    "web admin: no sudo password set — first-run bootstrap is open, but failed to write setup token \
                     to {}: {e}. Set RUMBLE_WEB_SETUP_TOKEN to provide one explicitly.",
                    token_path.display()
                ),
            }
        }
    }

    let web_state = WebState {
        state,
        persistence,
        sessions: Arc::new(Sessions::new()),
        setup_token: Arc::new(setup_token),
    };

    let app = router(web_state, settings.assets_dir.clone());
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
        if let Err(e) = axum::serve(listener, app).await {
            warn!("web admin server error: {e}");
        }
    })
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
        .with_state(web_state);

    api.merge(assets::router(assets_dir))
}
