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
    extract::{ConnectInfo, FromRequestParts, State},
    http::{HeaderMap, header, request::Parts},
    response::{IntoResponse, Response},
};
use dashmap::DashMap;
use rumble_web_types::{BootstrapRequest, LoginRequest, OkMessage, SessionInfo};
use std::{
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

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

/// Consecutive failed logins from one IP before it is locked out.
const MAX_LOGIN_FAILURES: u32 = 5;
/// How long an IP is locked out once it trips [`MAX_LOGIN_FAILURES`].
const LOGIN_LOCKOUT: Duration = Duration::from_secs(60);

struct FailureState {
    failures: u32,
    locked_until: Option<Instant>,
}

/// Per-IP login throttle. After [`MAX_LOGIN_FAILURES`] consecutive failures an
/// IP is locked out for [`LOGIN_LOCKOUT`], bounding how fast the sudo password
/// can be guessed and how much bcrypt work an attacker can force. A successful
/// login clears the IP's counter.
///
/// Note: behind a reverse proxy the peer address is the proxy, so all clients
/// share one bucket — operators terminating TLS at a proxy should rate-limit
/// there too. For a direct bind (the documented deployment) this keys on the
/// real client.
pub struct LoginThrottle {
    by_ip: DashMap<IpAddr, FailureState>,
}

impl LoginThrottle {
    pub fn new() -> Self {
        Self { by_ip: DashMap::new() }
    }

    /// If `ip` is currently locked out, return the remaining lockout duration;
    /// otherwise `None` (the attempt may proceed).
    pub fn check(&self, ip: IpAddr) -> Option<Duration> {
        let entry = self.by_ip.get(&ip)?;
        let until = entry.locked_until?;
        until.checked_duration_since(Instant::now())
    }

    /// Record a failed attempt, locking the IP out once it reaches the limit.
    pub fn record_failure(&self, ip: IpAddr) {
        let mut entry = self.by_ip.entry(ip).or_insert(FailureState {
            failures: 0,
            locked_until: None,
        });
        // A previously-expired lockout resets the window before we count again.
        if let Some(until) = entry.locked_until
            && until <= Instant::now()
        {
            entry.failures = 0;
            entry.locked_until = None;
        }
        entry.failures += 1;
        if entry.failures >= MAX_LOGIN_FAILURES {
            entry.locked_until = Some(Instant::now() + LOGIN_LOCKOUT);
        }
    }

    /// Clear an IP's failure record after a successful login.
    pub fn record_success(&self, ip: IpAddr) {
        self.by_ip.remove(&ip);
    }
}

impl Default for LoginThrottle {
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

/// Request-extension marker stamped onto every request arriving over the local
/// admin unix socket. The socket file is `0600` in the server's data directory,
/// so being able to connect at all already proves the caller owns the server's
/// files — filesystem permissions are the credential, no session needed.
#[derive(Clone, Copy)]
pub struct LocalSocket;

/// Extractor that succeeds only for requests carrying a live admin session, or
/// arriving over the local admin socket ([`LocalSocket`]). Protected handlers
/// take this as an argument; the request is rejected with 401 otherwise.
pub struct Admin;

impl FromRequestParts<WebState> for Admin {
    type Rejection = ApiErrorResponse;

    async fn from_request_parts(parts: &mut Parts, state: &WebState) -> Result<Self, Self::Rejection> {
        if parts.extensions.get::<LocalSocket>().is_some() {
            return Ok(Admin);
        }
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
    let needs_bootstrap = st.persistence.get_sudo_password().is_none();
    Json(SessionInfo {
        authenticated,
        needs_bootstrap,
    })
}

/// `POST /api/login` — verify the sudo password and set a session cookie.
///
/// bcrypt verification (cost 12, ~100–300 ms) runs on the blocking thread pool
/// via `spawn_blocking`, so it never stalls the async workers that also relay
/// voice. Per-IP throttling locks out an address after repeated failures so the
/// expensive verify can't be used to brute-force the password or soak CPU.
pub async fn login(
    State(st): State<WebState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(req): Json<LoginRequest>,
) -> Response {
    let ip = peer.ip();
    if let Some(retry_after) = st.login_throttle.check(ip) {
        return ApiErrorResponse::too_many_requests(format!(
            "Too many failed login attempts; try again in {}s",
            retry_after.as_secs() + 1
        ))
        .into_response();
    }

    let persist = &st.persistence;
    let Some(hash) = persist.get_sudo_password() else {
        return ApiErrorResponse::bad_request("Sudo password not configured; bootstrap required").into_response();
    };

    // bcrypt is CPU-bound and deliberately slow — run it off the async workers.
    let password = req.password;
    let verified = match tokio::task::spawn_blocking(move || bcrypt::verify(&password, &hash)).await {
        Ok(Ok(ok)) => ok,
        // A bcrypt error (malformed stored hash) or a join failure is a server
        // fault, not a credential verdict — don't count it against the IP.
        Ok(Err(e)) => return ApiErrorResponse::internal(format!("Password verification failed: {e}")).into_response(),
        Err(e) => return ApiErrorResponse::internal(format!("Password verification task failed: {e}")).into_response(),
    };

    if verified {
        st.login_throttle.record_success(ip);
        let token = st.sessions.create();
        let mut response = Json(OkMessage {
            message: "Logged in".to_string(),
        })
        .into_response();
        if let Ok(value) = header::HeaderValue::from_str(&set_cookie(&token)) {
            response.headers_mut().insert(header::SET_COOKIE, value);
        }
        response
    } else {
        st.login_throttle.record_failure(ip);
        ApiErrorResponse::unauthorized().into_response()
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
    let persist = &st.persistence;

    if persist.get_sudo_password().is_some() {
        return Err(ApiErrorResponse::conflict("Already bootstrapped"));
    }
    if req.setup_token != *st.setup_token {
        return Err(ApiErrorResponse::unauthorized());
    }
    if req.sudo_password.is_empty() {
        return Err(ApiErrorResponse::bad_request("Sudo password cannot be empty"));
    }

    // bcrypt hashing is CPU-bound and slow — keep it off the async workers.
    let password = req.sudo_password.clone();
    let hash = tokio::task::spawn_blocking(move || bcrypt::hash(&password, bcrypt::DEFAULT_COST))
        .await
        .map_err(|e| ApiErrorResponse::internal(format!("Hashing task failed: {e}")))?
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

    // The token is single-use and now spent — remove the file it was published
    // in so no stale secret lingers on disk.
    if let Some(path) = st.setup_token_file.as_ref() {
        match std::fs::remove_file(path) {
            Ok(()) => tracing::info!("bootstrap complete — deleted setup token file {}", path.display()),
            Err(e) => tracing::warn!(
                "bootstrap complete, but failed to delete setup token file {}: {e} — delete it manually (the token is \
                 no longer valid)",
                path.display()
            ),
        }
    } else {
        tracing::info!("bootstrap complete");
    }

    Ok(Json(OkMessage {
        message: "Bootstrap complete".to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([127, 0, 0, n])
    }

    #[test]
    fn locks_out_after_max_failures() {
        let throttle = LoginThrottle::new();
        let addr = ip(1);
        for _ in 0..MAX_LOGIN_FAILURES - 1 {
            throttle.record_failure(addr);
            assert!(throttle.check(addr).is_none(), "not locked before the limit");
        }
        throttle.record_failure(addr); // reaches the limit
        assert!(throttle.check(addr).is_some(), "locked out at the limit");
    }

    #[test]
    fn success_clears_failures() {
        let throttle = LoginThrottle::new();
        let addr = ip(2);
        for _ in 0..MAX_LOGIN_FAILURES {
            throttle.record_failure(addr);
        }
        assert!(throttle.check(addr).is_some(), "locked after repeated failures");
        throttle.record_success(addr);
        assert!(throttle.check(addr).is_none(), "a successful login resets the lockout");
    }

    #[test]
    fn failures_are_per_ip() {
        let throttle = LoginThrottle::new();
        for _ in 0..MAX_LOGIN_FAILURES {
            throttle.record_failure(ip(3));
        }
        assert!(throttle.check(ip(3)).is_some(), "the offending ip is locked");
        assert!(throttle.check(ip(4)).is_none(), "a different ip is unaffected");
    }
}
