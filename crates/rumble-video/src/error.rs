#[cfg(not(windows))]
use std::ffi::CStr;

#[cfg(not(windows))]
use crate::sys;

/// Errors surfaced from the libmpv FFI layer. The numeric `code` is
/// libmpv's `mpv_error` value; `message` is whatever
/// `mpv_error_string()` returns for that code (always a static C
/// string per the API).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("libmpv error {code}: {message} (during {context})")]
    Mpv {
        code: i32,
        message: &'static str,
        context: &'static str,
    },

    #[error("libmpv: out of memory creating handle")]
    OutOfMemory,

    #[error("libmpv: file failed to load (END_FILE before FILE_LOADED)")]
    LoadFailed,

    #[error("libmpv: timed out waiting for {0}")]
    Timeout(&'static str),

    #[error("libmpv: handle already terminated")]
    Shutdown,

    #[error("libmpv: invalid argument — {0}")]
    InvalidArg(&'static str),

    /// Returned by every `MpvPlayer` / `VideoStream` constructor on
    /// platforms where libmpv isn't linked in. Currently: Windows.
    #[error("rumble-video: unsupported on this platform — {0}")]
    Unsupported(&'static str),
}

#[cfg(not(windows))]
impl Error {
    pub(crate) fn from_code(code: i32, context: &'static str) -> Self {
        // Safety: per docs `mpv_error_string` returns a static
        // C string for any `mpv_error` value (and for unknown
        // codes too).
        let cstr = unsafe { CStr::from_ptr(sys::mpv_error_string(code)) };
        // The static-string guarantee means we can safely treat
        // this as a `&'static str`.
        let message: &'static str = unsafe {
            let bytes = cstr.to_bytes();
            std::str::from_utf8(std::slice::from_raw_parts(bytes.as_ptr(), bytes.len()))
                .unwrap_or("invalid utf-8 in mpv error string")
        };
        Self::Mpv { code, message, context }
    }
}

/// Convenience: convert a libmpv int return into `Result`. `0` is
/// success per the libmpv convention; negative values map through
/// `mpv_error_string`.
#[cfg(not(windows))]
pub(crate) fn check(code: i32, context: &'static str) -> Result<(), Error> {
    if code >= 0 {
        Ok(())
    } else {
        Err(Error::from_code(code, context))
    }
}
