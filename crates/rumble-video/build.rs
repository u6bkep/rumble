//! Link against the system libmpv via pkg-config. We require >= 2.0
//! (libmpv 2.x ABI, mpv >= 0.35) for the stable software-render API
//! shape used in `src/sys.rs`.

fn main() {
    pkg_config::Config::new()
        .atleast_version("2.0")
        .probe("mpv")
        .expect("libmpv >= 2.0 not found via pkg-config (install the `mpv` / `libmpv-dev` package)");
}
