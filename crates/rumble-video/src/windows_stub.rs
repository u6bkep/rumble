//! Windows stubs for the libmpv-backed types in [`crate::stream`]
//! and the `MpvPlayer` in [`crate::lib`]. The libmpv FFI isn't built
//! on Windows (see `build.rs`); these stubs preserve the public API
//! shape so [`rumble_aetna`] can call into the crate unconditionally
//! and gracefully receive `Err(Error::Unsupported)` from every
//! constructor.
//!
//! Once a Windows libmpv build is bundled, replace this module with
//! a real implementation behind the same surface.

use std::{path::Path, time::Duration};

use crate::Error;

const MSG: &str = "libmpv is not bundled in Windows builds yet";

// ---------------------------------------------------------------------
// MpvPlayer stub
// ---------------------------------------------------------------------

pub struct MpvPlayer {
    _private: (),
}

impl MpvPlayer {
    pub fn new() -> Result<Self, Error> {
        Err(Error::Unsupported(MSG))
    }

    pub fn set_option_string(&self, _name: &str, _value: &str) -> Result<(), Error> {
        Err(Error::Unsupported(MSG))
    }

    pub fn set_property_string(&self, _name: &str, _value: &str) -> Result<(), Error> {
        Err(Error::Unsupported(MSG))
    }

    pub fn get_property_i64(&self, _name: &str) -> Result<i64, Error> {
        Err(Error::Unsupported(MSG))
    }

    pub fn get_property_f64(&self, _name: &str) -> Result<Option<f64>, Error> {
        Err(Error::Unsupported(MSG))
    }

    pub fn command(&self, _args: &[&str]) -> Result<(), Error> {
        Err(Error::Unsupported(MSG))
    }

    pub fn load_file(&self, _path: &Path) -> Result<(), Error> {
        Err(Error::Unsupported(MSG))
    }

    pub fn wait_for_frame(&self, _timeout: Duration) -> Result<bool, Error> {
        Err(Error::Unsupported(MSG))
    }

    pub fn render_sw(&self, _buf: &mut [u8], _width: u32, _height: u32, _stride: usize) -> Result<(), Error> {
        Err(Error::Unsupported(MSG))
    }

    pub fn dimensions(&self) -> Result<(u32, u32), Error> {
        Err(Error::Unsupported(MSG))
    }

    pub fn error_string(_code: i32) -> &'static str {
        MSG
    }
}

// ---------------------------------------------------------------------
// FrameBuffer (cross-platform shape, surfaced for `gpu.rs`)
// ---------------------------------------------------------------------

pub struct FrameBuffer {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
}

// ---------------------------------------------------------------------
// VideoStream stub
// ---------------------------------------------------------------------

/// Windows stub. `open` always returns `Err(Error::Unsupported)`, so
/// no instance of this type ever exists at runtime — the other
/// methods are present only to keep the public API uniform across
/// platforms (and to satisfy `crate::gpu::VideoGpu`'s signature).
pub struct VideoStream {
    _private: (),
}

impl VideoStream {
    pub fn open(_path: &Path, _looped: bool) -> Result<Self, Error> {
        Err(Error::Unsupported(MSG))
    }

    pub fn pause(&self) -> Result<(), Error> {
        Err(Error::Unsupported(MSG))
    }

    pub fn resume(&self) -> Result<(), Error> {
        Err(Error::Unsupported(MSG))
    }

    pub fn set_property_string(&self, _name: &str, _value: &str) -> Result<(), Error> {
        Err(Error::Unsupported(MSG))
    }

    pub fn seek(&self, _target: Duration) -> Result<(), Error> {
        Err(Error::Unsupported(MSG))
    }

    pub fn time_pos(&self) -> Result<Option<Duration>, Error> {
        Err(Error::Unsupported(MSG))
    }

    pub fn duration(&self) -> Result<Option<Duration>, Error> {
        Err(Error::Unsupported(MSG))
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (0, 0)
    }

    pub fn frame_seq(&self) -> u64 {
        0
    }

    pub fn with_latest_frame<R>(&self, f: impl FnOnce(&FrameBuffer) -> R) -> R {
        // Unreachable in practice — `open` never succeeds on Windows,
        // so no `VideoStream` ever exists. Provide a 1×1 empty buffer
        // anyway so the closure has a valid reference if some future
        // refactor lets a stub instance leak through.
        let frame = FrameBuffer {
            data: vec![0u8; 4],
            width: 1,
            height: 1,
            stride: 4,
        };
        f(&frame)
    }
}
