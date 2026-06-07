//! Static serving of the wasm admin UI bundle.
//!
//! When an `assets_dir` is configured the bundle is served from disk (dev
//! iteration). Otherwise the bundle baked into the binary at build time (under
//! `crates/server/web-dist/`) is served. Both apply an SPA fallback: unknown
//! paths return `index.html` so client-side routing works.
//!
//! The admin wasm module is large (tens of MB), so serving it efficiently
//! matters:
//!
//! * **Compression.** `tools/build_admin_web.sh` precompresses each asset to
//!   `<name>.br` (brotli) and `<name>.gz` (gzip). Both the embedded and the
//!   on-disk path negotiate `Accept-Encoding` and serve the precompressed
//!   variant with the matching `Content-Encoding`, falling back to the raw
//!   bytes when no precompressed sibling exists. Precompressing at build time
//!   keeps per-request CPU at zero (no on-the-fly recompression of the wasm).
//! * **Caching.** Embedded assets carry a content-hash `ETag` and a
//!   `Cache-Control: no-cache` (revalidate-always) header, so reloads return a
//!   tiny `304 Not Modified` instead of re-downloading the module. `ServeDir`
//!   does the equivalent `Last-Modified`/`If-Modified-Since` revalidation on
//!   the disk path.

use axum::{
    Router,
    http::{HeaderValue, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;
use std::path::PathBuf;
use tower_http::{
    services::{ServeDir, ServeFile},
    set_header::SetResponseHeaderLayer,
};

/// The admin UI bundle, embedded at build time. Populated by
/// `tools/build_admin_web.sh`; ships with a placeholder until then.
#[derive(RustEmbed)]
#[folder = "web-dist/"]
struct Assets;

/// `Cache-Control` applied to UI assets: cache, but always revalidate. Combined
/// with an `ETag`/`Last-Modified`, an unchanged asset costs a `304` round-trip
/// instead of re-downloading the (large) wasm module, while a rebuilt bundle is
/// picked up immediately.
const CACHE_CONTROL: &str = "no-cache";

/// Build the static-asset router (UI + SPA fallback).
pub fn router(assets_dir: Option<PathBuf>) -> Router {
    match assets_dir {
        Some(dir) => {
            let index = dir.join("index.html");
            // Serve `<file>.br`/`<file>.gz` when the client accepts them.
            let serve = ServeDir::new(dir)
                .precompressed_br()
                .precompressed_gzip()
                .fallback(ServeFile::new(index).precompressed_br().precompressed_gzip());
            Router::new()
                .fallback_service(serve)
                .layer(SetResponseHeaderLayer::overriding(
                    header::CACHE_CONTROL,
                    HeaderValue::from_static(CACHE_CONTROL),
                ))
        }
        None => Router::new().fallback(embedded),
    }
}

/// Encodings we serve precompressed, in descending preference order, paired with
/// the filename suffix of the precompressed sibling and the `Content-Encoding`
/// token it maps to.
const ENCODINGS: &[(&str, &str)] = &[("br", "br"), ("gzip", "gz")];

/// Pick the best precompressed encoding the client accepts. Returns
/// `(content_encoding, file_suffix)`, or `None` to serve the raw bytes.
///
/// Deliberately simple: a substring match on the `Accept-Encoding` header. We do
/// not honour `q=0` opt-outs, which are vanishingly rare for these tokens and
/// not worth a full header parser here.
fn negotiate_encoding(accept_encoding: Option<&str>) -> Option<(&'static str, &'static str)> {
    let accept = accept_encoding?;
    ENCODINGS
        .iter()
        .find(|(token, _)| accept.contains(token))
        .map(|&(token, suffix)| (token, suffix))
}

/// Fetch an embedded asset, preferring a precompressed sibling for `encoding`.
/// Returns the file plus the `Content-Encoding` to advertise (empty when serving
/// the raw bytes).
fn get_encoded(
    path: &str,
    encoding: Option<(&'static str, &'static str)>,
) -> Option<(rust_embed::EmbeddedFile, &'static str)> {
    if let Some((token, suffix)) = encoding {
        if let Some(file) = Assets::get(&format!("{path}.{suffix}")) {
            return Some((file, token));
        }
    }
    Assets::get(path).map(|file| (file, ""))
}

/// Render an `ETag` from a file's content hash (hex of the first 8 bytes of the
/// embedded sha256 — ample to detect changes, short on the wire).
fn etag(file: &rust_embed::EmbeddedFile) -> String {
    let h = file.metadata.sha256_hash();
    format!(
        "\"{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}\"",
        h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]
    )
}

/// Serve an embedded asset by path, falling back to `index.html` (SPA routing).
async fn embedded(uri: Uri, headers: axum::http::HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    let accept = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok());
    let encoding = negotiate_encoding(accept);

    if let Some((file, content_encoding)) = get_encoded(path, encoding) {
        let mime = mimetype_of(path);
        return asset_response(&mime, file, content_encoding, &headers);
    }

    // SPA fallback: unknown path -> index.html.
    match get_encoded("index.html", encoding) {
        Some((file, content_encoding)) => {
            let mime = mimetype_of("index.html");
            asset_response(&mime, file, content_encoding, &headers)
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// The MIME type of an embedded asset, derived from the *raw* (uncompressed)
/// file's path. Looking up the raw entry — not the `.br`/`.gz` sibling — avoids
/// mistyping the response as `application/brotli`/`application/gzip`. Falls back
/// to `application/octet-stream` for assets with no raw entry.
fn mimetype_of(path: &str) -> String {
    Assets::get(path)
        .map(|f| f.metadata.mimetype().to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string())
}

/// Build the HTTP response for an embedded asset, including caching/encoding
/// headers and `304 Not Modified` short-circuiting on a matching `If-None-Match`.
fn asset_response(
    mime: &str,
    file: rust_embed::EmbeddedFile,
    content_encoding: &'static str,
    req_headers: &axum::http::HeaderMap,
) -> Response {
    let etag = etag(&file);

    // Conditional request: if the client already has this exact version, save
    // the (potentially large) body.
    if let Some(inm) = req_headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) {
        if inm.split(',').any(|t| t.trim() == etag) {
            let mut resp = StatusCode::NOT_MODIFIED.into_response();
            resp.headers_mut().insert(header::ETAG, header_value(&etag));
            resp.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static(CACHE_CONTROL));
            return resp;
        }
    }

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, header_value(mime));
    headers.insert(header::ETAG, header_value(&etag));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(CACHE_CONTROL));
    headers.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
    if !content_encoding.is_empty() {
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static(content_encoding));
    }

    (headers, file.data.into_owned()).into_response()
}

/// Build a `HeaderValue` from a string, falling back to an empty value if it
/// somehow contains illegal bytes (mime/etag are ASCII, so this never trips).
fn header_value(s: &str) -> HeaderValue {
    HeaderValue::from_str(s).unwrap_or_else(|_| HeaderValue::from_static(""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    #[test]
    fn negotiates_brotli_over_gzip() {
        assert_eq!(negotiate_encoding(Some("gzip, deflate, br")), Some(("br", "br")));
        assert_eq!(negotiate_encoding(Some("gzip, deflate")), Some(("gzip", "gz")));
        assert_eq!(negotiate_encoding(Some("identity")), None);
        assert_eq!(negotiate_encoding(None), None);
    }

    /// Build the embedded-serving router (no on-disk assets dir).
    fn embedded_router() -> Router {
        router(None)
    }

    #[tokio::test]
    async fn serves_index_with_cache_and_etag() {
        let resp = embedded_router()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get(header::CACHE_CONTROL).unwrap(), CACHE_CONTROL);
        assert!(resp.headers().get(header::ETAG).is_some(), "index should carry an ETag");
    }

    #[tokio::test]
    async fn matching_if_none_match_returns_304() {
        let router = embedded_router();
        // First request to learn the current ETag.
        let first = router
            .clone()
            .oneshot(Request::builder().uri("/index.html").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let etag = first.headers().get(header::ETAG).unwrap().clone();

        // Re-request with the ETag -> 304, no body.
        let second = router
            .oneshot(
                Request::builder()
                    .uri("/index.html")
                    .header(header::IF_NONE_MATCH, &etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn unknown_path_falls_back_to_index() {
        let resp = embedded_router()
            .oneshot(
                Request::builder()
                    .uri("/some/client/route")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // SPA fallback serves index.html (HTML), not a 404.
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp.headers().get(header::CONTENT_TYPE).unwrap().to_str().unwrap();
        assert!(ct.contains("html"), "fallback should be html, got {ct}");
    }
}
