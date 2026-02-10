//! JSON protocol for daemon-client communication.

use serde::{Deserialize, Serialize};

/// Crop rectangle for screenshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CropRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Commands sent from CLI to daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Ping the daemon to check it's alive.
    Ping,

    /// Shut down the daemon.
    Shutdown,

    /// Get daemon status.
    Status,

    // -- Server management --
    /// Start the Rumble server.
    ServerStart {
        #[serde(default = "default_server_port")]
        port: u16,
    },

    /// Stop the Rumble server.
    ServerStop,

    // -- Client management --
    /// Create a new GUI client instance.
    ClientNew {
        name: Option<String>,
        server: Option<String>,
    },

    /// List all active clients.
    ClientList,

    /// Close a client instance.
    ClientClose { id: u32 },

    // -- Interaction --
    /// Take a screenshot of a client.
    Screenshot {
        id: u32,
        /// Output path for the PNG file. If None, uses a temp file.
        output: Option<String>,
        /// Optional crop rectangle.
        crop: Option<CropRect>,
    },

    /// Click at a position.
    Click { id: u32, x: f32, y: f32 },

    /// Press a key.
    KeyPress { id: u32, key: String },

    /// Release a key.
    KeyRelease { id: u32, key: String },

    /// Tap a key (press + release).
    KeyTap { id: u32, key: String },

    /// Type text.
    TypeText { id: u32, text: String },

    /// Run frames to advance the UI.
    RunFrames {
        id: u32,
        #[serde(default = "default_frame_count")]
        count: u32,
    },

    // -- State inspection --
    /// Get client connection status.
    IsConnected { id: u32 },

    /// Get full backend state as JSON.
    GetState { id: u32 },

    /// Move mouse pointer to position
    MouseMove { id: u32, x: f32, y: f32 },

    // -- Widget queries (kittest) --
    /// Click a widget by its accessible label.
    ClickWidget { id: u32, label: String },

    /// Check if a widget with the given label exists.
    HasWidget { id: u32, label: String },

    /// Get the bounding rectangle of a widget by label.
    WidgetRect { id: u32, label: String },

    /// Run frames until UI settles (animations complete, no repaints pending).
    Run { id: u32 },

    // -- Settings --
    /// Enable or disable auto-download for a client.
    SetAutoDownload { id: u32, enabled: bool },

    /// Set auto-download rules for a client.
    SetAutoDownloadRules {
        id: u32,
        /// Rules as JSON array: [{"mime_pattern": "image/*", "max_size_bytes": 10485760}, ...]
        rules: Vec<AutoDownloadRuleConfig>,
    },

    /// Get current file transfer settings for a client.
    GetFileTransferSettings { id: u32 },

    /// Set a keyboard shortcut (hotkey) for a client.
    /// action: "ptt", "mute", or "deafen"
    /// key: e.g., "Space", "M", "F1", or null to clear
    SetHotkey {
        id: u32,
        action: String,
        key: Option<String>,
    },

    // -- File transfers --
    /// Share a file.
    ShareFile { id: u32, path: String },

    /// Get list of file transfers.
    GetFileTransfers { id: u32 },

    /// Show or hide the file transfers window.
    ShowTransfers { id: u32, show: bool },
}

/// Auto-download rule configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoDownloadRuleConfig {
    pub mime_pattern: String,
    pub max_size_bytes: u64,
}

fn default_server_port() -> u16 {
    5000
}

fn default_frame_count() -> u32 {
    1
}

/// Responses sent from daemon to CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    /// Command succeeded.
    Ok { data: ResponseData },

    /// Command failed.
    Error { message: String },
}

/// Data payload for successful responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseData {
    /// Simple acknowledgment.
    Ack,

    /// Pong response.
    Pong,

    /// Daemon status.
    Status { server_running: bool, client_count: u32 },

    /// New client created.
    ClientCreated { id: u32 },

    /// List of clients.
    ClientList { clients: Vec<ClientInfo> },

    /// Screenshot taken.
    Screenshot { path: String, width: u32, height: u32 },

    /// Connection status.
    Connected { connected: bool },

    /// Backend state.
    State { state: serde_json::Value },

    /// Frames executed.
    FramesRun { count: u32 },

    /// Widget exists check result.
    WidgetExists { exists: bool },

    /// Widget rectangle.
    WidgetRect {
        found: bool,
        x: Option<f32>,
        y: Option<f32>,
        width: Option<f32>,
        height: Option<f32>,
    },

    /// File transfer settings.
    FileTransferSettings {
        auto_download_enabled: bool,
        auto_download_rules: Vec<AutoDownloadRuleConfig>,
    },

    /// File shared result.
    FileShared { infohash: String, magnet_link: String },

    /// List of file transfers.
    FileTransfers { transfers: Vec<FileTransferInfo> },
}

/// Information about a file transfer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileTransferInfo {
    pub infohash: String,
    pub name: String,
    pub size: u64,
    pub progress: f32,
    pub state: String,
    pub is_downloading: bool,
}

/// Information about a client instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub id: u32,
    pub name: String,
    pub connected: bool,
}

impl Response {
    pub fn ok(data: ResponseData) -> Self {
        Response::Ok { data }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Response::Error {
            message: message.into(),
        }
    }

    pub fn ack() -> Self {
        Response::ok(ResponseData::Ack)
    }
}
