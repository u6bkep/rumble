//! Adapter between damascene's `App` and `rumble-client`'s `BackendHandle`.
//!
//! Same shape as `rumble-next`'s `UiBackend`: the renderer reads `State`
//! snapshots and pushes `Command`s; tests can swap a mock in.

use std::sync::Arc;

use rumble_client::{AudioStats, BackendEvent, Command, MeterSnapshot, OutputFrame, State, handle::BackendHandle};
use rumble_client_traits::file_transfer::TransferStatus;
use tokio::sync::mpsc;

pub trait UiBackend: 'static {
    fn state(&self) -> State;
    fn send(&self, command: Command);
    /// Return a clonable callback that schedules a frame when invoked.
    /// Spawn sites call this before `runtime.spawn` and move the clone
    /// into the async closure, so completing a picker or video-open task
    /// immediately wakes the host's frame scheduler without waiting for
    /// user input.  The default no-op is safe for mock/test backends
    /// that have no event loop to poke.
    fn repaint_arc(&self) -> Arc<dyn Fn() + Send + Sync> {
        Arc::new(|| {})
    }
    /// Live audio meter snapshot. Defaults to `Unmeasured` so test
    /// backends without a running audio task compile without extra
    /// plumbing; fixtures that want a representative meter override
    /// this to return a canned snapshot.
    fn meter(&self) -> MeterSnapshot {
        MeterSnapshot::default()
    }
    /// Audio stats roll-up. Defaults to the all-zero roll-up so test
    /// backends compile without plumbing; fixtures override it.
    fn stats(&self) -> AudioStats {
        AudioStats::default()
    }
    /// Live per-stage pipeline outputs (VAD probability, gate state).
    /// Defaults to an empty frame so test/mock backends compile without
    /// plumbing; fixtures override it to exercise the output meters.
    fn outputs(&self) -> OutputFrame {
        OutputFrame::default()
    }
    /// Snapshot of file-transfer state. The default returns an empty
    /// vec so test backends without a transfer plugin compile without
    /// extra plumbing.
    fn transfers(&self) -> Vec<TransferStatus> {
        Vec::new()
    }
    /// Drain pending backend events produced since the last frame.
    /// The default returns an empty vec for mock backends.
    fn drain_events(&self) -> Vec<BackendEvent> {
        Vec::new()
    }
}

pub struct NativeUiBackend {
    inner: BackendHandle<rumble_desktop::NativePlatform>,
    event_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<BackendEvent>>>,
    /// Repaint callback shared with `BackendHandle` — poking it schedules
    /// a new frame from any thread.  Stored here so spawn sites can move
    /// a clone into async tasks and wake the host on completion.
    repaint: Arc<dyn Fn() + Send + Sync>,
}

impl NativeUiBackend {
    pub fn new(inner: BackendHandle<rumble_desktop::NativePlatform>, repaint: Arc<dyn Fn() + Send + Sync>) -> Self {
        let event_rx = inner.take_event_receiver();
        Self {
            inner,
            event_rx: std::sync::Mutex::new(event_rx),
            repaint,
        }
    }

    pub fn inner(&self) -> &BackendHandle<rumble_desktop::NativePlatform> {
        &self.inner
    }
}

impl UiBackend for NativeUiBackend {
    fn state(&self) -> State {
        self.inner.state()
    }

    fn send(&self, command: Command) {
        self.inner.send(command);
    }

    fn repaint_arc(&self) -> Arc<dyn Fn() + Send + Sync> {
        self.repaint.clone()
    }

    fn meter(&self) -> MeterSnapshot {
        self.inner.meter()
    }

    fn stats(&self) -> AudioStats {
        self.inner.stats()
    }

    fn outputs(&self) -> OutputFrame {
        self.inner.outputs()
    }

    fn transfers(&self) -> Vec<TransferStatus> {
        self.inner.transfers()
    }

    fn drain_events(&self) -> Vec<BackendEvent> {
        let mut rx = match self.event_rx.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let Some(rx) = rx.as_mut() else {
            return Vec::new();
        };
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        events
    }
}
