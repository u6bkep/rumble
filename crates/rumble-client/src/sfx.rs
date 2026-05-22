//! Sound effect definitions for notification events.
//!
//! Uses the synth module to generate short notification sounds at startup,
//! cached in memory for instant playback.

use crate::synth::{Envelope, SAMPLE_RATE, Tone, Waveform, mix_tones};
use std::collections::HashMap;

pub use rumble_protocol::SfxKind;

pub struct SfxLibrary {
    sounds: HashMap<SfxKind, Vec<f32>>,
}

impl Default for SfxLibrary {
    fn default() -> Self {
        Self::new()
    }
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

        // Disconnect: gentle sawtooth sweep down — kept soft (lower level,
        // eased attack) so a routine disconnect isn't jarring.
        sounds.insert(
            SfxKind::Disconnect,
            mix_tones(
                &[Tone {
                    waveform: Waveform::Sawtooth,
                    frequency: 600.0,
                    end_frequency: Some(200.0),
                    amplitude: 0.2,
                    envelope: Envelope::percussive(0.02, 0.23),
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

        // PrivateMessage: two-note rising ping (A5 -> D6) to distinguish a
        // DM from a normal room message.
        sounds.insert(
            SfxKind::PrivateMessage,
            mix_tones(
                &[
                    Tone {
                        waveform: Waveform::Sine,
                        frequency: 880.0,
                        end_frequency: None,
                        amplitude: 0.32,
                        envelope: Envelope::percussive(0.004, 0.066),
                        duration: 0.07,
                        delay: 0.0,
                    },
                    Tone {
                        waveform: Waveform::Sine,
                        frequency: 1175.0,
                        end_frequency: None,
                        amplitude: 0.32,
                        envelope: Envelope::percussive(0.004, 0.086),
                        duration: 0.09,
                        delay: 0.07,
                    },
                ],
                SAMPLE_RATE,
            ),
        );

        // Deafen: two descending low blips (all audio going dark).
        sounds.insert(
            SfxKind::Deafen,
            mix_tones(
                &[
                    Tone {
                        waveform: Waveform::Sine,
                        frequency: 320.0,
                        end_frequency: None,
                        amplitude: 0.3,
                        envelope: Envelope::percussive(0.002, 0.028),
                        duration: 0.03,
                        delay: 0.0,
                    },
                    Tone {
                        waveform: Waveform::Sine,
                        frequency: 220.0,
                        end_frequency: None,
                        amplitude: 0.3,
                        envelope: Envelope::percussive(0.002, 0.038),
                        duration: 0.04,
                        delay: 0.04,
                    },
                ],
                SAMPLE_RATE,
            ),
        );

        // Undeafen: two ascending blips (audio coming back).
        sounds.insert(
            SfxKind::Undeafen,
            mix_tones(
                &[
                    Tone {
                        waveform: Waveform::Sine,
                        frequency: 400.0,
                        end_frequency: None,
                        amplitude: 0.3,
                        envelope: Envelope::percussive(0.002, 0.028),
                        duration: 0.03,
                        delay: 0.0,
                    },
                    Tone {
                        waveform: Waveform::Sine,
                        frequency: 600.0,
                        end_frequency: None,
                        amplitude: 0.3,
                        envelope: Envelope::percussive(0.002, 0.038),
                        duration: 0.04,
                        delay: 0.04,
                    },
                ],
                SAMPLE_RATE,
            ),
        );

        // SelfChannelJoin: confident two-note rising (G4 -> C5) for your own
        // channel switch — fuller and lower than the remote UserJoin chime.
        sounds.insert(
            SfxKind::SelfChannelJoin,
            mix_tones(
                &[
                    Tone {
                        waveform: Waveform::Sine,
                        frequency: 392.0,
                        end_frequency: None,
                        amplitude: 0.4,
                        envelope: Envelope::percussive(0.005, 0.065),
                        duration: 0.07,
                        delay: 0.0,
                    },
                    Tone {
                        waveform: Waveform::Sine,
                        frequency: 523.0,
                        end_frequency: None,
                        amplitude: 0.4,
                        envelope: Envelope::percussive(0.005, 0.085),
                        duration: 0.09,
                        delay: 0.07,
                    },
                ],
                SAMPLE_RATE,
            ),
        );

        // SelfChannelMoved: neutral triangle double-tap — you didn't choose
        // this move (an admin or another user relocated you).
        sounds.insert(
            SfxKind::SelfChannelMoved,
            mix_tones(
                &[
                    Tone {
                        waveform: Waveform::Triangle,
                        frequency: 494.0,
                        end_frequency: None,
                        amplitude: 0.32,
                        envelope: Envelope::percussive(0.004, 0.056),
                        duration: 0.06,
                        delay: 0.0,
                    },
                    Tone {
                        waveform: Waveform::Triangle,
                        frequency: 494.0,
                        end_frequency: None,
                        amplitude: 0.32,
                        envelope: Envelope::percussive(0.004, 0.066),
                        duration: 0.07,
                        delay: 0.09,
                    },
                ],
                SAMPLE_RATE,
            ),
        );

        // ServerMute: low square-wave double tone — an admin muted you
        // server-side (also fires when you move to an AFK channel), so it's
        // kept gentle: lower level and a softer attack to round off the
        // square's buzz.
        sounds.insert(
            SfxKind::ServerMute,
            mix_tones(
                &[
                    Tone {
                        waveform: Waveform::Square,
                        frequency: 250.0,
                        end_frequency: None,
                        amplitude: 0.14,
                        envelope: Envelope::percussive(0.008, 0.052),
                        duration: 0.06,
                        delay: 0.0,
                    },
                    Tone {
                        waveform: Waveform::Square,
                        frequency: 250.0,
                        end_frequency: None,
                        amplitude: 0.14,
                        envelope: Envelope::percussive(0.008, 0.072),
                        duration: 0.08,
                        delay: 0.10,
                    },
                ],
                SAMPLE_RATE,
            ),
        );

        // Kicked: alarming descending sawtooth sweep — longer and harsher
        // than a plain Disconnect.
        sounds.insert(
            SfxKind::Kicked,
            mix_tones(
                &[Tone {
                    waveform: Waveform::Sawtooth,
                    frequency: 500.0,
                    end_frequency: Some(120.0),
                    amplitude: 0.32,
                    envelope: Envelope::percussive(0.005, 0.395),
                    duration: 0.40,
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
