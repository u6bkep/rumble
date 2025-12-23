//! Backend handle for UI integration.
//!
//! The `BackendHandle` provides a clean interface for UI code to interact with
//! the backend. It manages the async runtime, client connection, and provides
//! synchronous command/event methods suitable for immediate-mode UI frameworks.
//!
//! # Usage Patterns
//!
//! ## Polling (Immediate-mode UIs like egui)
//! ```ignore
//! let mut handle = BackendHandle::new();
//! // In your update loop:
//! while let Some(event) = handle.poll_event() {
//!     match event {
//!         BackendEvent::ChatReceived { sender, text } => { /* handle */ }
//!         _ => {}
//!     }
//! }
//! ```
//!
//! ## Callback-based (Reactive/MVVM frameworks like Android)
//! ```ignore
//! let handle = BackendHandle::builder()
//!     .on_event(|event| {
//!         // Update your view model here
//!         match event {
//!             BackendEvent::StateUpdated { state } => update_ui(state),
//!             _ => {}
//!         }
//!     })
//!     .build();
//! ```

use crate::events::{BackendCommand, BackendEvent, ConnectionState};
use crate::{Client, VoiceDatagram};
use api::proto;
use std::sync::mpsc;
use std::sync::Arc;
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{error, info};

/// Type alias for event callbacks.
/// 
/// The callback receives each `BackendEvent` as it occurs. It must be
/// `Send + Sync + 'static` to work across threads.
pub type EventCallback = Arc<dyn Fn(BackendEvent) + Send + Sync>;

/// Builder for creating a `BackendHandle` with custom configuration.
/// 
/// # Example
/// ```ignore
/// let handle = BackendHandle::builder()
///     .on_event(|event| println!("Got event: {:?}", event))
///     .on_repaint(|| request_ui_refresh())
///     .build();
/// ```
pub struct BackendHandleBuilder {
    repaint_callback: Option<Arc<dyn Fn() + Send + Sync>>,
    event_callback: Option<EventCallback>,
}

impl Default for BackendHandleBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl BackendHandleBuilder {
    /// Create a new builder with default settings.
    pub fn new() -> Self {
        Self {
            repaint_callback: None,
            event_callback: None,
        }
    }
    
    /// Set a callback to be invoked when the UI should repaint.
    /// 
    /// This is useful for immediate-mode UIs that need to know when
    /// new events are available.
    pub fn on_repaint<F>(mut self, callback: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.repaint_callback = Some(Arc::new(callback));
        self
    }
    
    /// Set a callback to be invoked for each event.
    /// 
    /// This is the recommended approach for reactive/MVVM frameworks.
    /// The callback will be called on a background thread, so you may
    /// need to dispatch to your UI thread.
    /// 
    /// # Example
    /// ```ignore
    /// // Android-style with a dispatcher
    /// let dispatcher = ui_dispatcher.clone();
    /// handle_builder.on_event(move |event| {
    ///     dispatcher.post(|| update_view_model(event));
    /// });
    /// ```
    pub fn on_event<F>(mut self, callback: F) -> Self
    where
        F: Fn(BackendEvent) + Send + Sync + 'static,
    {
        self.event_callback = Some(Arc::new(callback));
        self
    }
    
    /// Build the `BackendHandle`.
    pub fn build(self) -> BackendHandle {
        let (command_tx, command_rx) = tokio_mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::channel();
        
        let repaint_callback = self.repaint_callback;
        let event_callback = self.event_callback;
        
        let repaint_for_task = repaint_callback.clone();
        let event_callback_for_task = event_callback.clone();
        
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
            rt.block_on(async move {
                run_backend_task(command_rx, event_tx, repaint_for_task, event_callback_for_task).await;
            });
        });
        
        BackendHandle {
            command_tx,
            event_rx,
            state: ConnectionState::default(),
            _thread: thread,
            repaint_callback,
            event_callback,
        }
    }
}

/// A handle to the backend that can be used from UI code.
/// 
/// This type manages the tokio runtime, async client, and provides a synchronous
/// interface for sending commands and receiving events. It's designed to be used
/// with immediate-mode UI frameworks like egui, but also supports callback-based
/// usage for reactive/MVVM frameworks.
/// 
/// # Example (Polling)
/// 
/// ```ignore
/// let handle = BackendHandle::new();
/// 
/// // Send a connect command
/// handle.send(BackendCommand::Connect { 
///     addr: "127.0.0.1:5000".to_string(),
///     name: "user".to_string(),
///     password: None,
///     config: ConnectConfig::new(),
/// });
/// 
/// // In your update loop, poll for events
/// while let Some(event) = handle.poll_event() {
///     match event {
///         BackendEvent::Connected { user_id, .. } => { /* handle */ }
///         BackendEvent::ChatReceived { sender, text } => { /* handle */ }
///         _ => {}
///     }
/// }
/// ```
pub struct BackendHandle {
    /// Send commands to the backend task.
    command_tx: tokio_mpsc::UnboundedSender<BackendCommand>,
    /// Receive events from the backend task.
    event_rx: mpsc::Receiver<BackendEvent>,
    /// Current connection state (updated by events).
    state: ConnectionState,
    /// Background thread running the tokio runtime.
    _thread: std::thread::JoinHandle<()>,
    /// Callback to request UI repaint (optional).
    /// Note: Actual callback is passed to the task; this is kept for potential future use.
    #[allow(dead_code)]
    repaint_callback: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Callback for each event (optional, for reactive frameworks).
    /// Note: Actual callback is passed to the task; this is kept for potential future use.
    #[allow(dead_code)]
    event_callback: Option<EventCallback>,
}

/// A cloneable command sender for use from other threads (e.g., audio callbacks).
/// 
/// This is obtained via `BackendHandle::command_sender()` and can be cloned
/// and sent to other threads.
#[derive(Clone)]
pub struct CommandSender {
    tx: tokio_mpsc::UnboundedSender<BackendCommand>,
}

impl CommandSender {
    /// Send a command to the backend.
    pub fn send(&self, command: BackendCommand) {
        let _ = self.tx.send(command);
    }
    
    /// Send voice audio data to the backend.
    /// 
    /// This is a convenience method for sending voice data from audio callbacks.
    pub fn send_voice(&self, audio_bytes: Vec<u8>) {
        let _ = self.tx.send(BackendCommand::SendVoice { audio_bytes });
    }
}

impl BackendHandle {
    /// Create a new backend handle.
    /// 
    /// This spawns a background thread with a tokio runtime to handle async operations.
    /// For more configuration options, use `BackendHandle::builder()`.
    pub fn new() -> Self {
        BackendHandleBuilder::new().build()
    }
    
    /// Create a builder for configuring the backend handle.
    /// 
    /// # Example
    /// ```ignore
    /// let handle = BackendHandle::builder()
    ///     .on_event(|event| println!("{:?}", event))
    ///     .on_repaint(|| refresh_ui())
    ///     .build();
    /// ```
    pub fn builder() -> BackendHandleBuilder {
        BackendHandleBuilder::new()
    }
    
    /// Create a new backend handle with a repaint callback.
    /// 
    /// The callback will be invoked whenever an event is ready, allowing
    /// the UI to request a repaint.
    /// 
    /// This is a convenience method. For more options, use `BackendHandle::builder()`.
    pub fn with_repaint_callback<F>(callback: Option<F>) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        match callback {
            Some(cb) => BackendHandleBuilder::new().on_repaint(cb).build(),
            None => BackendHandleBuilder::new().build(),
        }
    }
    
    /// Send a command to the backend.
    /// 
    /// This is non-blocking. The command will be processed asynchronously
    /// and any results will be received as events.
    pub fn send(&self, command: BackendCommand) {
        let _ = self.command_tx.send(command);
    }
    
    /// Get a cloneable command sender for use from other threads.
    /// 
    /// This is useful for audio callbacks that need to send voice data
    /// from a non-UI thread.
    pub fn command_sender(&self) -> CommandSender {
        CommandSender {
            tx: self.command_tx.clone(),
        }
    }
    
    /// Poll for the next event without blocking.
    /// 
    /// Returns `Some(event)` if an event is available, `None` otherwise.
    /// Call this in your UI update loop.
    pub fn poll_event(&mut self) -> Option<BackendEvent> {
        match self.event_rx.try_recv() {
            Ok(event) => {
                // Update local state based on events
                self.apply_event(&event);
                Some(event)
            }
            Err(_) => None,
        }
    }
    
    /// Get the current connection state.
    /// 
    /// This is a cached copy of the last known state, updated as events arrive.
    pub fn state(&self) -> &ConnectionState {
        &self.state
    }
    
    /// Check if we're currently connected.
    pub fn is_connected(&self) -> bool {
        self.state.connected
    }
    
    /// Get our user ID if connected and assigned.
    pub fn my_user_id(&self) -> Option<u64> {
        self.state.my_user_id
    }
    
    /// Get the current room ID if in a room.
    pub fn current_room_id(&self) -> Option<u64> {
        self.state.current_room_id
    }
    
    /// Get the current connection status.
    pub fn connection_status(&self) -> crate::events::ConnectionStatus {
        self.state.status
    }
    
    /// Apply an event to update local state.
    fn apply_event(&mut self, event: &BackendEvent) {
        use crate::events::ConnectionStatus;
        
        match event {
            BackendEvent::Connected { user_id, client_name } => {
                self.state.status = ConnectionStatus::Connected;
                self.state.connected = true;
                self.state.my_user_id = Some(*user_id);
                self.state.my_client_name = client_name.clone();
            }
            BackendEvent::Disconnected { .. } => {
                self.state = ConnectionState::default();
            }
            BackendEvent::ConnectionLost { .. } => {
                self.state.status = ConnectionStatus::ConnectionLost;
                self.state.connected = false;
            }
            BackendEvent::ConnectionStatusChanged { status } => {
                self.state.status = *status;
                self.state.connected = status.is_connected();
            }
            BackendEvent::StateUpdated { state } => {
                self.state = state.clone();
            }
            _ => {}
        }
    }
    
    /// Request a UI repaint if a callback was provided.
    #[allow(dead_code)]
    fn request_repaint(&self) {
        if let Some(callback) = &self.repaint_callback {
            callback();
        }
    }
}

impl Default for BackendHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// Helper struct to dispatch events to both the channel and callback.
struct EventDispatcher {
    event_tx: mpsc::Sender<BackendEvent>,
    repaint_callback: Option<Arc<dyn Fn() + Send + Sync>>,
    event_callback: Option<EventCallback>,
}

impl EventDispatcher {
    fn new(
        event_tx: mpsc::Sender<BackendEvent>,
        repaint_callback: Option<Arc<dyn Fn() + Send + Sync>>,
        event_callback: Option<EventCallback>,
    ) -> Self {
        Self { event_tx, repaint_callback, event_callback }
    }
    
    /// Send an event to both the polling channel and the callback (if set).
    fn send(&self, event: BackendEvent) {
        // Always send to the channel for polling
        let _ = self.event_tx.send(event.clone());
        
        // Call the event callback if set
        if let Some(callback) = &self.event_callback {
            callback(event);
        }
        
        // Request repaint if callback is set
        if let Some(callback) = &self.repaint_callback {
            callback();
        }
    }
}

impl Clone for EventDispatcher {
    fn clone(&self) -> Self {
        Self {
            event_tx: self.event_tx.clone(),
            repaint_callback: self.repaint_callback.clone(),
            event_callback: self.event_callback.clone(),
        }
    }
}

/// The async task that manages the client connection.
async fn run_backend_task(
    mut command_rx: tokio_mpsc::UnboundedReceiver<BackendCommand>,
    event_tx: mpsc::Sender<BackendEvent>,
    repaint_callback: Option<Arc<dyn Fn() + Send + Sync>>,
    event_callback: Option<EventCallback>,
) {
    use crate::ConnectionLostReason;
    use crate::events::ConnectionStatus;
    
    let dispatcher = EventDispatcher::new(event_tx, repaint_callback, event_callback);
    let mut client: Option<Client> = None;
    let mut client_name = String::new();
    let mut connection_lost_rx: Option<tokio_mpsc::UnboundedReceiver<ConnectionLostReason>> = None;
    
    // Track last connection parameters for potential reconnection
    let mut _last_addr = String::new();
    let mut _last_password: Option<String> = None;
    let mut _last_config = crate::ConnectConfig::default();
    
    loop {
        tokio::select! {
            biased;
            
            // Check for connection loss first (higher priority)
            Some(reason) = async {
                match connection_lost_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                info!("Connection lost detected: {:?}", reason);
                
                // Clear the client and connection_lost receiver
                client = None;
                connection_lost_rx = None;
                
                // Format error message based on reason
                let error_msg = match reason {
                    ConnectionLostReason::StreamClosed => "Server closed the connection".to_string(),
                    ConnectionLostReason::StreamError(e) => format!("Stream error: {}", e),
                    ConnectionLostReason::DatagramError(e) => format!("Datagram error: {}", e),
                    ConnectionLostReason::ClientDisconnect => "Client disconnected".to_string(),
                };
                
                // Emit ConnectionLost event
                dispatcher.send(BackendEvent::ConnectionLost {
                    error: error_msg,
                    will_reconnect: false, // For now, no auto-reconnect
                });
                
                // Update status
                dispatcher.send(BackendEvent::ConnectionStatusChanged {
                    status: ConnectionStatus::ConnectionLost,
                });
            }
            
            Some(cmd) = command_rx.recv() => {
                match cmd {
                    BackendCommand::Connect { addr, name, password, config } => {
                        client_name = name.clone();
                        
                        // Save connection params for potential reconnection
                        _last_addr = addr.clone();
                        _last_password = password.clone();
                        _last_config = config.clone();
                        
                        // Emit connecting status
                        dispatcher.send(BackendEvent::ConnectionStatusChanged {
                            status: ConnectionStatus::Connecting,
                        });
                        
                        match Client::connect(&addr, &name, password.as_deref(), config).await {
                            Ok(mut c) => {
                                let events_rx = c.take_events_receiver();
                                let voice_rx = c.take_voice_receiver();
                                let lost_rx = c.take_connection_lost_receiver();
                                
                                // Store the connection_lost receiver for monitoring
                                connection_lost_rx = Some(lost_rx);
                                
                                // Get user_id from the client - always valid after connect()
                                // since connect() waits for ServerHello
                                let user_id = c.user_id();
                                
                                // Get initial snapshot
                                let snap = c.get_snapshot().await;
                                
                                // Send connected event
                                dispatcher.send(BackendEvent::Connected {
                                    user_id,
                                    client_name: name.clone(),
                                });
                                
                                // Emit connected status change
                                dispatcher.send(BackendEvent::ConnectionStatusChanged {
                                    status: ConnectionStatus::Connected,
                                });
                                
                                // Send initial state
                                dispatcher.send(BackendEvent::StateUpdated {
                                    state: ConnectionState {
                                        status: ConnectionStatus::Connected,
                                        connected: true,
                                        my_user_id: Some(user_id),
                                        my_client_name: name.clone(),
                                        current_room_id: snap.current_room_id,
                                        rooms: snap.rooms.clone(),
                                        users: snap.users.clone(),
                                    },
                                });
                                
                                // Spawn event listener task
                                let dispatcher_for_events = dispatcher.clone();
                                let snapshot = c.snapshot.clone();
                                let name_for_events = name.clone();
                                let user_id_for_events = user_id;
                                tokio::spawn(async move {
                                    handle_server_events(events_rx, dispatcher_for_events, snapshot, name_for_events, user_id_for_events).await;
                                });
                                
                                // Spawn voice datagram listener
                                let dispatcher_for_voice = dispatcher.clone();
                                tokio::spawn(async move {
                                    handle_voice_datagrams(voice_rx, dispatcher_for_voice).await;
                                });
                                
                                client = Some(c);
                            }
                            Err(e) => {
                                dispatcher.send(BackendEvent::ConnectFailed {
                                    error: e.to_string(),
                                });
                                dispatcher.send(BackendEvent::ConnectionStatusChanged {
                                    status: ConnectionStatus::Disconnected,
                                });
                            }
                        }
                    }
                    
                    BackendCommand::Disconnect => {
                        if let Some(c) = client.take() {
                            if let Err(e) = c.disconnect().await {
                                error!("disconnect error: {e}");
                            }
                            connection_lost_rx = None;
                            dispatcher.send(BackendEvent::Disconnected { reason: None });
                            dispatcher.send(BackendEvent::ConnectionStatusChanged {
                                status: ConnectionStatus::Disconnected,
                            });
                        }
                    }
                    
                    BackendCommand::SendChat { text } => {
                        if let Some(c) = &client {
                            if let Err(e) = c.send_chat(&client_name, &text).await {
                                error!("send_chat error: {e}");
                                dispatcher.send(BackendEvent::Error {
                                    message: format!("Failed to send chat: {e}"),
                                });
                            }
                        }
                    }
                    
                    BackendCommand::JoinRoom { room_id } => {
                        if let Some(c) = &client {
                            if let Err(e) = c.join_room(room_id).await {
                                error!("join_room error: {e}");
                            }
                        }
                    }
                    
                    BackendCommand::CreateRoom { name } => {
                        if let Some(c) = &client {
                            if let Err(e) = c.create_room(&name).await {
                                error!("create_room error: {e}");
                            }
                        }
                    }
                    
                    BackendCommand::DeleteRoom { room_id } => {
                        if let Some(c) = &client {
                            if let Err(e) = c.delete_room(room_id).await {
                                error!("delete_room error: {e}");
                            }
                        }
                    }
                    
                    BackendCommand::RenameRoom { room_id, new_name } => {
                        if let Some(c) = &client {
                            if let Err(e) = c.rename_room(room_id, &new_name).await {
                                error!("rename_room error: {e}");
                            }
                        }
                    }
                    
                    BackendCommand::SendVoice { audio_bytes } => {
                        if let Some(c) = &client {
                            if let Err(e) = c.send_voice_datagram_async(audio_bytes).await {
                                // Don't spam errors for voice - just log at debug level
                                tracing::debug!("send_voice_datagram error: {e}");
                            }
                        }
                    }
                }
            }
            
            else => break,
        }
    }
    
    // Clean up on exit
    if let Some(c) = client.take() {
        info!("Backend task exiting, disconnecting client...");
        let _ = c.disconnect().await;
    }
}

/// Handle incoming server events and forward to the event dispatcher.
async fn handle_server_events(
    mut events_rx: tokio_mpsc::UnboundedReceiver<proto::ServerEvent>,
    dispatcher: EventDispatcher,
    snapshot: Arc<tokio::sync::Mutex<crate::RoomSnapshot>>,
    client_name: String,
    user_id: u64,
) {
    use crate::events::ConnectionStatus;
    
    while let Some(ev) = events_rx.recv().await {
        match ev.kind {
            Some(proto::server_event::Kind::ChatBroadcast(cb)) => {
                dispatcher.send(BackendEvent::ChatReceived {
                    sender: cb.sender,
                    text: cb.text,
                });
            }
            Some(proto::server_event::Kind::RoomStateUpdate(_)) => {
                let snap = snapshot.lock().await;
                dispatcher.send(BackendEvent::StateUpdated {
                    state: ConnectionState {
                        status: ConnectionStatus::Connected,
                        connected: true,
                        my_user_id: Some(user_id),
                        my_client_name: client_name.clone(),
                        current_room_id: snap.current_room_id,
                        rooms: snap.rooms.clone(),
                        users: snap.users.clone(),
                    },
                });
            }
            _ => {}
        }
    }
}

/// Handle incoming voice datagrams and forward to the event dispatcher.
async fn handle_voice_datagrams(
    mut voice_rx: tokio_mpsc::UnboundedReceiver<VoiceDatagram>,
    dispatcher: EventDispatcher,
) {
    while let Some(dgram) = voice_rx.recv().await {
        // sender_id is set by server on relay; if missing, skip this datagram
        let Some(sender_id) = dgram.sender_id else {
            continue;
        };
        dispatcher.send(BackendEvent::VoiceReceived {
            sender_id,
            audio_bytes: dgram.opus_data,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_backend_handle_creation() {
        let handle = BackendHandle::new();
        assert!(!handle.is_connected());
        assert!(handle.my_user_id().is_none());
    }
    
    #[test]
    fn test_connection_state_default() {
        let state = ConnectionState::default();
        assert!(!state.connected);
        assert!(state.my_user_id.is_none());
        assert!(state.current_room_id.is_none());
        assert!(state.rooms.is_empty());
        assert!(state.users.is_empty());
    }
    
    #[test]
    fn test_apply_connected_event() {
        let mut handle = BackendHandle::new();
        handle.apply_event(&BackendEvent::Connected {
            user_id: 42,
            client_name: "test_user".to_string(),
        });
        assert!(handle.state.connected);
        assert_eq!(handle.state.my_user_id, Some(42));
        assert_eq!(handle.state.my_client_name, "test_user");
    }
    
    #[test]
    fn test_apply_disconnected_event() {
        let mut handle = BackendHandle::new();
        // First connect
        handle.apply_event(&BackendEvent::Connected {
            user_id: 42,
            client_name: "test_user".to_string(),
        });
        // Then disconnect
        handle.apply_event(&BackendEvent::Disconnected { reason: None });
        assert!(!handle.state.connected);
        assert!(handle.state.my_user_id.is_none());
    }
    
    #[test]
    fn test_event_dispatcher() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        
        let (tx, rx) = std::sync::mpsc::channel();
        let callback_count = Arc::new(AtomicUsize::new(0));
        let count_clone = callback_count.clone();
        
        let callback: EventCallback = Arc::new(move |_event| {
            count_clone.fetch_add(1, Ordering::SeqCst);
        });
        
        let dispatcher = EventDispatcher {
            event_tx: tx,
            repaint_callback: None,
            event_callback: Some(callback),
        };
        
        // Send an event
        dispatcher.send(BackendEvent::Error { message: "test".to_string() });
        
        // Verify channel received event
        let event = rx.try_recv().unwrap();
        assert!(matches!(event, BackendEvent::Error { .. }));
        
        // Verify callback was invoked
        assert_eq!(callback_count.load(Ordering::SeqCst), 1);
    }
    
    #[test]
    fn test_builder_with_callbacks() {
        use std::sync::atomic::{AtomicBool, Ordering};
        
        let repaint_called = Arc::new(AtomicBool::new(false));
        let event_called = Arc::new(AtomicBool::new(false));
        
        let repaint_flag = repaint_called.clone();
        let event_flag = event_called.clone();
        
        let _handle = BackendHandle::builder()
            .on_repaint(move || { repaint_flag.store(true, Ordering::SeqCst); })
            .on_event(move |_| { event_flag.store(true, Ordering::SeqCst); })
            .build();
        
        // Just verify the builder works without panicking
        assert!(!_handle.is_connected());
    }
}
