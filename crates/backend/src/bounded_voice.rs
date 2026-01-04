//! Bounded voice channel for handling slow connections gracefully.
//!
//! This module provides bounded channels for voice data that drop old frames
//! when the channel is full, rather than growing unbounded or blocking.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// Configuration for bounded voice channels.
#[derive(Debug, Clone, Copy)]
pub struct VoiceChannelConfig {
    /// Maximum number of voice frames to buffer before dropping old frames.
    /// Default: 50 (approx 250ms at 5ms frames, or 1 second at 20ms frames)
    pub max_frames: usize,
}

impl Default for VoiceChannelConfig {
    fn default() -> Self {
        Self {
            // 50 frames at 5ms = 250ms, at 20ms = 1 second
            // This gives some buffer for network jitter while preventing unbounded growth
            max_frames: 50,
        }
    }
}

/// Statistics for a bounded voice channel.
#[derive(Debug, Clone, Default)]
pub struct VoiceChannelStats {
    /// Total frames sent through the channel.
    pub total_sent: usize,
    /// Total frames dropped due to channel being full.
    pub dropped: usize,
    /// Total frames received from the channel.
    pub received: usize,
}

/// Sender half of a bounded voice channel.
///
/// When the channel is full, new frames overwrite the oldest frames.
/// This is the right behavior for real-time voice: we'd rather hear
/// the most recent audio than buffer arbitrarily old audio.
pub struct BoundedVoiceSender<T> {
    tx: tokio::sync::mpsc::Sender<T>,
    dropped_count: Arc<AtomicUsize>,
    total_sent: Arc<AtomicUsize>,
}

impl<T> Clone for BoundedVoiceSender<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            dropped_count: self.dropped_count.clone(),
            total_sent: self.total_sent.clone(),
        }
    }
}

impl<T> BoundedVoiceSender<T> {
    /// Try to send a voice frame, dropping it if the channel is full.
    ///
    /// Returns true if the frame was sent, false if it was dropped.
    pub fn try_send(&self, value: T) -> bool {
        self.total_sent.fetch_add(1, Ordering::Relaxed);
        match self.tx.try_send(value) {
            Ok(()) => true,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                self.dropped_count.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Send a voice frame, blocking if necessary.
    ///
    /// This will block if the channel is full. For non-blocking behavior,
    /// use `try_send` instead.
    pub async fn send(&self, value: T) -> Result<(), tokio::sync::mpsc::error::SendError<T>> {
        self.total_sent.fetch_add(1, Ordering::Relaxed);
        self.tx.send(value).await
    }

    /// Get the number of dropped frames.
    pub fn dropped_count(&self) -> usize {
        self.dropped_count.load(Ordering::Relaxed)
    }

    /// Get the total frames sent.
    pub fn total_sent(&self) -> usize {
        self.total_sent.load(Ordering::Relaxed)
    }

    /// Get channel statistics.
    pub fn stats(&self) -> VoiceChannelStats {
        VoiceChannelStats {
            total_sent: self.total_sent.load(Ordering::Relaxed),
            dropped: self.dropped_count.load(Ordering::Relaxed),
            received: 0, // Receiver has this
        }
    }
}

/// Receiver half of a bounded voice channel.
pub struct BoundedVoiceReceiver<T> {
    rx: tokio::sync::mpsc::Receiver<T>,
    received_count: Arc<AtomicUsize>,
    // Shared counters from sender
    dropped_count: Arc<AtomicUsize>,
    total_sent: Arc<AtomicUsize>,
}

impl<T> BoundedVoiceReceiver<T> {
    /// Receive a voice frame, waiting if necessary.
    pub async fn recv(&mut self) -> Option<T> {
        let result = self.rx.recv().await;
        if result.is_some() {
            self.received_count.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    /// Try to receive a voice frame without blocking.
    pub fn try_recv(&mut self) -> Option<T> {
        match self.rx.try_recv() {
            Ok(value) => {
                self.received_count.fetch_add(1, Ordering::Relaxed);
                Some(value)
            }
            Err(_) => None,
        }
    }

    /// Get the number of frames received.
    pub fn received_count(&self) -> usize {
        self.received_count.load(Ordering::Relaxed)
    }

    /// Get channel statistics.
    pub fn stats(&self) -> VoiceChannelStats {
        VoiceChannelStats {
            total_sent: self.total_sent.load(Ordering::Relaxed),
            dropped: self.dropped_count.load(Ordering::Relaxed),
            received: self.received_count.load(Ordering::Relaxed),
        }
    }
}

/// Create a bounded voice channel.
///
/// The channel will hold at most `config.max_frames` frames. When full,
/// `try_send` will drop new frames (returning false) rather than blocking
/// or growing the buffer.
pub fn bounded_voice_channel<T>(config: VoiceChannelConfig) -> (BoundedVoiceSender<T>, BoundedVoiceReceiver<T>) {
    let (tx, rx) = tokio::sync::mpsc::channel(config.max_frames);
    let dropped_count = Arc::new(AtomicUsize::new(0));
    let total_sent = Arc::new(AtomicUsize::new(0));
    let received_count = Arc::new(AtomicUsize::new(0));

    (
        BoundedVoiceSender {
            tx,
            dropped_count: dropped_count.clone(),
            total_sent: total_sent.clone(),
        },
        BoundedVoiceReceiver {
            rx,
            received_count,
            dropped_count,
            total_sent,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_bounded_channel_drops_when_full() {
        // Create a very small channel to easily test overflow
        let config = VoiceChannelConfig { max_frames: 3 };
        let (tx, mut rx) = bounded_voice_channel::<u32>(config);

        // Fill the channel
        assert!(tx.try_send(1));
        assert!(tx.try_send(2));
        assert!(tx.try_send(3));

        // Next send should drop (channel is full)
        assert!(!tx.try_send(4));
        assert!(!tx.try_send(5));

        // Check stats
        assert_eq!(tx.total_sent(), 5);
        assert_eq!(tx.dropped_count(), 2);

        // Receive should still get the first 3
        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.recv().await, Some(2));
        assert_eq!(rx.recv().await, Some(3));

        // Channel is now empty
        assert!(rx.try_recv().is_none());
        assert_eq!(rx.received_count(), 3);
    }

    #[tokio::test]
    async fn test_bounded_channel_stats() {
        let config = VoiceChannelConfig { max_frames: 10 };
        let (tx, mut rx) = bounded_voice_channel::<u32>(config);

        // Send some frames
        for i in 0..5 {
            tx.try_send(i);
        }

        // Receive some frames
        rx.recv().await;
        rx.recv().await;

        let stats = rx.stats();
        assert_eq!(stats.total_sent, 5);
        assert_eq!(stats.dropped, 0);
        assert_eq!(stats.received, 2);
    }

    #[tokio::test]
    async fn test_bounded_channel_clone_sender() {
        let config = VoiceChannelConfig { max_frames: 3 };
        let (tx1, mut rx) = bounded_voice_channel::<u32>(config);
        let tx2 = tx1.clone();

        tx1.try_send(1);
        tx2.try_send(2);
        tx1.try_send(3);

        // Both senders share the same channel
        assert!(!tx2.try_send(4)); // Should drop

        // Stats are shared
        assert_eq!(tx1.dropped_count(), 1);
        assert_eq!(tx2.dropped_count(), 1);

        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.recv().await, Some(2));
        assert_eq!(rx.recv().await, Some(3));
    }
}
