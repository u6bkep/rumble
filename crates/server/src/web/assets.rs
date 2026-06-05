//! Static serving of the wasm admin UI bundle.
//!
//! When an `assets_dir` is configured the bundle is served from disk (dev
//! iteration). Otherwise the bundle baked into the binary at build time (under
//! `crates/server/web-dist/`) is served. Both apply an SPA fallback: unknown
//! paths return `index.html` so client-side routing works.

use axum::{
    Router,
    http::{Uri, header},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;
use std::path::PathBuf;
use tower_http::services::{ServeDir, ServeFile};

/// The admin UI bundle, embedded at build time. Populated by
/// `tools/build_admin_web.sh`; ships with a placeholder until then.
#[derive(RustEmbed)]
#[folder = "web-dist/"]
struct Assets;

/// Build the static-asset router (UI + SPA fallback).
pub fn router(assets_dir: Option<PathBuf>) -> Router {
    match assets_dir {
        Some(dir) => {
            let index = dir.join("index.html");
            let serve = ServeDir::new(dir).fallback(ServeFile::new(index));
            Router::new().fallback_service(serve)
        }
        None => Router::new().fallback(embedded),
    }
}

/// Serve an embedded asset by path, falling back to `index.html`.
async fn embedded(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(content) = Assets::get(path) {
        let mime = content.metadata.mimetype().to_string();
        return ([(header::CONTENT_TYPE, mime)], content.data.into_owned()).into_response();
    }

    // SPA fallback.
    match Assets::get("index.html") {
        Some(content) => (
            [(header::CONTENT_TYPE, "text/html".to_string())],
            content.data.into_owned(),
        )
            .into_response(),
        None => (axum::http::StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
