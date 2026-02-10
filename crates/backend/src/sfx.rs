//! Sound effect definitions for notification events.
//!
//! Uses the synth module to generate short notification sounds at startup,
//! cached in memory for instant playback.

use crate::synth::{Envelope, SAMPLE_RATE, Tone, Waveform, mix_tones};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SfxKind {
    UserJoin,
    UserLeave,
    Connect,
    Disconnect,
    Mute,
    Unmute,
    Message,
}

impl SfxKind {
    pub fn all() -> &'static [SfxKind] {
        &[
            SfxKind::UserJoin,
            SfxKind::UserLeave,
            SfxKind::Connect,
            SfxKind::Disconnect,
            SfxKind::Mute,
            SfxKind::Unmute,
            SfxKind::Message,
        ]
    }

    pub fn label(&self) -> &'static str {
        match self {
            SfxKind::UserJoin => "User Join",
            SfxKind::UserLeave => "User Leave",
            SfxKind::Connect => "Connect",
            SfxKind::Disconnect => "Disconnect",
            SfxKind::Mute => "Mute",
            SfxKind::Unmute => "Unmute",
            SfxKind::Message => "Message",
        }
    }
}

pub struct SfxLibrary {
    sounds: HashMap<SfxKind, Vec<f32>>,
}

impl SfxLibrary {
    pub fn new() -> Self {
        let mut sounds = HashMap::new();

        // UserJoin: two-note rising chime (C5 + E5)
        sounds.insert(
            SfxKind::UserJoin,
            mix_tones(
                &[
                    Tone {
                        waveform: Waveform::Sine,
                        frequency: 523.0,
                        end_frequency: None,
                        amplitude: 0.4,
                        envelope: Envelope::percussive(0.005, 0.075),
                        duration: 0.08,
                        delay: 0.0,
                    },
                    Tone {
                        waveform: Waveform::Sine,
                        frequency: 659.0,
                        end_frequency: None,
                        amplitude: 0.4,
                        envelope: Envelope::percussive(0.005, 0.075),
                        duration: 0.08,
                        delay: 0.08,
                    },
                ],
                SAMPLE_RATE,
            ),
        );

        // UserLeave: two-note falling (E5 -> A4)
        sounds.insert(
            SfxKind::UserLeave,
            mix_tones(
                &[
                    Tone {
                        waveform: Waveform::Sine,
                        frequency: 659.0,
                        end_frequency: None,
                        amplitude: 0.35,
                        envelope: Envelope::percussive(0.005, 0.075),
                        duration: 0.08,
                        delay: 0.0,
                    },
                    Tone {
                        waveform: Waveform::Sine,
                        frequency: 440.0,
                        end_frequency: None,
                        amplitude: 0.35,
                        envelope: Envelope::percussive(0.005, 0.075),
                        duration: 0.08,
                        delay: 0.08,
                    },
                ],
                SAMPLE_RATE,
            ),
        );

        // Connect: three-note ascending arpeggio
        sounds.insert(
            SfxKind::Connect,
            mix_tones(
                &[
                    Tone {
                        waveform: Waveform::Sine,
                        frequency: 440.0,
                        end_frequency: None,
                        amplitude: 0.4,
                        envelope: Envelope::percussive(0.005, 0.055),
                        duration: 0.06,
                        delay: 0.0,
                    },
                    Tone {
                        waveform: Waveform::Sine,
                        frequency: 554.0,
                        end_frequency: None,
                        amplitude: 0.4,
                        envelope: Envelope::percussive(0.005, 0.055),
                        duration: 0.06,
                        delay: 0.07,
                    },
                    Tone {
                        waveform: Waveform::Sine,
                        frequency: 659.0,
                        end_frequency: None,
                        amplitude: 0.4,
                        envelope: Envelope::percussive(0.005, 0.095),
                        duration: 0.10,
                        delay: 0.14,
                    },
                ],
                SAMPLE_RATE,
            ),
        );

        // Disconnect: sawtooth sweep down
        sounds.insert(
            SfxKind::Disconnect,
            mix_tones(
                &[Tone {
                    waveform: Waveform::Sawtooth,
                    frequency: 600.0,
                    end_frequency: Some(200.0),
                    amplitude: 0.3,
                    envelope: Envelope::percussive(0.01, 0.24),
                    duration: 0.25,
                    delay: 0.0,
                }],
                SAMPLE_RATE,
            ),
        );

        // Mute: short low blip
        sounds.insert(
            SfxKind::Mute,
            mix_tones(
                &[Tone {
                    waveform: Waveform::Sine,
                    frequency: 300.0,
                    end_frequency: None,
                    amplitude: 0.3,
                    envelope: Envelope::percussive(0.002, 0.028),
                    duration: 0.03,
                    delay: 0.0,
                }],
                SAMPLE_RATE,
            ),
        );

        // Unmute: short high blip
        sounds.insert(
            SfxKind::Unmute,
            mix_tones(
                &[Tone {
                    waveform: Waveform::Sine,
                    frequency: 500.0,
                    end_frequency: None,
                    amplitude: 0.3,
                    envelope: Envelope::percussive(0.002, 0.028),
                    duration: 0.03,
                    delay: 0.0,
                }],
                SAMPLE_RATE,
            ),
        );

        // Message: short high ping
        sounds.insert(
            SfxKind::Message,
            mix_tones(
                &[Tone {
                    waveform: Waveform::Sine,
                    frequency: 880.0,
                    end_frequency: None,
                    amplitude: 0.35,
                    envelope: Envelope::percussive(0.005, 0.095),
                    duration: 0.10,
                    delay: 0.0,
                }],
                SAMPLE_RATE,
            ),
        );

        Self { sounds }
    }

    pub fn get(&self, kind: SfxKind) -> Option<&[f32]> {
        self.sounds.get(&kind).map(|v| v.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_library_has_all_sounds() {
        let lib = SfxLibrary::new();
        for kind in SfxKind::all() {
            assert!(lib.get(*kind).is_some(), "Missing sound for {:?}", kind);
            assert!(!lib.get(*kind).unwrap().is_empty(), "Empty sound for {:?}", kind);
        }
    }
}
