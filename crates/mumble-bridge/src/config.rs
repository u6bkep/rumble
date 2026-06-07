use std::path::PathBuf;

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
    /// How to verify the Rumble server's TLS certificate.
    pub rumble_tls: RumbleTlsTrust,
}

/// How the bridge establishes trust in the Rumble server's TLS certificate.
///
/// Defaults to [`RumbleTlsTrust::WebPki`] (fail-closed); operators pin a
/// self-signed server explicitly.
#[derive(Debug, Clone, Default)]
pub enum RumbleTlsTrust {
    /// Standard WebPKI verification against system roots, matching the dialed
    /// hostname. Correct for a Rumble server behind a CA-signed cert.
    #[default]
    WebPki,
    /// Trust the CA/leaf certificate(s) in this PEM or DER file.
    CertFile(PathBuf),
    /// Pin the server's leaf cert by its SHA-256 fingerprint (hostname-independent).
    Fingerprint([u8; 32]),
    /// Accept any certificate without verification. Dangerous; requires the
    /// `dangerous-accept-invalid-certs` feature in rumble-desktop.
    Insecure,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            rumble_addr: "127.0.0.1:5000".to_string(),
            mumble_port: 64738,
            bridge_name: "MumbleBridge".to_string(),
            welcome_text: "Connected to Rumble via Mumble Bridge".to_string(),
            max_bandwidth: 558000,
            rumble_tls: RumbleTlsTrust::default(),
        }
    }
}
