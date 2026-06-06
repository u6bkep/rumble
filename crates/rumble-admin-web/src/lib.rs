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
//! The projection (`app`, `inbox`, `api`) compiles on **both** targets: the
//! browser deps (`gloo-net`, `web-sys`, `wasm-bindgen`) only exist under
//! `cfg(target_arch = "wasm32")`, where the real `fetch` client lives, but
//! `api` carries a native stub of its transport so the whole `App` — and
//! therefore [`App::build`](damascene_core::prelude::App::build) — type-checks
//! and *renders* on the host. That host render path is what the `lint` binary
//! drives: it builds each screen against canned [`AdminApp`] state and runs
//! damascene's lint pass, with no browser involved. Only the wasm entry point
//! (`entry`, `#[wasm_bindgen(start)]`) stays `wasm32`-gated.
//!
//! Build the browser bundle with `tools/build_admin_web.sh` (wraps `wasm-pack
//! build --target web --release`); the output `pkg/` + `index.html` are served
//! by the axum control-plane. Run the UI lint gate with
//! `cargo run -p rumble-admin-web --bin lint`.

mod api;
mod app;
mod inbox;

pub use app::AdminApp;

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
