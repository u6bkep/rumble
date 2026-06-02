//! Adapter between damascene's `App` and `rumble-client`'s `BackendHandle`.
//!
//! Same shape as `rumble-next`'s `UiBackend`: the renderer reads `State`
//! snapshots and pushes `Command`s; tests can swap a mock in.

use rumble_client::{AudioStats, BackendEvent, Command, MeterSnapshot, OutputFrame, State, handle::BackendHandle};
use rumble_client_traits::file_transfer::TransferStatus;
use tokio::sync::mpsc;

pub trait UiBackend: 'static {
    fn state(&self) -> State;
    fn send(&self, command: Command);
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
}

impl NativeUiBackend {
    pub fn new(inner: BackendHandle<rumble_desktop::NativePlatform>) -> Self {
        let event_rx = inner.take_event_receiver();
        Self {
            inner,
            event_rx: std::sync::Mutex::new(event_rx),
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
