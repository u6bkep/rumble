//! Backend handle for UI integration.
//!
//! The `BackendHandle` provides a clean interface for UI code to interact with
//! the backend. It manages the async runtime, client connection, audio subsystem,
//! and provides synchronous command/event methods suitable for immediate-mode UI
//! frameworks.
//!
//! # Audio Handling
//!
//! Audio capture, encoding, decoding, and playback are handled entirely by the
//! backend. The UI only needs to send commands to start/stop capture and playback,
//! and receives `VoiceActivity` events to show who is talking.
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
//!         BackendEvent::VoiceActivity { user_id, is_talking } => { /* update UI */ }
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

use crate::{
    Client, VoiceDatagram,
    audio::{AudioConfig, AudioInput, AudioOutput, AudioSystem},
    bounded_voice::{BoundedVoiceSender, VoiceChannelConfig, bounded_voice_channel},
    codec::{VoiceDecoder, VoiceEncoder},
    events::{AudioState, BackendCommand, BackendEvent, ConnectionState},
};
use api::proto;
use std::sync::{Arc, Mutex, mpsc};
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{debug, error, info, warn};

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
    /// Capacity of the outbound voice frame buffer.
    voice_buffer_capacity: usize,
}

/// Default capacity for the outbound voice buffer.
/// 50 frames at 5ms = 250ms, at 20ms = 1 second.
const DEFAULT_VOICE_BUFFER_CAPACITY: usize = 50;

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
            voice_buffer_capacity: DEFAULT_VOICE_BUFFER_CAPACITY,
        }
    }

    /// Set the capacity of the outbound voice buffer.
    ///
    /// When the buffer is full, old voice frames will be dropped rather than
    /// growing unbounded. Default is 50 frames (about 250ms at 5ms frame size).
    pub fn voice_buffer_capacity(mut self, capacity: usize) -> Self {
        self.voice_buffer_capacity = capacity;
        self
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

        // Create bounded channel for outbound voice frames
        let voice_config = VoiceChannelConfig {
            max_frames: self.voice_buffer_capacity,
        };
        let (voice_tx, voice_rx) = bounded_voice_channel::<Vec<u8>>(voice_config);

        // Create channel for incoming voice datagrams
        let (incoming_voice_tx, incoming_voice_rx) = mpsc::channel();

        let repaint_callback = self.repaint_callback;
        let event_callback = self.event_callback;

        let repaint_for_task = repaint_callback.clone();
        let event_callback_for_task = event_callback.clone();

        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
            rt.block_on(async move {
                run_backend_task(
                    command_rx,
                    voice_rx,
                    event_tx,
                    incoming_voice_tx,
                    repaint_for_task,
                    event_callback_for_task,
                )
                .await;
            });
        });

        // Initialize audio system and get device lists
        let audio_system = AudioSystem::new();
        let input_devices = audio_system.list_input_devices();
        let output_devices = audio_system.list_output_devices();

        // Initialize state with audio info
        let mut state = ConnectionState::default();
        state.audio = AudioState {
            input_devices,
            output_devices,
            selected_input_id: None,
            selected_output_id: None,
            input_volume: 1.0,
            output_volume: 1.0,
            is_capturing: false,
            is_playing: false,
        };

        BackendHandle {
            command_tx,
            voice_tx,
            event_rx,
            incoming_voice_rx,
            state,
            _thread: thread,
            repaint_callback,
            event_callback,
            // Audio subsystem
            audio_system,
            audio_input: None,
            audio_output: None,
            voice_decoder: None,
            talking_users: std::collections::HashMap::new(),
        }
    }
}

/// A handle to the backend that can be used from UI code.
///
/// This type manages the tokio runtime, async client, audio subsystem, and provides
/// a synchronous interface for sending commands and receiving events. It's designed
/// to be used with immediate-mode UI frameworks like egui, but also supports
/// callback-based usage for reactive/MVVM frameworks.
///
/// # Audio
///
/// Audio capture, encoding, decoding, and playback are handled internally.
/// Use `BackendCommand::StartVoiceCapture` and `BackendCommand::StopVoiceCapture`
/// for push-to-talk functionality.
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
///         BackendEvent::VoiceActivity { user_id, is_talking } => { /* update UI */ }
///         _ => {}
///     }
/// }
/// ```
pub struct BackendHandle {
    /// Send commands to the backend task.
    command_tx: tokio_mpsc::UnboundedSender<BackendCommand>,
    /// Send encoded voice frames to the backend task (bounded, drops on overflow).
    voice_tx: BoundedVoiceSender<Vec<u8>>,
    /// Receive events from the backend task.
    event_rx: mpsc::Receiver<BackendEvent>,
    /// Receive incoming voice datagrams for local decoding and playback.
    incoming_voice_rx: mpsc::Receiver<(u64, Vec<u8>)>,
    /// Current connection state (updated by events).
    state: ConnectionState,
    /// Background thread running the tokio runtime.
    _thread: std::thread::JoinHandle<()>,
    /// Callback to request UI repaint (optional).
    #[allow(dead_code)]
    repaint_callback: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Callback for each event (optional, for reactive frameworks).
    #[allow(dead_code)]
    event_callback: Option<EventCallback>,

    // Audio subsystem (lives on UI thread since cpal streams aren't Send)
    /// Audio system for device enumeration.
    audio_system: AudioSystem,
    /// Active audio input stream (mic capture).
    audio_input: Option<AudioInput>,
    /// Active audio output stream (playback).
    audio_output: Option<AudioOutput>,
    /// Opus decoder for incoming voice (one for all senders for simplicity).
    voice_decoder: Option<VoiceDecoder>,
    /// Track which users are currently talking (user_id -> last frame time).
    talking_users: std::collections::HashMap<u64, std::time::Instant>,
}

/// A cloneable sender for sending commands from other threads.
///
/// This is obtained via `BackendHandle::command_sender()` and can be cloned
/// and sent to other threads (e.g., for UI callbacks on other threads).
#[derive(Clone)]
pub struct CommandSender {
    tx: tokio_mpsc::UnboundedSender<BackendCommand>,
}

impl CommandSender {
    /// Send a command to the backend.
    pub fn send(&self, command: BackendCommand) {
        let _ = self.tx.send(command);
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
    pub fn with_repaint_callback<F>(callback: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        BackendHandleBuilder::new().on_repaint(callback).build()
    }

    /// Send a command to the backend.
    ///
    /// This is non-blocking. The command will be processed asynchronously
    /// and any results will be received as events.
    ///
    /// Audio commands (StartVoiceCapture, StopVoiceCapture, etc.) are handled
    /// locally by the BackendHandle since audio devices live on the UI thread.
    pub fn send(&mut self, command: BackendCommand) {
        match command {
            // Audio commands are handled locally
            BackendCommand::StartVoiceCapture => {
                self.start_voice_capture();
            }
            BackendCommand::StopVoiceCapture => {
                self.stop_voice_capture();
            }
            BackendCommand::StartPlayback => {
                self.start_playback();
            }
            BackendCommand::StopPlayback => {
                self.stop_playback();
            }
            BackendCommand::SetInputDevice { device_id } => {
                self.state.audio.selected_input_id = device_id;
                // Restart capture if currently active
                if self.state.audio.is_capturing {
                    self.stop_voice_capture();
                    self.start_voice_capture();
                }
                self.emit_audio_state_changed();
            }
            BackendCommand::SetOutputDevice { device_id } => {
                self.state.audio.selected_output_id = device_id;
                // Restart playback if currently active
                if self.state.audio.is_playing {
                    self.stop_playback();
                    self.start_playback();
                }
                self.emit_audio_state_changed();
            }
            BackendCommand::SetInputVolume { volume } => {
                self.state.audio.input_volume = volume.clamp(0.0, 2.0);
                self.emit_audio_state_changed();
            }
            BackendCommand::SetOutputVolume { volume } => {
                self.state.audio.output_volume = volume.clamp(0.0, 2.0);
                self.emit_audio_state_changed();
            }
            BackendCommand::RefreshAudioDevices => {
                self.refresh_audio_devices();
            }
            // Other commands go to the backend task
            _ => {
                let _ = self.command_tx.send(command);
            }
        }
    }

    /// Refresh the list of available audio devices.
    fn refresh_audio_devices(&mut self) {
        self.state.audio.input_devices = self.audio_system.list_input_devices();
        self.state.audio.output_devices = self.audio_system.list_output_devices();
        self.emit_audio_state_changed();
    }

    /// Emit an AudioStateChanged event and request repaint.
    fn emit_audio_state_changed(&self) {
        // Call event callback if set
        if let Some(callback) = &self.event_callback {
            callback(BackendEvent::AudioStateChanged {
                state: self.state.audio.clone(),
            });
        }
        // Request UI repaint
        if let Some(callback) = &self.repaint_callback {
            callback();
        }
    }

    /// Get a cloneable command sender for use from other threads.
    ///
    /// This is useful for callbacks that need to send commands
    /// from a non-UI thread.
    pub fn command_sender(&self) -> CommandSender {
        CommandSender {
            tx: self.command_tx.clone(),
        }
    }

    /// Get voice channel statistics.
    ///
    /// Returns the total number of voice frames sent and dropped.
    pub fn voice_stats(&self) -> crate::VoiceChannelStats {
        self.voice_tx.stats()
    }

    /// Poll for the next event without blocking.
    ///
    /// Returns `Some(event)` if an event is available, `None` otherwise.
    /// Call this in your UI update loop.
    ///
    /// This also processes incoming voice datagrams, decoding them and
    /// queueing for playback. VoiceActivity events are emitted when users
    /// start/stop talking.
    pub fn poll_event(&mut self) -> Option<BackendEvent> {
        // Process incoming voice datagrams first (decode and play)
        self.process_incoming_voice();

        // Check for stale talkers (users who stopped talking)
        let voice_activity_event = self.check_voice_activity_timeout();
        if voice_activity_event.is_some() {
            return voice_activity_event;
        }

        // Then check for regular events
        match self.event_rx.try_recv() {
            Ok(event) => {
                // Update local state based on events
                self.apply_event(&event);
                Some(event)
            }
            Err(_) => None,
        }
    }

    /// Process all pending incoming voice datagrams.
    fn process_incoming_voice(&mut self) {
        while let Ok((sender_id, opus_data)) = self.incoming_voice_rx.try_recv() {
            // Decode and play
            if self.process_voice_datagram(sender_id, &opus_data) {
                // Track that this user is talking
                let was_talking = self.talking_users.contains_key(&sender_id);
                self.talking_users
                    .insert(sender_id, std::time::Instant::now());

                // If they just started talking, we'll emit an event
                if !was_talking {
                    // Note: We could emit the event here, but for now we batch them
                    debug!("User {} started talking", sender_id);
                }
            }
        }
    }

    /// Check for users who stopped talking (no voice frames for a while).
    fn check_voice_activity_timeout(&mut self) -> Option<BackendEvent> {
        /// Timeout for considering a user as "stopped talking".
        const VOICE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

        let now = std::time::Instant::now();
        let mut stopped_user = None;

        self.talking_users.retain(|&user_id, &mut last_time| {
            if now.duration_since(last_time) > VOICE_TIMEOUT {
                stopped_user = Some(user_id);
                false // Remove from map
            } else {
                true
            }
        });

        stopped_user.map(|user_id| BackendEvent::VoiceActivity {
            user_id,
            is_talking: false,
        })
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

    /// Get the current room UUID if in a room.
    pub fn current_room_id(&self) -> Option<uuid::Uuid> {
        self.state.current_room_id
    }

    /// Get the current connection status.
    pub fn connection_status(&self) -> crate::events::ConnectionStatus {
        self.state.status
    }

    // ========== Audio Device Enumeration ==========
    //
    // These methods are provided for convenience, but the UI should prefer
    // to read audio state from `state().audio` for consistency.

    /// List available input (microphone) devices.
    ///
    /// Prefer using `state().audio.input_devices` for reactive UIs.
    pub fn list_input_devices(&self) -> &[crate::AudioDeviceInfo] {
        &self.state.audio.input_devices
    }

    /// List available output (speaker/headphone) devices.
    ///
    /// Prefer using `state().audio.output_devices` for reactive UIs.
    pub fn list_output_devices(&self) -> &[crate::AudioDeviceInfo] {
        &self.state.audio.output_devices
    }

    /// Get the currently selected input device ID.
    pub fn selected_input_device(&self) -> Option<&str> {
        self.state.audio.selected_input_id.as_deref()
    }

    /// Get the currently selected output device ID.
    pub fn selected_output_device(&self) -> Option<&str> {
        self.state.audio.selected_output_id.as_deref()
    }

    /// Get the current input volume (0.0 - 2.0).
    pub fn input_volume(&self) -> f32 {
        self.state.audio.input_volume
    }

    /// Get the current output volume (0.0 - 2.0).
    pub fn output_volume(&self) -> f32 {
        self.state.audio.output_volume
    }

    /// Check if voice capture is currently active.
    pub fn is_capturing(&self) -> bool {
        self.state.audio.is_capturing
    }

    /// Check if playback is currently active.
    pub fn is_playing(&self) -> bool {
        self.state.audio.is_playing
    }

    // ========== Audio Control (Internal) ==========

    /// Start voice capture (encoding and sending to server).
    fn start_voice_capture(&mut self) {
        if self.audio_input.is_some() {
            return; // Already capturing
        }

        let device = match &self.state.audio.selected_input_id {
            Some(id) => self.audio_system.get_input_device_by_id(id),
            None => self.audio_system.default_input_device(),
        };

        let Some(device) = device else {
            warn!("No input device available for voice capture");
            return;
        };

        let config = AudioConfig::default();
        let input_volume = self.state.audio.input_volume;

        // Create encoder wrapped in Arc<Mutex> for the audio callback
        let encoder = match VoiceEncoder::new() {
            Ok(enc) => Arc::new(Mutex::new(enc)),
            Err(e) => {
                error!("Failed to create voice encoder: {}", e);
                return;
            }
        };

        // Get sender for the bounded voice channel
        let voice_tx = self.voice_tx.clone();

        match AudioInput::new(&device, &config, move |samples| {
            // Apply input volume
            let adjusted: Vec<f32> = samples.iter().map(|s| s * input_volume).collect();

            // Encode with Opus
            if let Ok(mut enc) = encoder.lock() {
                match enc.encode(&adjusted) {
                    Ok(opus_data) => {
                        // Send encoded data to the backend task
                        voice_tx.try_send(opus_data);
                    }
                    Err(e) => {
                        debug!("Opus encode error: {}", e);
                    }
                }
            }
        }) {
            Ok(input) => {
                self.audio_input = Some(input);
                self.state.audio.is_capturing = true;
                self.emit_audio_state_changed();
                info!("Voice capture started");
            }
            Err(e) => {
                error!("Failed to start voice capture: {}", e);
            }
        }
    }

    /// Stop voice capture.
    fn stop_voice_capture(&mut self) {
        if let Some(input) = self.audio_input.take() {
            input.pause();
            self.state.audio.is_capturing = false;
            self.emit_audio_state_changed();
            info!("Voice capture stopped");
        }
    }

    /// Start audio playback (receiving and playing voice from others).
    fn start_playback(&mut self) {
        if self.audio_output.is_some() {
            return; // Already playing
        }

        let device = match &self.state.audio.selected_output_id {
            Some(id) => self.audio_system.get_output_device_by_id(id),
            None => self.audio_system.default_output_device(),
        };

        let Some(device) = device else {
            warn!("No output device available for playback");
            return;
        };

        let config = AudioConfig::default();

        match AudioOutput::new(&device, &config) {
            Ok(output) => {
                self.audio_output = Some(output);
                self.state.audio.is_playing = true;
                self.emit_audio_state_changed();
                info!("Audio playback started");

                // Initialize decoder if not already created
                if self.voice_decoder.is_none() {
                    match VoiceDecoder::new() {
                        Ok(dec) => self.voice_decoder = Some(dec),
                        Err(e) => error!("Failed to create voice decoder: {}", e),
                    }
                }
            }
            Err(e) => {
                error!("Failed to start audio playback: {}", e);
            }
        }
    }

    /// Stop audio playback.
    fn stop_playback(&mut self) {
        if let Some(output) = self.audio_output.take() {
            output.pause();
            self.state.audio.is_playing = false;
            self.emit_audio_state_changed();
            info!("Audio playback stopped");
        }
    }

    /// Process received voice datagram (decode and queue for playback).
    ///
    /// This is called from the event loop when voice data arrives.
    fn process_voice_datagram(&mut self, sender_id: u64, opus_data: &[u8]) -> bool {
        let Some(output) = &self.audio_output else {
            return false;
        };

        let Some(decoder) = &mut self.voice_decoder else {
            return false;
        };

        match decoder.decode(opus_data) {
            Ok(pcm_samples) => {
                // Apply output volume
                let adjusted: Vec<f32> = pcm_samples
                    .iter()
                    .map(|s| s * self.state.audio.output_volume)
                    .collect();
                output.queue_samples(&adjusted);
                true
            }
            Err(e) => {
                debug!("Opus decode error for sender {}: {}", sender_id, e);
                false
            }
        }
    }

    /// Apply an event to update local state.
    fn apply_event(&mut self, event: &BackendEvent) {
        use crate::events::ConnectionStatus;

        match event {
            BackendEvent::Connected {
                user_id,
                client_name,
            } => {
                self.state.status = ConnectionStatus::Connected;
                self.state.connected = true;
                self.state.my_user_id = Some(*user_id);
                self.state.my_client_name = client_name.clone();
            }
            BackendEvent::Disconnected { .. } => {
                // Preserve audio state when disconnecting
                let audio = std::mem::take(&mut self.state.audio);
                self.state = ConnectionState::default();
                self.state.audio = audio;
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
                // Preserve local audio state when receiving state updates from backend task
                let audio = std::mem::take(&mut self.state.audio);
                self.state = state.clone();
                self.state.audio = audio;
            }
            BackendEvent::AudioStateChanged { state: audio_state } => {
                self.state.audio = audio_state.clone();
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
        Self {
            event_tx,
            repaint_callback,
            event_callback,
        }
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
    mut voice_rx: crate::BoundedVoiceReceiver<Vec<u8>>,
    event_tx: mpsc::Sender<BackendEvent>,
    incoming_voice_tx: mpsc::Sender<(u64, Vec<u8>)>,
    repaint_callback: Option<Arc<dyn Fn() + Send + Sync>>,
    event_callback: Option<EventCallback>,
) {
    use crate::{ConnectionLostReason, events::ConnectionStatus};

    let dispatcher = EventDispatcher::new(event_tx, repaint_callback.clone(), event_callback);
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
                                        audio: AudioState::default(),
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

                                // Spawn voice datagram listener - sends to UI thread for decoding
                                let voice_tx_clone = incoming_voice_tx.clone();
                                let repaint_clone = repaint_callback.clone();
                                tokio::spawn(async move {
                                    handle_voice_datagrams(voice_rx, voice_tx_clone).await;
                                    // Request repaint when voice arrives
                                    if let Some(cb) = repaint_clone {
                                        cb();
                                    }
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

                    BackendCommand::JoinRoom { room_uuid } => {
                        if let Some(c) = &client {
                            if let Err(e) = c.join_room(room_uuid).await {
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

                    BackendCommand::DeleteRoom { room_uuid } => {
                        if let Some(c) = &client {
                            if let Err(e) = c.delete_room(room_uuid).await {
                                error!("delete_room error: {e}");
                            }
                        }
                    }

                    BackendCommand::RenameRoom { room_uuid, new_name } => {
                        if let Some(c) = &client {
                            if let Err(e) = c.rename_room(room_uuid, &new_name).await {
                                error!("rename_room error: {e}");
                            }
                        }
                    }

                    // Audio commands are handled by the BackendHandle on the UI thread
                    BackendCommand::StartVoiceCapture
                    | BackendCommand::StopVoiceCapture
                    | BackendCommand::StartPlayback
                    | BackendCommand::StopPlayback
                    | BackendCommand::SetInputDevice { .. }
                    | BackendCommand::SetOutputDevice { .. }
                    | BackendCommand::SetInputVolume { .. }
                    | BackendCommand::SetOutputVolume { .. }
                    | BackendCommand::RefreshAudioDevices => {
                        // These are handled by BackendHandle::send() and shouldn't reach here
                        debug!("Audio command received in backend task - should be handled by BackendHandle");
                    }
                }
            }

            // Handle encoded voice frames from the bounded channel - send to server
            Some(opus_data) = voice_rx.recv() => {
                if let Some(c) = &client {
                    if let Err(e) = c.send_voice_datagram_async(opus_data).await {
                        // Don't spam errors for voice - just log at debug level
                        tracing::debug!("send_voice_datagram error: {e}");
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
                        audio: AudioState::default(),
                    },
                });
            }
            _ => {}
        }
    }
}

/// Handle incoming voice datagrams by forwarding to the voice channel.
///
/// The receiver is bounded, so if playback can't keep up, old frames
/// will be dropped at the sender side rather than buffering indefinitely.
async fn handle_voice_datagrams(
    mut voice_rx: tokio_mpsc::Receiver<VoiceDatagram>,
    incoming_voice_tx: mpsc::Sender<(u64, Vec<u8>)>,
) {
    while let Some(dgram) = voice_rx.recv().await {
        // sender_id is set by server on relay; if missing, skip this datagram
        let Some(sender_id) = dgram.sender_id else {
            continue;
        };
        // Forward to the UI thread for decoding and playback
        let _ = incoming_voice_tx.send((sender_id, dgram.opus_data));
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
        dispatcher.send(BackendEvent::Error {
            message: "test".to_string(),
        });

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
            .on_repaint(move || {
                repaint_flag.store(true, Ordering::SeqCst);
            })
            .on_event(move |_| {
                event_flag.store(true, Ordering::SeqCst);
            })
            .build();

        // Just verify the builder works without panicking
        assert!(!_handle.is_connected());
    }
}
