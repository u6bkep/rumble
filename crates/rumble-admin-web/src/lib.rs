//! Rumble server **web admin UI** — a Damascene application compiled to
//! wasm that drives the server's REST control-plane (`server::web`).
//!
//! The whole app is a `damascene_core::App` ([`app::AdminApp`]) projecting
//! login / bootstrap / dashboard screens, with side effects routed through
//! a small [`api`] fetch client. Async REST responses are funnelled back
//! into the single-threaded render loop through a shared [`inbox::Inbox`]
//! that also holds the host's redraw handle, so a completed fetch wakes the
//! frame loop on-demand.
//!
//! Everything substantive is gated to `cfg(target_arch = "wasm32")`: the
//! browser deps (`gloo-net`, `web-sys`, `wasm-bindgen`) only exist there,
//! and `damascene-web`'s native stub keeps the crate type-checking on the
//! host target so it can ride along as an ordinary workspace member.
//!
//! Build with `tools/build_admin_web.sh` (wraps `wasm-pack build --target
//! web --release`); the output `pkg/` + `index.html` are served by the
//! axum control-plane.

#[cfg(target_arch = "wasm32")]
mod api;
#[cfg(target_arch = "wasm32")]
mod app;
#[cfg(target_arch = "wasm32")]
mod inbox;

#[cfg(target_arch = "wasm32")]
mod entry {
    use std::{cell::RefCell, rc::Rc};

    use damascene_web::{VIEWPORT, WebHostConfig, start_with_config};
    use wasm_bindgen::prelude::*;

    use crate::{app::AdminApp, inbox::Inbox};

    /// Canvas element id the page must provide (see `index.html`).
    const CANVAS_ID: &str = "rumble_admin_canvas";

    /// wasm entry point, invoked by the generated JS glue on module load.
    #[wasm_bindgen(start)]
    pub fn start() {
        // The inbox is created before the app so the app can hold a clone
        // for its fetch callbacks; the host redraw handle is injected back
        // into it once `start_with_config` returns.
        let inbox = Rc::new(RefCell::new(Inbox::new()));
        let app = AdminApp::new(inbox.clone());
        let handle = start_with_config(WebHostConfig::new(VIEWPORT).with_canvas_id(CANVAS_ID), app);
        inbox.borrow_mut().set_handle(handle);
    }
}
