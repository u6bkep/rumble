//! Link against the system libmpv via pkg-config. We require >= 2.0
//! (libmpv 2.x ABI, mpv >= 0.35) for the stable software-render API
//! shape used in `src/sys.rs`.
//!
//! Windows is intentionally skipped: the crate compiles to stub
//! `MpvPlayer` / `VideoStream` types that return
//! [`crate::Error::Unsupported`] from every constructor. This lets
//! the desktop client cross-compile for `x86_64-pc-windows-gnu`
//! without bundling a Windows libmpv build. A future phase can
//! reintroduce Windows support by linking a vendored `libmpv-2.dll`.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        return;
    }
    pkg_config::Config::new()
        .atleast_version("2.0")
        .probe("mpv")
        .expect("libmpv >= 2.0 not found via pkg-config (install the `mpv` / `libmpv-dev` package)");
}
