//! Transport abstraction for reliable + unreliable messaging.

use async_trait::async_trait;

use crate::cert::CapturedCert;

/// Send half of a server-initiated bi-directional stream.
#[async_trait]
pub trait BiSendStream: Send + 'static {
    /// Write all bytes to the stream.
    async fn write_all(&mut self, data: &[u8]) -> anyhow::Result<()>;
    /// Signal that no more data will be sent.
    async fn finish(&mut self) -> anyhow::Result<()>;
}

/// Recv half of a server-initiated bi-directional stream.
#[async_trait]
pub trait BiRecvStream: Send + 'static {
    /// Read bytes into the buffer. Returns the number of bytes read, or `None` on EOF.
    async fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<Option<usize>>;
}

/// First frame on a plugin-owned bi-directional stream.
///
/// The client reads this from server-initiated streams to dispatch them to the
/// correct plugin. The wire format mirrors the server's `StreamHeader`:
/// `[name_len: u16 BE][name: bytes]`.
///
/// Unlike the server's version (which reads remaining bytes as metadata),
/// the client version only reads the plugin name and leaves the rest of the
/// stream for the plugin to consume directly.
#[derive(Debug, Clone)]
pub struct StreamHeader {
    /// Plugin name (e.g. "file-relay").
    pub plugin: String,
}

impl StreamHeader {
    /// Write a stream header to a send stream.
    pub async fn write_to(&self, send: &mut (impl BiSendStream + ?Sized)) -> anyhow::Result<()> {
        let name_bytes = self.plugin.as_bytes();
        send.write_all(&(name_bytes.len() as u16).to_be_bytes()).await?;
        send.write_all(name_bytes).await?;
        Ok(())
    }

    /// Read a stream header from a recv stream.
    ///
    /// Reads the 2-byte name length and the name. The remaining stream bytes
    /// are left for the plugin to read.
    pub async fn read_from(recv: &mut (impl BiRecvStream + ?Sized)) -> anyhow::Result<Self> {
        let mut len_buf = [0u8; 2];
        read_exact(recv, &mut len_buf).await?;
        let name_len = u16::from_be_bytes(len_buf) as usize;

        if name_len == 0 || name_len > 255 {
            anyhow::bail!("invalid stream header name length: {name_len}");
        }

        let mut name_buf = vec![0u8; name_len];
        read_exact(recv, &mut name_buf).await?;

        let plugin = String::from_utf8(name_buf).map_err(|e| anyhow::anyhow!("invalid UTF-8 in plugin name: {e}"))?;

        Ok(Self { plugin })
    }
}

/// Read exactly `buf.len()` bytes from a `BiRecvStream`.
pub async fn read_exact(recv: &mut (impl BiRecvStream + ?Sized), buf: &mut [u8]) -> anyhow::Result<()> {
    let mut offset = 0;
    while offset < buf.len() {
        match recv.read(&mut buf[offset..]).await? {
            Some(0) => continue,
            Some(n) => offset += n,
            None => anyhow::bail!("unexpected EOF after {offset} of {} bytes", buf.len()),
        }
    }
    Ok(())
}

/// TLS configuration for transport connections.
pub struct TlsConfig {
    pub accept_invalid_certs: bool,
    /// Additional CA certificates to trust. Each entry is the raw file bytes —
    /// either DER (binary) or PEM (begins with `-----BEGIN`). The transport
    /// implementation is responsible for detecting the format and converting.
    pub additional_ca_certs: Vec<Vec<u8>>,
    /// SHA-256 fingerprints of accepted server certificates.
    pub accepted_fingerprints: Vec<[u8; 32]>,
    /// Optional storage for captured certificate info during interactive
    /// verification. When set, the transport implementation should use an
    /// interactive verifier that captures unknown certificates here instead
    /// of simply rejecting them. The caller checks this after a connection
    /// error to prompt the user for acceptance.
    pub captured_cert: Option<CapturedCert>,
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

/// Handle for accepting and opening bi-directional streams.
///
/// This trait is separated from `Transport` to allow the stream dispatch task
/// to accept server-initiated streams independently of the connection task
/// which retains ownership of the reliable stream send methods.
///
/// Implementations should be cheaply cloneable (e.g. wrapping an `Arc` or
/// `quinn::Connection` which is already internally `Arc`-based).
#[async_trait]
pub trait BiStreamHandle: Send + Sync + 'static {
    /// Send half of a bi-directional stream.
    type BiSend: BiSendStream;
    /// Recv half of a bi-directional stream.
    type BiRecv: BiRecvStream;

    /// Accept a server-initiated bi-directional stream.
    ///
    /// Returns `None` when the connection is closed.
    async fn accept_bi(&self) -> anyhow::Result<Option<(Self::BiSend, Self::BiRecv)>>;

    /// Open a new bi-directional stream to the server.
    ///
    /// Returns the send and recv halves.
    async fn open_bi(&self) -> anyhow::Result<(Self::BiSend, Self::BiRecv)>;
}

/// Type-erased stream opener for plugins that need to open bi-directional streams
/// without coupling to a specific transport implementation.
///
/// Plugins store `Arc<dyn StreamOpener>` and use it to open streams to the server.
#[async_trait]
pub trait StreamOpener: Send + Sync + 'static {
    /// Open a new bi-directional stream to the server.
    ///
    /// Returns boxed send and recv halves.
    async fn open_bi(&self) -> anyhow::Result<(Box<dyn BiSendStream>, Box<dyn BiRecvStream>)>;
}

/// Adapter that implements [`StreamOpener`] for any [`BiStreamHandle`].
pub struct BiStreamOpener<H: BiStreamHandle> {
    handle: H,
}

impl<H: BiStreamHandle> BiStreamOpener<H> {
    /// Create a new stream opener from a bi-stream handle.
    pub fn new(handle: H) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl<H: BiStreamHandle> StreamOpener for BiStreamOpener<H> {
    async fn open_bi(&self) -> anyhow::Result<(Box<dyn BiSendStream>, Box<dyn BiRecvStream>)> {
        let (send, recv) = self.handle.open_bi().await?;
        Ok((Box::new(send), Box::new(recv)))
    }
}

/// The receive half of a transport, for use in a separate task.
///
/// Created by [`Transport::take_recv`]. Allows the connection task to retain
/// the send half while a receiver task handles incoming messages.
#[async_trait]
pub trait TransportRecvStream: Send + 'static {
    /// Receive the next reliable message, or `None` if the connection closed.
    ///
    /// Returns the prost-encoded message bytes (without the length prefix).
    async fn recv(&mut self) -> anyhow::Result<Option<Vec<u8>>>;
}

/// Reliable + unreliable transport for the Rumble protocol.
///
/// Implementations may use QUIC (native) or WebTransport (browser).
/// Reliable messages go over streams; unreliable datagrams carry voice.
#[async_trait]
pub trait Transport: Send + Sync + 'static {
    /// The datagram handle type, used by the audio task for voice I/O.
    type Datagram: DatagramTransport;

    /// The receive stream type, split off via [`take_recv`](Self::take_recv).
    type RecvStream: TransportRecvStream;

    /// Send half of a bi-directional stream.
    type BiSend: BiSendStream;

    /// Recv half of a bi-directional stream.
    type BiRecv: BiRecvStream;

    /// Connect to a server at the given address.
    async fn connect(addr: &str, tls_config: TlsConfig) -> anyhow::Result<Self>
    where
        Self: Sized;

    /// Send a protobuf message over the reliable stream.
    ///
    /// `data` is the prost-encoded message bytes (without any length prefix).
    /// The transport adds its own framing (varint length-delimited, matching
    /// `rumble_protocol::encode_frame_raw` / `rumble_protocol::try_decode_frame`).
    async fn send(&mut self, data: &[u8]) -> anyhow::Result<()>;

    /// Receive the next reliable message, or `None` if the connection closed.
    ///
    /// Returns the prost-encoded message bytes (without the length prefix).
    async fn recv(&mut self) -> anyhow::Result<Option<Vec<u8>>>;

    /// Split off the receive half for use in a separate task.
    ///
    /// After calling this, [`recv`](Self::recv) will return an error.
    /// The returned [`TransportRecvStream`] handles all incoming reliable messages.
    fn take_recv(&mut self) -> Self::RecvStream;

    /// Send a datagram (unreliable, unordered).
    fn send_datagram(&self, data: &[u8]) -> anyhow::Result<()>;

    /// Receive the next datagram, or `None` if the connection closed.
    async fn recv_datagram(&self) -> anyhow::Result<Option<Vec<u8>>>;

    /// The bi-directional stream handle type, used by the stream dispatch task.
    type BiHandle: BiStreamHandle<BiSend = Self::BiSend, BiRecv = Self::BiRecv>;

    /// Get a cloneable datagram handle for the audio task.
    ///
    /// This handle provides only datagram operations, allowing the audio
    /// task to send/receive voice data independently of the connection task.
    fn datagram_handle(&self) -> Self::Datagram;

    /// Get a cloneable bi-directional stream handle for the stream dispatch task.
    ///
    /// This handle provides accept_bi/open_bi operations, allowing the stream
    /// dispatch task to accept server-initiated streams independently of the
    /// connection task.
    fn bi_stream_handle(&self) -> Self::BiHandle;

    /// Accept a server-initiated bi-directional stream.
    ///
    /// Returns `None` when the connection is closed.
    async fn accept_bi(&self) -> anyhow::Result<Option<(Self::BiSend, Self::BiRecv)>>;

    /// Open a new bi-directional stream to the server.
    ///
    /// Returns the send and recv halves.
    async fn open_bi(&self) -> anyhow::Result<(Self::BiSend, Self::BiRecv)>;

    /// Return the DER-encoded peer certificate, if available.
    fn peer_certificate_der(&self) -> Option<Vec<u8>>;

    /// Gracefully close the connection.
    async fn close(&self);
}
