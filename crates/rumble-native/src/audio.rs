//! Audio I/O implementation using cpal.
//!
//! TODO: Implement CpalAudioBackend wrapping cpal host/devices.

/// Audio backend using the cpal library.
pub struct CpalAudioBackend;

/// Capture stream wrapping a cpal input stream.
pub struct CpalCaptureStream;

/// Playback stream wrapping a cpal output stream.
pub struct CpalPlaybackStream;
