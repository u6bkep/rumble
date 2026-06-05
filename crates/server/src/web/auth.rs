//! Web admin authentication: sudo-password login, opaque session cookies, and
//! first-run bootstrap.
//!
//! A valid session means full admin: the sudo password is already the server's
//! elevation gate, so an authenticated session bypasses per-room ACL checks the
//! same way an elevated superuser does over QUIC. Sessions are in-memory and
//! die with the process.

use super::{ApiErrorResponse, ApiResult, WebState};
use axum::{
    Json,
    extract::{FromRequestParts, State},
    http::{HeaderMap, StatusCode, header, request::Parts},
    response::{IntoResponse, Response},
};
use dashmap::DashMap;
use rumble_web_types::{BootstrapRequest, LoginRequest, OkMessage, SessionInfo};
use std::time::{Duration, Instant};

/// Cookie name for the admin session token.
const SESSION_COOKIE: &str = "rumble_admin_session";
/// Session lifetime.
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// In-memory session store: token → expiry instant.
pub struct Sessions {
    tokens: DashMap<String, Instant>,
}

impl Sessions {
    pub fn new() -> Self {
        Self { tokens: DashMap::new() }
    }

    /// Create a fresh session and return its token.
    pub fn create(&self) -> String {
        let token = generate_token();
        self.tokens.insert(token.clone(), Instant::now() + SESSION_TTL);
        token
    }

    /// Returns true if the token is present and unexpired (lazily evicting
    /// expired entries).
    pub fn validate(&self, token: &str) -> bool {
        match self.tokens.get(token).map(|e| *e.value()) {
            Some(expiry) if expiry > Instant::now() => true,
            Some(_) => {
                self.tokens.remove(token);
                false
            }
            None => false,
        }
    }

    pub fn remove(&self, token: &str) {
        self.tokens.remove(token);
    }
}

impl Default for Sessions {
    fn default() -> Self {
        Self::new()
    }
}

/// Generate a 256-bit random token, hex-encoded.
pub fn generate_token() -> String {
    let bytes: [u8; 32] = rand::random();
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Extract the session cookie value from request headers.
fn session_cookie(headers: &HeaderMap) -> Option<String> {
    let cookie_header = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(&format!("{SESSION_COOKIE}=")) {
            return Some(value.to_string());
        }
    }
    None
}

/// Extractor that succeeds only for requests carrying a live admin session.
/// Protected handlers take this as an argument; the request is rejected with
/// 401 otherwise.
pub struct Admin;

impl FromRequestParts<WebState> for Admin {
    type Rejection = ApiErrorResponse;

    async fn from_request_parts(parts: &mut Parts, state: &WebState) -> Result<Self, Self::Rejection> {
        let token = session_cookie(&parts.headers).ok_or_else(ApiErrorResponse::unauthorized)?;
        if state.sessions.validate(&token) {
            Ok(Admin)
        } else {
            Err(ApiErrorResponse::unauthorized())
        }
    }
}

/// Build the `Set-Cookie` header value for a new session token.
fn set_cookie(token: &str) -> String {
    format!(
        "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        SESSION_TTL.as_secs()
    )
}

/// `GET /api/session` — report auth + bootstrap status.
pub async fn session_info(State(st): State<WebState>, headers: HeaderMap) -> Json<SessionInfo> {
    let authenticated = session_cookie(&headers)
        .map(|t| st.sessions.validate(&t))
        .unwrap_or(false);
    let needs_bootstrap = st
        .persistence
        .as_ref()
        .map(|p| p.get_sudo_password().is_none())
        .unwrap_or(false);
    Json(SessionInfo {
        authenticated,
        needs_bootstrap,
    })
}

/// `POST /api/login` — verify the sudo password and set a session cookie.
pub async fn login(State(st): State<WebState>, Json(req): Json<LoginRequest>) -> Response {
    let Some(persist) = st.persistence.as_ref() else {
        return ApiErrorResponse::new(StatusCode::SERVICE_UNAVAILABLE, "Persistence not enabled").into_response();
    };
    let Some(hash) = persist.get_sudo_password() else {
        return ApiErrorResponse::bad_request("Sudo password not configured; bootstrap required").into_response();
    };
    match bcrypt::verify(&req.password, &hash) {
        Ok(true) => {
            let token = st.sessions.create();
            let mut response = Json(OkMessage {
                message: "Logged in".to_string(),
            })
            .into_response();
            if let Ok(value) = header::HeaderValue::from_str(&set_cookie(&token)) {
                response.headers_mut().insert(header::SET_COOKIE, value);
            }
            response
        }
        _ => ApiErrorResponse::unauthorized().into_response(),
    }
}

/// `POST /api/logout` — drop the caller's session.
pub async fn logout(State(st): State<WebState>, headers: HeaderMap) -> Json<OkMessage> {
    if let Some(token) = session_cookie(&headers) {
        st.sessions.remove(&token);
    }
    Json(OkMessage {
        message: "Logged out".to_string(),
    })
}

/// `POST /api/bootstrap` — first-run setup. Available only while no sudo
/// password is configured, and guarded by the one-time setup token. Sets the
/// sudo password and optionally seeds an admin key.
pub async fn bootstrap(State(st): State<WebState>, Json(req): Json<BootstrapRequest>) -> ApiResult<OkMessage> {
    let persist = st
        .persistence
        .as_ref()
        .ok_or_else(|| ApiErrorResponse::new(StatusCode::SERVICE_UNAVAILABLE, "Persistence not enabled"))?;

    if persist.get_sudo_password().is_some() {
        return Err(ApiErrorResponse::conflict("Already bootstrapped"));
    }
    if req.setup_token != *st.setup_token {
        return Err(ApiErrorResponse::unauthorized());
    }
    if req.sudo_password.is_empty() {
        return Err(ApiErrorResponse::bad_request("Sudo password cannot be empty"));
    }

    let hash = bcrypt::hash(&req.sudo_password, bcrypt::DEFAULT_COST)
        .map_err(|e| ApiErrorResponse::bad_request(format!("Failed to hash password: {e}")))?;
    persist
        .set_sudo_password(&hash)
        .map_err(|e| ApiErrorResponse::bad_request(format!("Failed to set sudo password: {e}")))?;

    if let Some(key_b64) = req.admin_public_key_b64.as_ref().filter(|s| !s.is_empty()) {
        let key_bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, key_b64)
            .map_err(|e| ApiErrorResponse::bad_request(format!("Invalid base64 public key: {e}")))?;
        if key_bytes.len() != 32 {
            return Err(ApiErrorResponse::bad_request("Public key must be 32 bytes"));
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&key_bytes);
        let _ = persist.ensure_default_groups();
        persist
            .add_user_to_group(&key, "admin")
            .map_err(|e| ApiErrorResponse::bad_request(format!("Failed to add admin: {e}")))?;
    }
    let _ = persist.flush();

    Ok(Json(OkMessage {
        message: "Bootstrap complete".to_string(),
    }))
}
