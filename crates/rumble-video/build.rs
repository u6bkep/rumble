//! Native (Linux/macOS): probes pkg-config for a system libmpv >= 2.0
//! (libmpv 2.x ABI, mpv >= 0.35) — the same shape that `src/sys.rs`
//! is bound against.
//!
//! Windows: there is no widely-used Windows libmpv pkg-config setup,
//! and the existing Rust wrappers (libmpv-rs, libmpv2) all push
//! responsibility onto the user via an `MPV_SOURCE` environment
//! variable. We do better here: the build script downloads a pinned
//! `mpv-dev-x86_64-…7z` from the official mpv-player-windows
//! distribution on sourceforge, verifies a sha256, extracts it into
//! `OUT_DIR/libmpv/`, and copies `libmpv-2.dll` next to the eventual
//! binary so `cargo run --target x86_64-pc-windows-gnu` "just works"
//! without vendoring binaries into the repo.
//!
//! The fetch is target-conditional, so a Linux contributor doing
//! `cargo build` (no `--target windows`) never touches the network.
//!
//! Override knobs:
//! - `LIBMPV_DIR` — point at a directory containing `libmpv.dll.a`
//!   and `libmpv-2.dll`. Skips the download (useful for offline
//!   builds and CI caches).

use std::env;

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        windows_fetch::run();
    } else {
        pkg_config::Config::new()
            .atleast_version("2.0")
            .probe("mpv")
            .expect("libmpv >= 2.0 not found via pkg-config (install the `mpv` / `libmpv-dev` package)");
    }
}

// ---------------------------------------------------------------------
// Windows fetch
// ---------------------------------------------------------------------

mod windows_fetch {
    //! Build-time fetch of a pinned libmpv distribution for Windows
    //! GNU targets. Compiled in for any host (cross-compile from
    //! Linux is the common case here); the actual work is gated on
    //! `CARGO_CFG_TARGET_OS == "windows"` so non-Windows targets are
    //! unaffected.

    use std::{
        env, fs,
        io::{Read, Write},
        path::{Path, PathBuf},
    };

    /// Pinned mpv-dev release. Bump in lockstep with `URL` and `SHA256`.
    /// Source: https://sourceforge.net/projects/mpv-player-windows/files/libmpv/
    const VERSION: &str = "20260419-git-06f4ce7";
    const URL: &str = "https://sourceforge.net/projects/mpv-player-windows/files/libmpv/mpv-dev-x86_64-20260419-git-06f4ce7.7z/download";
    const SHA256: &str = "67cfa44cd7fadefd9248434176fdfa95b4a3ea2896c47b942199f4e2c1d22ec8";

    pub fn run() {
        let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
        if target_env != "gnu" {
            // The mpv-dev archive ships only the MinGW import lib
            // (`libmpv.dll.a`). MSVC users would need a separate
            // `mpv.lib` (or convert from `mpv.def` via `lib.exe`).
            // Skip noisily so the failure mode is a clear panic
            // instead of an opaque link error.
            panic!(
                "rumble-video: only x86_64-pc-windows-gnu is supported on Windows. The bundled mpv-dev archive ships \
                 a MinGW import library; MSVC builds need a separate `mpv.lib`. Got target_env={}.",
                target_env
            );
        }

        let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR set by cargo"));
        let libmpv_dir = match env::var_os("LIBMPV_DIR") {
            Some(p) => {
                let p = PathBuf::from(p);
                println!("cargo:rerun-if-env-changed=LIBMPV_DIR");
                println!("cargo:warning=rumble-video: using LIBMPV_DIR={}", p.display());
                p
            }
            None => {
                let dir = out_dir.join(format!("libmpv-{VERSION}"));
                ensure_extracted(&out_dir, &dir);
                dir
            }
        };

        let import_lib = libmpv_dir.join("libmpv.dll.a");
        let dll = libmpv_dir.join("libmpv-2.dll");
        if !import_lib.is_file() || !dll.is_file() {
            panic!(
                "rumble-video: expected libmpv.dll.a and libmpv-2.dll in {} (override via LIBMPV_DIR)",
                libmpv_dir.display()
            );
        }

        // Tell rustc where to find the import lib at link time, and
        // request a dynamic link against `mpv` (resolves to
        // `libmpv.dll.a`, which references symbols in `libmpv-2.dll`).
        println!("cargo:rustc-link-search=native={}", libmpv_dir.display());
        println!("cargo:rustc-link-lib=dylib=mpv");

        // Drop the DLL next to the eventual binary so `cargo run`
        // and downstream `cargo build`s produce a runnable artifact
        // without an out-of-band copy step. The path math here is the
        // long-standing `OUT_DIR.ancestors()` pattern other -sys
        // crates (aws-lc-sys, ring, etc.) use. Cargo issue #1614 is
        // the open ask for an official mechanism.
        if let Some(profile_dir) = profile_dir(&out_dir) {
            let dest = profile_dir.join("libmpv-2.dll");
            if needs_copy(&dll, &dest) {
                fs::copy(&dll, &dest).unwrap_or_else(|e| {
                    panic!(
                        "rumble-video: failed to copy {} -> {}: {e}",
                        dll.display(),
                        dest.display()
                    )
                });
            }
        }

        println!("cargo:rerun-if-changed=build.rs");
    }

    /// `target/<target>/<profile>/` derived from
    /// `target/<target>/<profile>/build/<crate-XXXX>/out`.
    fn profile_dir(out_dir: &Path) -> Option<PathBuf> {
        // out (0) -> <crate-XXXX> (1) -> build (2) -> <profile> (3)
        out_dir.ancestors().nth(3).map(Path::to_path_buf)
    }

    fn needs_copy(src: &Path, dest: &Path) -> bool {
        match (fs::metadata(src), fs::metadata(dest)) {
            (Ok(a), Ok(b)) => a.len() != b.len(),
            _ => true,
        }
    }

    /// Download (if needed), verify, and extract the mpv-dev archive
    /// into `dest`. Idempotent — bails early when `dest` already
    /// contains the expected payload.
    fn ensure_extracted(out_dir: &Path, dest: &Path) {
        let import_lib = dest.join("libmpv.dll.a");
        let dll = dest.join("libmpv-2.dll");
        if import_lib.is_file() && dll.is_file() {
            return;
        }

        let archive = out_dir.join(format!("libmpv-{VERSION}.7z"));
        if !archive.is_file() {
            println!(
                "cargo:warning=rumble-video: downloading {} ({} MB) -> {}",
                URL,
                30,
                archive.display()
            );
            download(URL, &archive);
        }
        verify_sha256(&archive, SHA256);

        // Clean target dir so a half-extracted previous run doesn't
        // leave stale files around.
        if dest.exists() {
            fs::remove_dir_all(dest).ok();
        }
        fs::create_dir_all(dest).expect("create libmpv extract dir");
        extract_7z(&archive, dest);

        // The archive lays the files directly at the root, but if a
        // future revision adds a top-level subdir we want to flatten
        // it out so the link-search path stays predictable.
        flatten_single_subdir(dest);

        if !import_lib.is_file() || !dll.is_file() {
            panic!(
                "rumble-video: extraction produced unexpected layout in {} (no libmpv.dll.a / libmpv-2.dll)",
                dest.display()
            );
        }
    }

    fn download(url: &str, dest: &Path) {
        let body = ureq::get(url)
            .call()
            .unwrap_or_else(|e| panic!("rumble-video: GET {url} failed: {e}"))
            .into_body();
        let mut reader = body.into_reader();
        let mut file = fs::File::create(dest).unwrap_or_else(|e| {
            panic!("rumble-video: create {}: {e}", dest.display());
        });
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = reader
                .read(&mut buf)
                .unwrap_or_else(|e| panic!("rumble-video: read body: {e}"));
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])
                .unwrap_or_else(|e| panic!("rumble-video: write archive: {e}"));
        }
    }

    fn verify_sha256(path: &Path, expected: &str) {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        let mut file = fs::File::open(path).unwrap_or_else(|e| panic!("rumble-video: open {}: {e}", path.display()));
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = file
                .read(&mut buf)
                .unwrap_or_else(|e| panic!("rumble-video: read {}: {e}", path.display()));
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let actual = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        if actual != expected {
            // Drop the bad archive so a subsequent build re-downloads
            // instead of looping forever on a poisoned cache.
            fs::remove_file(path).ok();
            panic!(
                "rumble-video: libmpv archive sha256 mismatch (cache poisoned?)\n  expected {expected}\n  got      \
                 {actual}\n  path     {}\n  url      {URL}",
                path.display()
            );
        }
    }

    fn extract_7z(archive: &Path, dest: &Path) {
        sevenz_rust2::decompress_file(archive, dest).unwrap_or_else(|e| {
            panic!(
                "rumble-video: 7z extract {} -> {}: {e}",
                archive.display(),
                dest.display()
            );
        });
    }

    /// If extraction produces a single top-level subdirectory,
    /// flatten its contents into `dest` and remove the wrapper. The
    /// current archive layout doesn't need this, but a future revision
    /// of the upstream packaging might add one and we'd rather adapt
    /// quietly than re-pin and recheck the sha.
    fn flatten_single_subdir(dest: &Path) {
        let entries: Vec<_> = match fs::read_dir(dest) {
            Ok(rd) => rd.filter_map(Result::ok).collect(),
            Err(_) => return,
        };
        if entries.len() != 1 {
            return;
        }
        let only = &entries[0];
        if !only.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            return;
        }
        let inner = only.path();
        let temp = dest.with_extension("flatten-tmp");
        fs::rename(&inner, &temp).ok();
        fs::remove_dir_all(dest).ok();
        fs::rename(&temp, dest).ok();
    }
}
