//! Thin safe wrapper over libmpv's player + software-render APIs.
//!
//! Phase 1 of rumble-aetna's video support. Just enough surface to:
//! load a file, wait for the first frame, render that frame into a
//! caller-supplied RGBA buffer, and tear down cleanly. Future phases
//! grow play/pause/seek + the GPU-mirror pattern from
//! `rumble-aetna::animated_gpu`; the API here is shaped so those slot
//! in without revisiting the FFI layer.
//!
//! Software render output is `"rgb0"` — bytes laid out R, G, B,
//! garbage. Callers wanting an alpha-correct buffer should overwrite
//! every fourth byte with `0xff` post-render. This matches the
//! pixel-format requirements of `wgpu::TextureFormat::Rgba8UnormSrgb`
//! used elsewhere in rumble-aetna.

mod error;
mod sys;

pub use error::Error;

use std::{
    ffi::{CStr, CString},
    os::raw::{c_char, c_void},
    path::Path,
    ptr,
    time::Duration,
};

use error::check;

/// Owned libmpv player handle plus a SW render context attached to it.
/// `Drop` tears both down in the correct order (render context first,
/// then `mpv_terminate_destroy`).
pub struct MpvPlayer {
    /// `*mut mpv_handle`. Internally synchronised by libmpv — safe to
    /// hand to multiple threads as long as no two methods are called
    /// concurrently on the same `&mut`.
    handle: *mut sys::mpv_handle,
    render_ctx: *mut sys::mpv_render_context,
}

// libmpv's handle is internally synchronised across threads. Holding
// the player on a worker thread (Phase 2 use case) is fine.
unsafe impl Send for MpvPlayer {}

impl MpvPlayer {
    /// Create a fresh player + SW render context. Sets a small set of
    /// "library embedding" defaults (no terminal, no input handling,
    /// no config file scanning) so libmpv behaves predictably inside a
    /// host application instead of inheriting user mpv config from
    /// `~/.config/mpv`.
    pub fn new() -> Result<Self, Error> {
        let handle = unsafe { sys::mpv_create() };
        if handle.is_null() {
            return Err(Error::OutOfMemory);
        }

        // Build a guard-then-fill struct so we don't leak the handle
        // if any of the option/init calls fail before we attach the
        // render context.
        let mut player = Self {
            handle,
            render_ctx: ptr::null_mut(),
        };

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
            player.set_option_string(k, v)?;
        }

        let rc = unsafe { sys::mpv_initialize(player.handle) };
        check(rc, "mpv_initialize")?;

        // Attach the SW render context. Phase 1 doesn't use the
        // update callback (we poll `mpv_render_context_update`
        // explicitly in `wait_for_frame`).
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
        let rc = unsafe { sys::mpv_render_context_create(&mut player.render_ctx, player.handle, params.as_mut_ptr()) };
        check(rc, "mpv_render_context_create")?;

        Ok(player)
    }

    /// Set a libmpv option (pre-init style). Most useful before
    /// playback starts; afterwards prefer [`Self::set_property_string`].
    pub fn set_option_string(&self, name: &str, value: &str) -> Result<(), Error> {
        let name = CString::new(name).map_err(|_| Error::InvalidArg("option name contained NUL"))?;
        let value = CString::new(value).map_err(|_| Error::InvalidArg("option value contained NUL"))?;
        let rc = unsafe { sys::mpv_set_option_string(self.handle, name.as_ptr(), value.as_ptr()) };
        check(rc, "mpv_set_option_string")
    }

    /// Set a libmpv property at runtime (e.g. `pause = "yes"`,
    /// `volume = "60"`). Prefer the typed wrappers below where they
    /// exist.
    pub fn set_property_string(&self, name: &str, value: &str) -> Result<(), Error> {
        let name = CString::new(name).map_err(|_| Error::InvalidArg("property name contained NUL"))?;
        let value = CString::new(value).map_err(|_| Error::InvalidArg("property value contained NUL"))?;
        let rc = unsafe { sys::mpv_set_property_string(self.handle, name.as_ptr(), value.as_ptr()) };
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
                self.handle,
                name.as_ptr(),
                sys::MPV_FORMAT_INT64,
                &mut out as *mut i64 as *mut c_void,
            )
        };
        check(rc, "mpv_get_property(int64)")?;
        Ok(out)
    }

    /// Issue a libmpv command. `args` is the same shape libmpv's
    /// `mpv_command` expects: e.g. `&["loadfile", "/tmp/a.mp4"]`.
    pub fn command(&self, args: &[&str]) -> Result<(), Error> {
        // Allocate owned CStrings so the pointers stay valid for the
        // duration of the call.
        let owned: Vec<CString> = args
            .iter()
            .map(|a| CString::new(*a).map_err(|_| Error::InvalidArg("command arg contained NUL")))
            .collect::<Result<_, _>>()?;
        let mut argv: Vec<*const c_char> = owned.iter().map(|s| s.as_ptr()).collect();
        argv.push(ptr::null()); // libmpv expects a NULL sentinel
        let rc = unsafe { sys::mpv_command(self.handle, argv.as_mut_ptr()) };
        check(rc, "mpv_command")
    }

    /// Load a file and block until libmpv has both opened it
    /// (`MPV_EVENT_FILE_LOADED`) and reported the video stream's
    /// configuration (`MPV_EVENT_VIDEO_RECONFIG`). Querying
    /// [`Self::dimensions`] is safe immediately after this returns.
    ///
    /// Returns `LoadFailed` if libmpv reports `END_FILE` at any
    /// point (decoder couldn't open the container, codec
    /// unsupported, no video stream, etc.).
    ///
    /// The caller owns format gating — pass paths to files we
    /// actually expect libmpv to handle.
    pub fn load_file(&self, path: &Path) -> Result<(), Error> {
        let path_str = path.to_str().ok_or(Error::InvalidArg("path is not valid UTF-8"))?;
        self.command(&["loadfile", path_str])?;

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
            let ev = unsafe { &*sys::mpv_wait_event(self.handle, timeout) };
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
    /// messages and end-of-file signals don't pile up unbounded.
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
            let ev = unsafe { &*sys::mpv_wait_event(self.handle, step) };
            match ev.event_id {
                sys::MPV_EVENT_END_FILE => return Err(Error::LoadFailed),
                sys::MPV_EVENT_SHUTDOWN => return Err(Error::Shutdown),
                _ => {}
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
    pub fn render_sw(&mut self, buf: &mut [u8], width: u32, height: u32, stride: usize) -> Result<(), Error> {
        if stride < (width as usize) * 4 || stride % 4 != 0 {
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

    /// Decoded video size in pixels (post-aspect-correction): the
    /// `(dwidth, dheight)` libmpv properties. Only valid after
    /// [`Self::load_file`] has returned successfully.
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

impl Drop for MpvPlayer {
    fn drop(&mut self) {
        // Render context must be freed *before* the handle, per the
        // libmpv docs ("you must free the context before the mpv
        // core is destroyed").
        if !self.render_ctx.is_null() {
            unsafe { sys::mpv_render_context_free(self.render_ctx) };
        }
        if !self.handle.is_null() {
            unsafe { sys::mpv_terminate_destroy(self.handle) };
        }
    }
}
