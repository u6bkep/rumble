//! Adapter between aetna's `App` and `rumble-client`'s `BackendHandle`.
//!
//! Same shape as `rumble-next`'s `UiBackend`: the renderer reads `State`
//! snapshots and pushes `Command`s; tests can swap a mock in.

use rumble_client::handle::BackendHandle;
use rumble_client_traits::file_transfer::TransferStatus;
use rumble_protocol::{Command, State};

pub trait UiBackend: 'static {
    fn state(&self) -> State;
    fn send(&self, command: Command);
    /// Snapshot of file-transfer state. The default returns an empty
    /// vec so test backends without a transfer plugin compile without
    /// extra plumbing.
    fn transfers(&self) -> Vec<TransferStatus> {
        Vec::new()
    }
}

pub struct NativeUiBackend {
    inner: BackendHandle<rumble_desktop::NativePlatform>,
}

impl NativeUiBackend {
    pub fn new(inner: BackendHandle<rumble_desktop::NativePlatform>) -> Self {
        Self { inner }
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

    fn transfers(&self) -> Vec<TransferStatus> {
        self.inner.transfers()
    }
}
