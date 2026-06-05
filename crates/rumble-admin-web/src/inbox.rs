//! The bridge between async REST callbacks and the synchronous render loop.
//!
//! Damascene's browser host redraws on-demand, not on a continuous timer, so
//! a `fetch` that completes off the frame path has to *wake* the loop. Every
//! API call pushes its result into this shared inbox and, if the host handle
//! is present, calls `request_redraw`. [`AdminApp::before_build`] drains the
//! queued [`Msg`]s into app state on the next frame.
//!
//! [`AdminApp::before_build`]: crate::app::AdminApp

use damascene_web::WebHandle;
use rumble_web_types::{SessionInfo, StateSnapshot};

/// One async result delivered back to the app.
pub enum Msg {
    /// Initial / refreshed session probe (`GET /api/session`).
    Session(SessionInfo),
    /// Login succeeded — the session cookie is now set.
    LoginOk,
    /// First-run bootstrap succeeded.
    Bootstrapped,
    /// Logout completed.
    LoggedOut,
    /// A dashboard state snapshot arrived (`GET /api/state`).
    State(StateSnapshot),
    /// A mutating action succeeded; carries a human-readable status line.
    ActionOk(String),
    /// Any request failed; carries a user-facing reason.
    Error(String),
}

/// Shared mailbox: queued messages plus the host redraw handle.
pub struct Inbox {
    messages: Vec<Msg>,
    handle: Option<WebHandle>,
}

impl Inbox {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            handle: None,
        }
    }

    /// Inject the host redraw handle once the runner exists. Until this is
    /// set, pushes still queue — they just don't force a wake (the initial
    /// frame is already scheduled, and the handle lands before any network
    /// round-trip can complete).
    pub fn set_handle(&mut self, handle: WebHandle) {
        self.handle = Some(handle);
    }

    /// Queue a message and wake the render loop.
    pub fn push(&mut self, msg: Msg) {
        self.messages.push(msg);
        if let Some(handle) = &self.handle {
            handle.request_redraw();
        }
    }

    /// Take everything queued since the last frame.
    pub fn drain(&mut self) -> Vec<Msg> {
        std::mem::take(&mut self.messages)
    }
}
