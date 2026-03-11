//! Transport abstraction for reliable + unreliable messaging.

use async_trait::async_trait;

/// TLS configuration for transport connections.
pub struct TlsConfig {
    pub accept_invalid_certs: bool,
    /// DER-encoded additional CA certificates.
    pub additional_ca_certs: Vec<Vec<u8>>,
    /// SHA-256 fingerprints of accepted server certificates.
    pub accepted_fingerprints: Vec<[u8; 32]>,
}

/// Handle for sending and receiving unreliable datagrams (voice data).
///
/// This trait is separated from `Transport` to allow the datagram handle
/// to be passed independently to the audio task while the connection task
/// retains ownership of the reliable stream methods.
///
/// Implementations should be cheaply cloneable (e.g. wrapping an `Arc`).
#[async_trait]
pub trait DatagramTransport: Send + Sync + 'static {
    /// Send a datagram (unreliable, unordered).
    fn send_datagram(&self, data: &[u8]) -> anyhow::Result<()>;

    /// Receive the next datagram, or `None` if the connection closed.
    async fn recv_datagram(&self) -> anyhow::Result<Option<Vec<u8>>>;
}

/// Reliable + unreliable transport for the Rumble protocol.
///
/// Implementations may use QUIC (native) or WebTransport (browser).
/// Reliable messages go over streams; unreliable datagrams carry voice.
#[async_trait]
pub trait Transport: Send + Sync + 'static {
    /// The datagram handle type, used by the audio task for voice I/O.
    type Datagram: DatagramTransport;

    /// Connect to a server at the given address.
    async fn connect(addr: &str, tls_config: TlsConfig) -> anyhow::Result<Self>
    where
        Self: Sized;

    /// Send a protobuf message over the reliable stream.
    ///
    /// `data` is the prost-encoded message bytes (without any length prefix).
    /// The transport adds its own framing (varint length-delimited, matching
    /// `api::encode_frame_raw` / `api::try_decode_frame`).
    async fn send(&mut self, data: &[u8]) -> anyhow::Result<()>;

    /// Receive the next reliable message, or `None` if the connection closed.
    ///
    /// Returns the prost-encoded message bytes (without the length prefix).
    async fn recv(&mut self) -> anyhow::Result<Option<Vec<u8>>>;

    /// Send a datagram (unreliable, unordered).
    fn send_datagram(&self, data: &[u8]) -> anyhow::Result<()>;

    /// Receive the next datagram, or `None` if the connection closed.
    async fn recv_datagram(&self) -> anyhow::Result<Option<Vec<u8>>>;

    /// Get a cloneable datagram handle for the audio task.
    ///
    /// This handle provides only datagram operations, allowing the audio
    /// task to send/receive voice data independently of the connection task.
    fn datagram_handle(&self) -> Self::Datagram;

    /// Return the DER-encoded peer certificate, if available.
    fn peer_certificate_der(&self) -> Option<Vec<u8>>;

    /// Gracefully close the connection.
    async fn close(&self);
}
