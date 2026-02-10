//! Waveform generators for sound effects.
//!
//! Pure functions that generate PCM samples at 48kHz mono.
//! No external dependencies required.

use std::f32::consts::PI;

pub const SAMPLE_RATE: u32 = 48000;

#[derive(Debug, Clone, Copy)]
pub enum Waveform {
    Sine,
    Square,
    Sawtooth,
    Triangle,
}

#[derive(Debug, Clone, Copy)]
pub struct Envelope {
    pub attack: f32,
    pub decay: f32,
    pub sustain: f32,
    pub release: f32,
}

impl Envelope {
    pub fn percussive(attack: f32, decay: f32) -> Self {
        Self {
            attack,
            decay,
            sustain: 0.0,
            release: 0.0,
        }
    }

    /// Compute amplitude at time `t` for a note of given `note_duration`.
    pub fn amplitude(&self, t: f32, note_duration: f32) -> f32 {
        if t < 0.0 {
            return 0.0;
        }

        let release_start = note_duration;

        if t < self.attack {
            // Attack phase: ramp from 0 to 1
            if self.attack > 0.0 { t / self.attack } else { 1.0 }
        } else if t < self.attack + self.decay {
            // Decay phase: ramp from 1 to sustain
            let decay_t = t - self.attack;
            if self.decay > 0.0 {
                1.0 - (1.0 - self.sustain) * (decay_t / self.decay)
            } else {
                self.sustain
            }
        } else if t < release_start {
            // Sustain phase
            self.sustain
        } else {
            // Release phase: ramp from sustain to 0
            let release_t = t - release_start;
            if self.release > 0.0 && release_t < self.release {
                self.sustain * (1.0 - release_t / self.release)
            } else if self.release <= 0.0 {
                0.0
            } else {
                0.0
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Tone {
    pub waveform: Waveform,
    pub frequency: f32,
    pub end_frequency: Option<f32>,
    pub amplitude: f32,
    pub envelope: Envelope,
    pub duration: f32,
    pub delay: f32,
}

fn waveform_sample(waveform: Waveform, phase: f32) -> f32 {
    match waveform {
        Waveform::Sine => (2.0 * PI * phase).sin(),
        Waveform::Square => {
            if (2.0 * PI * phase).sin() >= 0.0 {
                1.0
            } else {
                -1.0
            }
        }
        Waveform::Sawtooth => 2.0 * (phase - (phase + 0.5).floor()),
        Waveform::Triangle => 2.0 * (2.0 * (phase - (phase + 0.5).floor())).abs() - 1.0,
    }
}

pub fn generate_tone(tone: &Tone, sample_rate: u32) -> Vec<f32> {
    let total_duration = tone.delay + tone.duration + tone.envelope.release;
    let total_samples = (total_duration * sample_rate as f32).ceil() as usize;
    let mut samples = vec![0.0f32; total_samples];

    let delay_samples = (tone.delay * sample_rate as f32).ceil() as usize;

    for i in 0..total_samples {
        if i < delay_samples {
            continue;
        }

        let t = (i - delay_samples) as f32 / sample_rate as f32;
        if t > tone.duration + tone.envelope.release {
            break;
        }

        // Compute frequency (with optional sweep)
        let freq = match tone.end_frequency {
            Some(end_freq) if tone.duration > 0.0 => {
                let progress = (t / tone.duration).min(1.0);
                tone.frequency + (end_freq - tone.frequency) * progress
            }
            _ => tone.frequency,
        };

        let phase = freq * t;
        let wave = waveform_sample(tone.waveform, phase);
        let env = tone.envelope.amplitude(t, tone.duration);

        samples[i] = wave * env * tone.amplitude;
    }

    samples
}

pub fn mix_tones(tones: &[Tone], sample_rate: u32) -> Vec<f32> {
    if tones.is_empty() {
        return Vec::new();
    }

    // Generate each tone
    let generated: Vec<Vec<f32>> = tones.iter().map(|t| generate_tone(t, sample_rate)).collect();

    // Find max length
    let max_len = generated.iter().map(|g| g.len()).max().unwrap_or(0);

    // Sum all tones
    let mut mixed = vec![0.0f32; max_len];
    for tone_samples in &generated {
        for (i, &sample) in tone_samples.iter().enumerate() {
            mixed[i] += sample;
        }
    }

    // Clamp to [-1.0, 1.0]
    for sample in mixed.iter_mut() {
        *sample = sample.clamp(-1.0, 1.0);
    }

    mixed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sine_basic() {
        let tone = Tone {
            waveform: Waveform::Sine,
            frequency: 440.0,
            end_frequency: None,
            amplitude: 1.0,
            envelope: Envelope::percussive(0.001, 0.01),
            duration: 0.01,
            delay: 0.0,
        };
        let samples = generate_tone(&tone, 48000);
        assert!(!samples.is_empty());
        // Check samples are within range
        for &s in &samples {
            assert!(s >= -1.0 && s <= 1.0);
        }
    }

    #[test]
    fn test_mix_tones() {
        let tones = vec![
            Tone {
                waveform: Waveform::Sine,
                frequency: 440.0,
                end_frequency: None,
                amplitude: 0.5,
                envelope: Envelope::percussive(0.001, 0.01),
                duration: 0.01,
                delay: 0.0,
            },
            Tone {
                waveform: Waveform::Sine,
                frequency: 880.0,
                end_frequency: None,
                amplitude: 0.5,
                envelope: Envelope::percussive(0.001, 0.01),
                duration: 0.01,
                delay: 0.0,
            },
        ];
        let mixed = mix_tones(&tones, 48000);
        assert!(!mixed.is_empty());
        for &s in &mixed {
            assert!(s >= -1.0 && s <= 1.0);
        }
    }

    #[test]
    fn test_frequency_sweep() {
        let tone = Tone {
            waveform: Waveform::Sawtooth,
            frequency: 600.0,
            end_frequency: Some(200.0),
            amplitude: 0.3,
            envelope: Envelope::percussive(0.01, 0.24),
            duration: 0.25,
            delay: 0.0,
        };
        let samples = generate_tone(&tone, 48000);
        assert!(!samples.is_empty());
    }

    #[test]
    fn test_delay_offset() {
        let tone = Tone {
            waveform: Waveform::Sine,
            frequency: 440.0,
            end_frequency: None,
            amplitude: 1.0,
            envelope: Envelope::percussive(0.001, 0.01),
            duration: 0.01,
            delay: 0.05,
        };
        let samples = generate_tone(&tone, 48000);
        // First ~2400 samples (50ms) should be silence
        let delay_samples = (0.05 * 48000.0) as usize;
        for &s in &samples[..delay_samples] {
            assert_eq!(s, 0.0);
        }
    }
}
