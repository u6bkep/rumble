//! UI-facing backend boundary.
//!
//! `rumble-next` renders a `State` snapshot and emits `Command`s. The
//! production backend adapts `rumble_client::BackendHandle`; tests can
//! use `MockBackend` to arrange UI state without starting networking or
//! the audio pipeline.

use rumble_client::{AudioStats, Command, MeterSnapshot, State, handle::BackendHandle};
use rumble_client_traits::file_transfer::{TransferId, TransferStatus};
use std::path::PathBuf;

#[cfg(feature = "test-harness")]
use std::sync::{Arc, Mutex, RwLock};

pub trait UiBackend: 'static {
    fn state(&self) -> State;
    fn send(&self, command: Command);
    fn update_state<R>(&self, f: impl FnOnce(&mut State) -> R) -> R;

    /// Live meter snapshot. Default returns the empty snapshot so test
    /// backends don't have to provide one.
    fn meter(&self) -> MeterSnapshot {
        MeterSnapshot::default()
    }

    /// Audio stats roll-up. Default returns the all-zero roll-up.
    fn stats(&self) -> AudioStats {
        AudioStats::default()
    }

    /// Snapshot the current set of file transfers. Returns an empty
    /// vec when not connected or when no plugin is installed.
    fn transfers(&self) -> Vec<TransferStatus> {
        Vec::new()
    }

    /// Cancel an in-flight transfer. Errors when no plugin is
    /// installed (i.e. while disconnected) or when the cancel itself
    /// fails — UI should surface the message via a toast.
    fn cancel_transfer(&self, _id: &TransferId, _delete_files: bool) -> anyhow::Result<()> {
        anyhow::bail!("file transfer plugin not available")
    }

    /// Local path of a transfer once it has data on disk. `None`
    /// while a download is still pending bytes from the server.
    fn transfer_file_path(&self, _id: &TransferId) -> Option<PathBuf> {
        None
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

    fn update_state<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let mut state = self.inner.state_mut();
        f(&mut state)
    }

    fn meter(&self) -> MeterSnapshot {
        self.inner.meter()
    }

    fn stats(&self) -> AudioStats {
        self.inner.stats()
    }

    fn transfers(&self) -> Vec<TransferStatus> {
        self.inner.transfers()
    }

    fn cancel_transfer(&self, id: &TransferId, delete_files: bool) -> anyhow::Result<()> {
        self.inner.cancel_transfer(id, delete_files)
    }

    fn transfer_file_path(&self, id: &TransferId) -> Option<PathBuf> {
        self.inner.transfer_file_path(id)
    }
}

#[cfg(feature = "test-harness")]
#[derive(Clone, Default)]
pub struct MockBackend {
    state: Arc<RwLock<State>>,
    commands: Arc<Mutex<Vec<Command>>>,
    transfers: Arc<RwLock<Vec<TransferStatus>>>,
}

#[cfg(feature = "test-harness")]
impl MockBackend {
    pub fn new(state: State) -> Self {
        Self {
            state: Arc::new(RwLock::new(state)),
            commands: Arc::new(Mutex::new(Vec::new())),
            transfers: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub fn commands(&self) -> Vec<Command> {
        self.commands.lock().expect("mock commands poisoned").clone()
    }

    pub fn take_commands(&self) -> Vec<Command> {
        std::mem::take(&mut *self.commands.lock().expect("mock commands poisoned"))
    }

    /// Replace the canned transfer list returned by `transfers()`. Used
    /// by tests that need the chat to show a "completed" file offer
    /// without standing up a real plugin.
    pub fn set_transfers(&self, transfers: Vec<TransferStatus>) {
        *self.transfers.write().expect("mock transfers poisoned") = transfers;
    }
}

#[cfg(feature = "test-harness")]
impl UiBackend for MockBackend {
    fn state(&self) -> State {
        self.state.read().expect("mock state poisoned").clone()
    }

    fn send(&self, command: Command) {
        self.commands.lock().expect("mock commands poisoned").push(command);
    }

    fn update_state<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let mut state = self.state.write().expect("mock state poisoned");
        f(&mut state)
    }

    fn transfers(&self) -> Vec<TransferStatus> {
        self.transfers.read().expect("mock transfers poisoned").clone()
    }
}
