/// Bridge configuration.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// Address of the Rumble server to connect to (e.g., "127.0.0.1:5000").
    pub rumble_addr: String,
    /// Port to listen on for Mumble client connections.
    pub mumble_port: u16,
    /// Display name for the bridge user on the Rumble server.
    pub bridge_name: String,
    /// Optional welcome text shown to Mumble clients.
    pub welcome_text: String,
    /// Maximum bandwidth advertised to Mumble clients (bits/sec).
    pub max_bandwidth: u32,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            rumble_addr: "127.0.0.1:5000".to_string(),
            mumble_port: 64738,
            bridge_name: "MumbleBridge".to_string(),
            welcome_text: "Connected to Rumble via Mumble Bridge".to_string(),
            max_bandwidth: 558000,
        }
    }
}
