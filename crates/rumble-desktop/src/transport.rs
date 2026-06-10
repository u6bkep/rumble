//! QUIC transport implementation using quinn.
//!
//! Implements the `Transport` trait from `rumble-client-traits` over a QUIC connection,
//! using reliable bi-directional streams for protocol messages and unreliable
//! datagrams for voice data.

use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use quinn::{Endpoint, crypto::rustls::QuicClientConfig};
use rumble_client_traits::transport::{
    BiRecvStream, BiSendStream, BiStreamHandle, DatagramTransport, TlsConfig, Transport, TransportRecvStream,
};
use rustls::{RootCertStore, crypto::CryptoProvider};

use crate::cert_verifier::InteractiveCertVerifier;

/// Build a rustls `ClientConfig` that accepts any server certificate without
/// verification. Only available when the `dangerous-accept-invalid-certs`
/// feature is compiled in; otherwise the request fails closed.
#[cfg(feature = "dangerous-accept-invalid-certs")]
fn build_accept_all_config(provider: &Arc<CryptoProvider>) -> anyhow::Result<rustls::ClientConfig> {
    let verifier = Arc::new(crate::cert_verifier::AcceptAllVerifier::new());
    Ok(rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth())
}

#[cfg(not(feature = "dangerous-accept-invalid-certs"))]
fn build_accept_all_config(_provider: &Arc<CryptoProvider>) -> anyhow::Result<rustls::ClientConfig> {
    anyhow::bail!(
        "TlsConfig.accept_invalid_certs is set, but this build was compiled without the \
         `dangerous-accept-invalid-certs` feature; refusing to skip certificate verification"
    )
}

/// Datagram handle for QUIC voice data, wrapping a cloneable `quinn::Connection`.
///
/// This is passed to the audio task so it can send/receive voice datagrams
/// independently of the connection task's reliable stream operations.
#[derive(Clone)]
pub struct QuinnDatagramHandle {
    connection: quinn::Connection,
}

impl QuinnDatagramHandle {
    /// Create a datagram handle from a quinn connection.
    pub fn new(connection: quinn::Connection) -> Self {
        Self { connection }
    }
}

#[async_trait]
impl DatagramTransport for QuinnDatagramHandle {
    fn send_datagram(&self, data: &[u8]) -> anyhow::Result<()> {
        self.connection.send_datagram(Bytes::copy_from_slice(data))?;
        Ok(())
    }

    async fn recv_datagram(&self) -> anyhow::Result<Option<Vec<u8>>> {
        match self.connection.read_datagram().await {
            Ok(bytes) => Ok(Some(bytes.to_vec())),
            // Any ConnectionError means the connection is dead and no more
            // datagrams will arrive. Return Err so the audio task can stop
            // polling instead of spinning on instant-Ok(None).
            Err(e) => Err(e.into()),
        }
    }
}

/// Append received bytes to a frame-accumulation buffer, bounding its size.
///
/// `rumble_protocol::try_decode_frame` does not itself enforce
/// [`rumble_protocol::MAX_FRAME_LEN`]; a reader fed from an untrusted socket
/// must cap its buffer or the peer can declare a huge length and exhaust
/// memory by trickling bytes for a frame that never completes. A complete
/// frame is always decoded once enough bytes arrive, so exceeding the cap
/// means the peer declared an oversized (or never-completing) frame — fail
/// the connection rather than grow. Mirrors the server's read loop,
/// including the boundary: a buffer of exactly `MAX_FRAME_LEN` is allowed.
fn accumulate_frame_bytes(buf: &mut BytesMut, chunk: &[u8]) -> anyhow::Result<()> {
    buf.extend_from_slice(chunk);
    if buf.len() > rumble_protocol::MAX_FRAME_LEN {
        anyhow::bail!(
            "control frame exceeds MAX_FRAME_LEN ({} bytes buffered); closing connection",
            buf.len()
        );
    }
    Ok(())
}

/// The receive half of a QUIC transport, split off for a separate receiver task.
pub struct QuinnRecvStream {
    recv: quinn::RecvStream,
    buf: BytesMut,
}

#[async_trait]
impl TransportRecvStream for QuinnRecvStream {
    async fn recv(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        loop {
            if let Some(frame) = rumble_protocol::try_decode_frame(&mut self.buf) {
                return Ok(Some(frame));
            }

            let mut tmp = [0u8; 8192];
            match self.recv.read(&mut tmp).await? {
                Some(0) => continue,
                Some(n) => {
                    accumulate_frame_bytes(&mut self.buf, &tmp[..n])?;
                }
                None => return Ok(None),
            }
        }
    }
}

/// Send half of a server-initiated bi-directional QUIC stream.
pub struct QuinnBiSendStream(pub quinn::SendStream);

#[async_trait]
impl BiSendStream for QuinnBiSendStream {
    async fn write_all(&mut self, data: &[u8]) -> anyhow::Result<()> {
        self.0.write_all(data).await?;
        Ok(())
    }

    async fn finish(&mut self) -> anyhow::Result<()> {
        self.0.finish()?;
        Ok(())
    }
}

/// Recv half of a server-initiated bi-directional QUIC stream.
pub struct QuinnBiRecvStream(pub quinn::RecvStream);

#[async_trait]
impl BiRecvStream for QuinnBiRecvStream {
    async fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<Option<usize>> {
        match self.0.read(buf).await? {
            Some(n) => Ok(Some(n)),
            None => Ok(None),
        }
    }
}

/// Cloneable handle for accepting/opening bi-directional streams on a QUIC connection.
///
/// Wraps a `quinn::Connection` (which is internally `Arc`-based and cheaply cloneable).
/// Used by the stream dispatch task to accept server-initiated streams independently
/// of the connection task.
#[derive(Clone)]
pub struct QuinnBiStreamHandle {
    connection: quinn::Connection,
}

#[async_trait]
impl BiStreamHandle for QuinnBiStreamHandle {
    type BiSend = QuinnBiSendStream;
    type BiRecv = QuinnBiRecvStream;

    async fn accept_bi(&self) -> anyhow::Result<Option<(Self::BiSend, Self::BiRecv)>> {
        match self.connection.accept_bi().await {
            Ok((send, recv)) => Ok(Some((QuinnBiSendStream(send), QuinnBiRecvStream(recv)))),
            Err(quinn::ConnectionError::LocallyClosed) | Err(quinn::ConnectionError::ApplicationClosed(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn open_bi(&self) -> anyhow::Result<(Self::BiSend, Self::BiRecv)> {
        let (send, recv) = self.connection.open_bi().await?;
        Ok((QuinnBiSendStream(send), QuinnBiRecvStream(recv)))
    }
}

/// QUIC transport backed by the quinn library.
pub struct QuinnTransport {
    connection: quinn::Connection,
    send: quinn::SendStream,
    recv: Option<quinn::RecvStream>,
    buf: BytesMut,
}

impl QuinnTransport {
    /// Access the underlying quinn connection.
    ///
    /// Used by plugins (e.g., relay file transfer) that need to open
    /// their own bi-directional streams on the QUIC connection.
    pub fn connection(&self) -> &quinn::Connection {
        &self.connection
    }
}

#[async_trait]
impl Transport for QuinnTransport {
    type Datagram = QuinnDatagramHandle;
    type RecvStream = QuinnRecvStream;
    type BiSend = QuinnBiSendStream;
    type BiRecv = QuinnBiRecvStream;
    type BiHandle = QuinnBiStreamHandle;

    async fn connect(addr: &str, tls_config: TlsConfig) -> anyhow::Result<Self> {
        const DEFAULT_PORT: u16 = 5000;

        // Parse address — support "quic://host:port", "rumble://host:port", or "host:port"
        let addr_as_url = if addr.contains("://") {
            addr.to_string()
        } else {
            format!("rumble://{}", addr)
        };

        let url = url::Url::parse(&addr_as_url).map_err(|e| anyhow::anyhow!("Invalid server address: {}", e))?;

        let host = url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("No host in server address"))?
            .to_string();
        let port = url.port().unwrap_or(DEFAULT_PORT);

        // DNS resolve
        let socket_addr = tokio::net::lookup_host(format!("{}:{}", host, port))
            .await
            .context("Failed to resolve address")?
            .next()
            .ok_or_else(|| anyhow::anyhow!("No addresses found for hostname"))?;

        // Build rustls ClientConfig
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());

        // Build root store with additional CAs. Each entry in
        // `additional_ca_certs` is raw file bytes — either DER or PEM. We
        // detect PEM by the standard `-----BEGIN` armor; a single PEM file
        // may concatenate multiple certificates.
        let mut root_store = RootCertStore::empty();
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        for cert_bytes in &tls_config.additional_ca_certs {
            let der_certs: Vec<Vec<u8>> = if cert_bytes.starts_with(b"-----BEGIN") {
                let mut reader = std::io::BufReader::new(cert_bytes.as_slice());
                rustls_pemfile::certs(&mut reader)
                    .filter_map(|r| r.ok())
                    .map(|c| c.to_vec())
                    .collect()
            } else {
                vec![cert_bytes.clone()]
            };
            for der in der_certs {
                let cert = rustls::pki_types::CertificateDer::from(der);
                let _ = root_store.add(cert);
            }
        }

        let mut client_cfg = if tls_config.accept_invalid_certs {
            // Danger: accept any certificate (feature-gated, fails closed otherwise).
            build_accept_all_config(&provider)?
        } else if !tls_config.accepted_fingerprints.is_empty() || tls_config.captured_cert.is_some() {
            // Interactive / pinned verification: trust certs whose fingerprint the
            // user already accepted (name-independent TOFU), and capture unknown
            // certs for confirmation when a `captured_cert` slot is provided.
            let verifier = Arc::new(InteractiveCertVerifier::new(
                root_store,
                provider.clone(),
                tls_config.accepted_fingerprints,
                tls_config.captured_cert,
            ));
            rustls::ClientConfig::builder_with_provider(provider.clone())
                .with_protocol_versions(&[&rustls::version::TLS13])?
                .dangerous()
                .with_custom_certificate_verifier(verifier)
                .with_no_client_auth()
        } else {
            // Standard WebPKI verification
            rustls::ClientConfig::builder_with_provider(provider.clone())
                .with_protocol_versions(&[&rustls::version::TLS13])?
                .with_root_certificates(root_store)
                .with_no_client_auth()
        };

        client_cfg.alpn_protocols = vec![b"rumble".to_vec()];

        let rustls_config = Arc::new(client_cfg);
        let crypto = QuicClientConfig::try_from(rustls_config)?;
        let mut quinn_client_config = quinn::ClientConfig::new(Arc::new(crypto));

        // Configure transport settings
        let mut transport_config = quinn::TransportConfig::default();
        transport_config.max_idle_timeout(Some(
            std::time::Duration::from_secs(30)
                .try_into()
                .expect("30s is within idle timeout bounds"),
        ));
        transport_config.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
        transport_config.datagram_receive_buffer_size(Some(65536));
        quinn_client_config.transport_config(Arc::new(transport_config));

        // Bind to the same address family as the remote
        let bind_addr: std::net::SocketAddr = if socket_addr.is_ipv6() {
            "[::]:0".parse().expect("valid IPv6 any address")
        } else {
            "0.0.0.0:0".parse().expect("valid IPv4 any address")
        };

        let mut endpoint = Endpoint::client(bind_addr)?;
        endpoint.set_default_client_config(quinn_client_config);

        // Use the real dialed host as the SNI / cert-validation name so that
        // CA-signed certs validate by hostname. For IP-literal hosts rustls
        // produces an IP `ServerName` and checks IP SANs accordingly. Self-signed
        // servers fail WebPKI here and fall to the fingerprint-pin / capture path.
        let connection = endpoint
            .connect(socket_addr, &host)?
            .await
            .context("QUIC handshake failed")?;

        let (send, recv) = connection
            .open_bi()
            .await
            .context("Failed to open bi-directional stream")?;

        Ok(QuinnTransport {
            connection,
            send,
            recv: Some(recv),
            buf: BytesMut::new(),
        })
    }

    async fn send(&mut self, data: &[u8]) -> anyhow::Result<()> {
        let frame = rumble_protocol::encode_frame_raw(data);
        self.send.write_all(&frame).await?;
        Ok(())
    }

    async fn recv(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        let recv = self
            .recv
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("recv stream already taken via take_recv()"))?;

        loop {
            if let Some(frame) = rumble_protocol::try_decode_frame(&mut self.buf) {
                return Ok(Some(frame));
            }

            let mut tmp = [0u8; 8192];
            match recv.read(&mut tmp).await? {
                Some(0) => continue,
                Some(n) => {
                    accumulate_frame_bytes(&mut self.buf, &tmp[..n])?;
                }
                None => return Ok(None),
            }
        }
    }

    fn take_recv(&mut self) -> Self::RecvStream {
        let recv = self.recv.take().expect("take_recv() called more than once");
        let buf = std::mem::take(&mut self.buf);
        QuinnRecvStream { recv, buf }
    }

    fn send_datagram(&self, data: &[u8]) -> anyhow::Result<()> {
        self.connection.send_datagram(Bytes::copy_from_slice(data))?;
        Ok(())
    }

    async fn recv_datagram(&self) -> anyhow::Result<Option<Vec<u8>>> {
        match self.connection.read_datagram().await {
            Ok(bytes) => Ok(Some(bytes.to_vec())),
            // Any ConnectionError means the connection is dead and no more
            // datagrams will arrive. Return Err so the audio task can stop
            // polling instead of spinning on instant-Ok(None).
            Err(e) => Err(e.into()),
        }
    }

    fn datagram_handle(&self) -> Self::Datagram {
        QuinnDatagramHandle::new(self.connection.clone())
    }

    fn bi_stream_handle(&self) -> Self::BiHandle {
        QuinnBiStreamHandle {
            connection: self.connection.clone(),
        }
    }

    async fn accept_bi(&self) -> anyhow::Result<Option<(Self::BiSend, Self::BiRecv)>> {
        match self.connection.accept_bi().await {
            Ok((send, recv)) => Ok(Some((QuinnBiSendStream(send), QuinnBiRecvStream(recv)))),
            Err(quinn::ConnectionError::LocallyClosed) | Err(quinn::ConnectionError::ApplicationClosed(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn open_bi(&self) -> anyhow::Result<(Self::BiSend, Self::BiRecv)> {
        let (send, recv) = self.connection.open_bi().await?;
        Ok((QuinnBiSendStream(send), QuinnBiRecvStream(recv)))
    }

    fn peer_certificate_der(&self) -> Option<Vec<u8>> {
        let identity = self.connection.peer_identity()?;
        let certs = identity
            .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
            .ok()?;
        certs.first().map(|c| c.as_ref().to_vec())
    }

    async fn close(&self) {
        self.connection.close(quinn::VarInt::from_u32(0), b"bye");
        // Wait for the connection to actually finish closing so the
        // CONNECTION_CLOSE frame reaches the server. Without this, a
        // caller that closes the transport and immediately exits its
        // runtime (e.g. on process shutdown) can lose the frame and
        // leave the server waiting for the 30s idle timeout to evict
        // the user.
        let _ = self.connection.closed().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulate_allows_buffer_up_to_max_frame_len() {
        let mut buf = BytesMut::new();
        // Fill in chunks the size the recv loops use; the boundary itself
        // (exactly MAX_FRAME_LEN buffered) must be accepted.
        let chunk = [0u8; 8192];
        while buf.len() + chunk.len() <= rumble_protocol::MAX_FRAME_LEN {
            accumulate_frame_bytes(&mut buf, &chunk).expect("buffer within cap must be accepted");
        }
        let remaining = rumble_protocol::MAX_FRAME_LEN - buf.len();
        accumulate_frame_bytes(&mut buf, &vec![0u8; remaining]).expect("buffer of exactly MAX_FRAME_LEN is allowed");
        assert_eq!(buf.len(), rumble_protocol::MAX_FRAME_LEN);
    }

    #[test]
    fn accumulate_rejects_buffer_over_max_frame_len() {
        let mut buf = BytesMut::new();
        accumulate_frame_bytes(&mut buf, &vec![0u8; rumble_protocol::MAX_FRAME_LEN]).expect("cap itself is allowed");
        let err = accumulate_frame_bytes(&mut buf, &[0u8]).expect_err("exceeding the cap must fail");
        assert!(err.to_string().contains("MAX_FRAME_LEN"));
    }
}
