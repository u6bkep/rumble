//! Audio dumping for debugging.
//!
//! This module provides functionality to dump raw audio streams to files
//! for debugging audio issues. Each stream is written to a separate file:
//!
//! - `mic_raw.pcm` - Raw microphone input (f32 PCM, 48kHz mono)
//! - `tx_opus.bin` - Encoded Opus packets (length-prefixed)
//! - `rx_opus.bin` - Received Opus packets (length-prefixed)  
//! - `rx_decoded.pcm` - Decoded audio before playback (f32 PCM, 48kHz mono)
//!
//! PCM files can be imported into Audacity with:
//! - File > Import > Raw Data
//! - Encoding: 32-bit float
//! - Byte order: Little-endian
//! - Channels: 1 (Mono)
//! - Sample rate: 48000 Hz
//!
//! Opus files are length-prefixed: each packet is preceded by a 4-byte little-endian
//! length, then the raw Opus bytes.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing::{debug, error, info};

/// Configuration for audio dumping.
#[derive(Debug, Clone, Default)]
pub struct AudioDumpConfig {
    /// Enable audio dumping.
    pub enabled: bool,
    /// Directory to write dump files to.
    pub output_dir: PathBuf,
}

impl AudioDumpConfig {
    /// Create a new config that dumps to the specified directory.
    pub fn new(output_dir: impl Into<PathBuf>) -> Self {
        Self {
            enabled: true,
            output_dir: output_dir.into(),
        }
    }
    
    /// Create a disabled config (no dumping).
    pub fn disabled() -> Self {
        Self::default()
    }
    
    /// Check if audio dumping should be enabled via environment variable.
    /// 
    /// Set `RUMBLE_AUDIO_DUMP_DIR=/path/to/dir` to enable audio dumping.
    /// If the env var is set, returns `Some(config)` with that directory.
    /// If not set or empty, returns `None`.
    pub fn from_env() -> Option<Self> {
        std::env::var("RUMBLE_AUDIO_DUMP_DIR").ok()
            .filter(|s| !s.is_empty())
            .map(|dir| Self::new(dir))
    }
}

/// Handle for writing audio dump files.
/// 
/// Thread-safe - can be cloned and shared across threads.
#[derive(Clone)]
pub struct AudioDumper {
    inner: Arc<Mutex<AudioDumperInner>>,
}

struct AudioDumperInner {
    config: AudioDumpConfig,
    mic_raw: Option<BufWriter<File>>,
    tx_opus: Option<BufWriter<File>>,
    rx_opus: Option<BufWriter<File>>,
    rx_decoded: Option<BufWriter<File>>,
    playback: Option<BufWriter<File>>,
}

impl AudioDumper {
    /// Create a new audio dumper with the given config.
    pub fn new(config: AudioDumpConfig) -> Self {
        let inner = if config.enabled {
            // Create output directory if it doesn't exist
            if let Err(e) = std::fs::create_dir_all(&config.output_dir) {
                error!("Failed to create audio dump directory {:?}: {}", config.output_dir, e);
                AudioDumperInner {
                    config: AudioDumpConfig::disabled(),
                    mic_raw: None,
                    tx_opus: None,
                    rx_opus: None,
                    rx_decoded: None,
                    playback: None,
                }
            } else {
                info!("Audio dumping enabled, writing to {:?}", config.output_dir);
                
                let mic_raw = Self::open_file(&config.output_dir, "mic_raw.pcm");
                let tx_opus = Self::open_file(&config.output_dir, "tx_opus.bin");
                let rx_opus = Self::open_file(&config.output_dir, "rx_opus.bin");
                let rx_decoded = Self::open_file(&config.output_dir, "rx_decoded.pcm");
                let playback = Self::open_file(&config.output_dir, "playback.pcm");
                
                AudioDumperInner {
                    config,
                    mic_raw,
                    tx_opus,
                    rx_opus,
                    rx_decoded,
                    playback,
                }
            }
        } else {
            AudioDumperInner {
                config,
                mic_raw: None,
                tx_opus: None,
                rx_opus: None,
                rx_decoded: None,
                playback: None,
            }
        };
        
        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }
    
    /// Create a disabled dumper (no-op).
    pub fn disabled() -> Self {
        Self::new(AudioDumpConfig::disabled())
    }
    
    fn open_file(dir: &PathBuf, name: &str) -> Option<BufWriter<File>> {
        let path = dir.join(name);
        match File::create(&path) {
            Ok(f) => {
                debug!("Opened audio dump file: {:?}", path);
                Some(BufWriter::new(f))
            }
            Err(e) => {
                error!("Failed to create audio dump file {:?}: {}", path, e);
                None
            }
        }
    }
    
    /// Check if dumping is enabled.
    pub fn is_enabled(&self) -> bool {
        self.inner.lock().unwrap().config.enabled
    }
    
    /// Write raw microphone samples (f32 PCM).
    pub fn write_mic_raw(&self, samples: &[f32]) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(ref mut f) = inner.mic_raw {
                for &sample in samples {
                    if f.write_all(&sample.to_le_bytes()).is_err() {
                        break;
                    }
                }
            }
        }
    }
    
    /// Write encoded Opus packet (length-prefixed).
    pub fn write_tx_opus(&self, opus_data: &[u8]) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(ref mut f) = inner.tx_opus {
                let len = opus_data.len() as u32;
                let _ = f.write_all(&len.to_le_bytes());
                let _ = f.write_all(opus_data);
            }
        }
    }
    
    /// Write received Opus packet (length-prefixed).
    pub fn write_rx_opus(&self, opus_data: &[u8]) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(ref mut f) = inner.rx_opus {
                let len = opus_data.len() as u32;
                let _ = f.write_all(&len.to_le_bytes());
                let _ = f.write_all(opus_data);
            }
        }
    }
    
    /// Write decoded audio samples (f32 PCM).
    pub fn write_rx_decoded(&self, samples: &[f32]) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(ref mut f) = inner.rx_decoded {
                for &sample in samples {
                    if f.write_all(&sample.to_le_bytes()).is_err() {
                        break;
                    }
                }
            }
        }
    }
    
    /// Write mixed audio samples that are sent to playback (f32 PCM).
    /// This captures the exact audio that goes to the cpal output buffer.
    pub fn write_playback(&self, samples: &[f32]) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(ref mut f) = inner.playback {
                for &sample in samples {
                    if f.write_all(&sample.to_le_bytes()).is_err() {
                        break;
                    }
                }
            }
        }
    }
    
    /// Flush all buffers.
    pub fn flush(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(ref mut f) = inner.mic_raw {
                let _ = f.flush();
            }
            if let Some(ref mut f) = inner.tx_opus {
                let _ = f.flush();
            }
            if let Some(ref mut f) = inner.rx_opus {
                let _ = f.flush();
            }
            if let Some(ref mut f) = inner.rx_decoded {
                let _ = f.flush();
            }
            if let Some(ref mut f) = inner.playback {
                let _ = f.flush();
            }
        }
    }
}

impl Drop for AudioDumperInner {
    fn drop(&mut self) {
        // Flush on drop
        if let Some(ref mut f) = self.mic_raw {
            let _ = f.flush();
        }
        if let Some(ref mut f) = self.tx_opus {
            let _ = f.flush();
        }
        if let Some(ref mut f) = self.rx_opus {
            let _ = f.flush();
        }
        if let Some(ref mut f) = self.rx_decoded {
            let _ = f.flush();
        }
        if let Some(ref mut f) = self.playback {
            let _ = f.flush();
        }
        if self.config.enabled {
            info!("Audio dump files closed");
        }
    }
}
