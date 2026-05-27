//! Audio constants and types for voice communication.
//!
//! The concrete audio I/O implementations (capture streams, playback streams,
//! device enumeration) have moved to `rumble-desktop::audio` behind the
//! `AudioBackend` trait. This module retains shared constants used across
//! the backend.

/// Audio sample rate used for voice communication.
/// 48kHz is the native rate for Opus codec.
pub const SAMPLE_RATE: u32 = 48000;

/// Number of channels (mono for voice).
pub const CHANNELS: u16 = 1;

pub use crate::events::AudioDeviceInfo;
