//! Thin safe wrapper over libmpv's player + software-render APIs.
//!
//! Phase 1 of rumble-damascene's video support. Just enough surface to:
//! load a file, wait for the first frame, render that frame into a
//! caller-supplied RGBA buffer, and tear down cleanly. Future phases
//! grow play/pause/seek + the GPU-mirror pattern from
//! `rumble-damascene::animated_gpu`; the API here is shaped so those slot
//! in without revisiting the FFI layer.
//!
//! Software render output is `"rgb0"` — bytes laid out R, G, B,
//! garbage. Callers wanting an alpha-correct buffer should overwrite
//! every fourth byte with `0xff` post-render. This matches the
//! pixel-format requirements of `wgpu::TextureFormat::Rgba8UnormSrgb`
//! used elsewhere in rumble-damascene.
//!
//! ## Platform support
//!
//! Linux/macOS: `build.rs` probes pkg-config for a system
//! `libmpv >= 2.0`.
//!
//! Windows (`x86_64-pc-windows-gnu`): `build.rs` fetches a pinned
//! `mpv-dev-x86_64-…7z` from sourceforge, verifies a sha256, extracts
//! it into `OUT_DIR/libmpv-<version>/`, and copies `libmpv-2.dll`
//! next to the eventual binary so `cargo run --target
//! x86_64-pc-windows-gnu` produces a runnable artifact. Set
//! `LIBMPV_DIR` to skip the network step in offline / CI-cached
//! builds.

mod error;
mod gpu;
mod stream;
mod sys;

pub use error::Error;
pub use gpu::VideoGpu;
pub use stream::{FrameBuffer, VideoStream};

use std::{
    ffi::{CStr, CString},
    os::raw::{c_char, c_void},
    path::Path,
    ptr,
    sync::Arc,
    time::Duration,
};

use error::check;

/// Owned `*mut mpv_handle`, shared between the two public halves
/// ([`MpvPlayer`] / [`MpvRender`]) via `Arc`. `Drop` runs
/// `mpv_terminate_destroy`; the `Arc` guarantees that only happens
/// once *both* halves are gone, which (combined with
/// [`MpvRender`]'s `Drop` freeing the render context before its
/// `Arc` clone releases) preserves libmpv's "free the render
/// context before the core" teardown order.
struct Handle(*mut sys::mpv_handle);

// SAFETY: libmpv's core handle API is internally synchronised —
// per the libmpv docs every `mpv_*` function we call through
// `Handle` (`mpv_set_option_string`, `mpv_set_property_string`,
// `mpv_get_property`, `mpv_command`) is thread-safe and may be
// called from any thread, concurrently. The two functions that are
// NOT (`mpv_wait_event`: one caller thread at a time; the render
// context functions: externally synchronised) are only reachable
// through `MpvRender`, which is deliberately `!Sync`.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    /// Issue a libmpv command. `args` is the same shape libmpv's
    /// `mpv_command` expects: e.g. `&["loadfile", "/tmp/a.mp4"]`.
    fn command(&self, args: &[&str]) -> Result<(), Error> {
        // Allocate owned CStrings so the pointers stay valid for the
        // duration of the call.
        let owned: Vec<CString> = args
            .iter()
            .map(|a| CString::new(*a).map_err(|_| Error::InvalidArg("command arg contained NUL")))
            .collect::<Result<_, _>>()?;
        let mut argv: Vec<*const c_char> = owned.iter().map(|s| s.as_ptr()).collect();
        argv.push(ptr::null()); // libmpv expects a NULL sentinel
        let rc = unsafe { sys::mpv_command(self.0, argv.as_mut_ptr()) };
        check(rc, "mpv_command")
    }

    fn set_option_string(&self, name: &str, value: &str) -> Result<(), Error> {
        let name = CString::new(name).map_err(|_| Error::InvalidArg("option name contained NUL"))?;
        let value = CString::new(value).map_err(|_| Error::InvalidArg("option value contained NUL"))?;
        let rc = unsafe { sys::mpv_set_option_string(self.0, name.as_ptr(), value.as_ptr()) };
        check(rc, "mpv_set_option_string")
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { sys::mpv_terminate_destroy(self.0) };
        }
    }
}

/// The thread-safe command/property half of a libmpv player.
/// Created paired with its render/event half by
/// [`MpvPlayer::new`]; share this one freely (it is `Send + Sync`
/// through the inner `Arc<Handle>` — every method maps to a
/// libmpv call that is documented thread-safe).
///
/// The operations libmpv confines to a single thread
/// (`mpv_wait_event`, the render-context API) live on
/// [`MpvRender`], which is `Send` but **not** `Sync`, so the
/// threading contract is enforced by the type system rather than
/// usage discipline.
pub struct MpvPlayer {
    handle: Arc<Handle>,
}

/// The thread-confined render/event half of a libmpv player:
/// [`Self::load_file`] and [`Self::wait_for_frame`] pump
/// `mpv_wait_event` (one caller thread at a time per libmpv), and
/// [`Self::wait_for_frame`] / [`Self::render_sw`] drive the render
/// context (externally synchronised per libmpv). `Send` so it can
/// move into a decode worker thread; deliberately **not** `Sync`,
/// so `&self` calls can never race from two threads.
///
/// Owns the SW render context; `Drop` frees it. The shared
/// `Arc<Handle>` keeps the mpv core alive until both halves are
/// gone, and `Drop` ordering (explicit body before field drops)
/// guarantees the render context is freed before
/// `mpv_terminate_destroy` can run.
pub struct MpvRender {
    render_ctx: *mut sys::mpv_render_context,
    handle: Arc<Handle>,
}

// SAFETY: `MpvRender` owns the render context and is the only path
// to `mpv_wait_event` / `mpv_render_context_*`. Those calls are
// fine from any thread as long as they never run concurrently;
// the absence of a `Sync` impl (the raw `render_ctx` pointer
// suppresses the auto trait) means all `&self` access is confined
// to whichever single thread currently owns the value, so moving
// it between threads (`Send`) is sound.
unsafe impl Send for MpvRender {}

impl MpvPlayer {
    /// Create a fresh player + SW render context, returned as the
    /// shareable command/property half ([`MpvPlayer`]) and the
    /// thread-confined render/event half ([`MpvRender`]). Sets a
    /// small set of "library embedding" defaults (no terminal, no
    /// input handling, no config file scanning) so libmpv behaves
    /// predictably inside a host application instead of inheriting
    /// user mpv config from `~/.config/mpv`.
    pub fn new() -> Result<(Self, MpvRender), Error> {
        let raw = unsafe { sys::mpv_create() };
        if raw.is_null() {
            return Err(Error::OutOfMemory);
        }
        // Wrap immediately so early-return error paths below tear
        // the handle down instead of leaking it.
        let handle = Arc::new(Handle(raw));

        // Embedding-friendly defaults. Set before mpv_initialize per
        // the libmpv contract for these specific options.
        for (k, v) in [
            ("terminal", "no"),
            ("input-default-bindings", "no"),
            ("input-vo-keyboard", "no"),
            ("osc", "no"),
            ("config", "no"),
            // VO=libmpv routes frames through our render context.
            // Anything else (xv, drm, gpu...) would try to open its
            // own window, which is not what we want.
            ("vo", "libmpv"),
        ] {
            handle.set_option_string(k, v)?;
        }

        let rc = unsafe { sys::mpv_initialize(handle.0) };
        check(rc, "mpv_initialize")?;

        // Attach the SW render context. Phase 1 doesn't use the
        // update callback (we poll `mpv_render_context_update`
        // explicitly in `wait_for_frame`).
        let mut render_ctx: *mut sys::mpv_render_context = ptr::null_mut();
        let mut params = [
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_API_TYPE,
                data: sys::MPV_RENDER_API_TYPE_SW.as_ptr() as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];
        let rc = unsafe { sys::mpv_render_context_create(&mut render_ctx, handle.0, params.as_mut_ptr()) };
        check(rc, "mpv_render_context_create")?;

        let render = MpvRender {
            render_ctx,
            handle: Arc::clone(&handle),
        };
        Ok((Self { handle }, render))
    }

    /// Set a libmpv option (pre-init style). Most useful before
    /// playback starts; afterwards prefer [`Self::set_property_string`].
    pub fn set_option_string(&self, name: &str, value: &str) -> Result<(), Error> {
        self.handle.set_option_string(name, value)
    }

    /// Set a libmpv property at runtime (e.g. `pause = "yes"`,
    /// `volume = "60"`). Prefer the typed wrappers below where they
    /// exist.
    pub fn set_property_string(&self, name: &str, value: &str) -> Result<(), Error> {
        let name = CString::new(name).map_err(|_| Error::InvalidArg("property name contained NUL"))?;
        let value = CString::new(value).map_err(|_| Error::InvalidArg("property value contained NUL"))?;
        let rc = unsafe { sys::mpv_set_property_string(self.handle.0, name.as_ptr(), value.as_ptr()) };
        check(rc, "mpv_set_property_string")
    }

    /// Read an `int64` property. Returns the property's current
    /// value, or an error if the property doesn't exist / isn't yet
    /// available (e.g. `dwidth` before `MPV_EVENT_VIDEO_RECONFIG`
    /// fires).
    pub fn get_property_i64(&self, name: &str) -> Result<i64, Error> {
        let name = CString::new(name).map_err(|_| Error::InvalidArg("property name contained NUL"))?;
        let mut out: i64 = 0;
        let rc = unsafe {
            sys::mpv_get_property(
                self.handle.0,
                name.as_ptr(),
                sys::MPV_FORMAT_INT64,
                &mut out as *mut i64 as *mut c_void,
            )
        };
        check(rc, "mpv_get_property(int64)")?;
        Ok(out)
    }

    /// Read a `double` property. Used for `time-pos` / `duration`
    /// queries. Returns `Ok(None)` if libmpv reports the property
    /// is unavailable (e.g. `time-pos` before playback has actually
    /// started); other errors propagate.
    pub fn get_property_f64(&self, name: &str) -> Result<Option<f64>, Error> {
        let cname = CString::new(name).map_err(|_| Error::InvalidArg("property name contained NUL"))?;
        let mut out: f64 = 0.0;
        let rc = unsafe {
            sys::mpv_get_property(
                self.handle.0,
                cname.as_ptr(),
                sys::MPV_FORMAT_DOUBLE,
                &mut out as *mut f64 as *mut c_void,
            )
        };
        // mpv_error::MPV_ERROR_PROPERTY_UNAVAILABLE = -10. Surface as
        // `Ok(None)` so callers don't need to special-case the
        // "property not yet ready" race against decoder startup.
        if rc == -10 {
            return Ok(None);
        }
        check(rc, "mpv_get_property(double)")?;
        Ok(Some(out))
    }

    /// Issue a libmpv command. `args` is the same shape libmpv's
    /// `mpv_command` expects: e.g. `&["loadfile", "/tmp/a.mp4"]`.
    pub fn command(&self, args: &[&str]) -> Result<(), Error> {
        self.handle.command(args)
    }

    /// Decoded video size in pixels (post-aspect-correction): the
    /// `(dwidth, dheight)` libmpv properties. Only valid after
    /// [`MpvRender::load_file`] has returned successfully.
    pub fn dimensions(&self) -> Result<(u32, u32), Error> {
        let w = self.get_property_i64("dwidth")?;
        let h = self.get_property_i64("dheight")?;
        if w <= 0 || h <= 0 {
            return Err(Error::InvalidArg("dwidth/dheight not yet available"));
        }
        Ok((w as u32, h as u32))
    }

    /// Borrow libmpv's static error string for a code, for ad-hoc
    /// diagnostics in callers that already hold an error code from
    /// another call site.
    pub fn error_string(code: i32) -> &'static str {
        unsafe { CStr::from_ptr(sys::mpv_error_string(code)) }
            .to_str()
            .unwrap_or("invalid utf-8 in mpv error string")
    }
}

impl MpvRender {
    /// Load a file and block until libmpv has both opened it
    /// (`MPV_EVENT_FILE_LOADED`) and reported the video stream's
    /// configuration (`MPV_EVENT_VIDEO_RECONFIG`). Querying
    /// [`MpvPlayer::dimensions`] is safe immediately after this
    /// returns.
    ///
    /// Returns `LoadFailed` if libmpv reports `END_FILE` at any
    /// point (decoder couldn't open the container, codec
    /// unsupported, no video stream, etc.).
    ///
    /// The caller owns format gating — pass paths to files we
    /// actually expect libmpv to handle.
    pub fn load_file(&self, path: &Path) -> Result<(), Error> {
        let path_str = path.to_str().ok_or(Error::InvalidArg("path is not valid UTF-8"))?;
        self.handle.command(&["loadfile", path_str])?;

        // Drain events until both FILE_LOADED (file opens) and
        // VIDEO_RECONFIG (video stream's resolution is known) have
        // fired. Order isn't guaranteed in either direction, so
        // track them as flags and exit when both are set.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut file_loaded = false;
        let mut video_ready = false;
        while !(file_loaded && video_ready) {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(Error::Timeout("FILE_LOADED + VIDEO_RECONFIG"));
            }
            let timeout = remaining.as_secs_f64().min(1.0);
            let ev = unsafe { &*sys::mpv_wait_event(self.handle.0, timeout) };
            match ev.event_id {
                sys::MPV_EVENT_FILE_LOADED => file_loaded = true,
                sys::MPV_EVENT_VIDEO_RECONFIG => video_ready = true,
                sys::MPV_EVENT_END_FILE => return Err(Error::LoadFailed),
                sys::MPV_EVENT_SHUTDOWN => return Err(Error::Shutdown),
                _ => continue,
            }
        }
        Ok(())
    }

    /// Block (up to `timeout`) until the SW render context reports
    /// that a new frame is ready to be pulled with [`Self::render_sw`].
    /// Returns `Ok(true)` if a frame is ready, `Ok(false)` on
    /// timeout. Pumps libmpv's event queue while waiting so log
    /// messages and other signals don't pile up unbounded.
    ///
    /// `MPV_EVENT_END_FILE` is intentionally ignored here: during
    /// normal playback it can fire transiently between loop
    /// iterations of a `loop-file=inf` stream, and at true
    /// end-of-stream the absence of further frames will surface
    /// naturally as an `Ok(false)` timeout. Genuine load failures
    /// are caught by [`Self::load_file`] before this method is
    /// ever called.
    pub fn wait_for_frame(&self, timeout: Duration) -> Result<bool, Error> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let flags = unsafe { sys::mpv_render_context_update(self.render_ctx) };
            if flags & sys::MPV_RENDER_UPDATE_FRAME != 0 {
                return Ok(true);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            // Cap the wait_event timeout so we re-check the render
            // update flag promptly if libmpv signals frame-ready
            // without pushing an event we recognise.
            let step = remaining.as_secs_f64().min(0.05);
            let ev = unsafe { &*sys::mpv_wait_event(self.handle.0, step) };
            if ev.event_id == sys::MPV_EVENT_SHUTDOWN {
                return Err(Error::Shutdown);
            }
        }
    }

    /// Render the current frame into `buf` as `width × height` RGBA
    /// pixels with `stride` bytes per row (must be a multiple of 4
    /// and >= `width * 4`). The buffer must have capacity for
    /// `stride * height` bytes; libmpv writes the full slab.
    ///
    /// libmpv's SW format produces R, G, B, garbage per pixel; we
    /// post-process to set the alpha byte to `0xff` so callers can
    /// upload directly into an `Rgba8UnormSrgb` texture.
    pub fn render_sw(&self, buf: &mut [u8], width: u32, height: u32, stride: usize) -> Result<(), Error> {
        if stride < (width as usize) * 4 || !stride.is_multiple_of(4) {
            return Err(Error::InvalidArg("stride must be >= width*4 and a multiple of 4"));
        }
        let needed = stride
            .checked_mul(height as usize)
            .ok_or(Error::InvalidArg("stride*height overflow"))?;
        if buf.len() < needed {
            return Err(Error::InvalidArg("buffer too small for stride*height"));
        }

        let mut size: [i32; 2] = [width as i32, height as i32];
        let mut stride_v: usize = stride;
        let format = c"rgb0";
        let mut params = [
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_SW_SIZE,
                data: size.as_mut_ptr() as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_SW_FORMAT,
                data: format.as_ptr() as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_SW_STRIDE,
                data: &mut stride_v as *mut usize as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_SW_POINTER,
                data: buf.as_mut_ptr() as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];
        let rc = unsafe { sys::mpv_render_context_render(self.render_ctx, params.as_mut_ptr()) };
        check(rc, "mpv_render_context_render")?;

        // libmpv left the alpha bytes uninitialised. Force them to
        // 0xff so the buffer is a valid premultiplied-irrelevant
        // opaque RGBA frame for downstream samplers.
        for px in buf[..needed].chunks_exact_mut(4) {
            px[3] = 0xff;
        }
        Ok(())
    }
}

impl Drop for MpvRender {
    fn drop(&mut self) {
        // Render context must be freed *before* the handle, per the
        // libmpv docs ("you must free the context before the mpv
        // core is destroyed"). This body runs before the `handle`
        // field's `Arc` clone drops, so even when this is the last
        // reference, `mpv_terminate_destroy` (in `Handle::drop`)
        // strictly follows the free below.
        if !self.render_ctx.is_null() {
            unsafe { sys::mpv_render_context_free(self.render_ctx) };
        }
    }
}

#[cfg(test)]
mod thread_contract {
    use super::*;

    /// Compile-time checks of the split-type threading contract:
    /// the command/property half is shareable, the render/event
    /// half is movable. (`MpvRender: !Sync` is upheld by the raw
    /// `render_ctx` pointer suppressing the auto trait — adding a
    /// `Sync` impl would be an unsoundness regression.)
    #[test]
    fn split_halves_have_expected_auto_traits() {
        fn assert_send_sync<T: Send + Sync>() {}
        fn assert_send<T: Send>() {}
        assert_send_sync::<MpvPlayer>();
        assert_send::<MpvRender>();
    }
}
