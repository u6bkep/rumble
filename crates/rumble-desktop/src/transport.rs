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
use rustls::RootCertStore;

use crate::cert_verifier::{AcceptAllVerifier, FingerprintVerifier, InteractiveCertVerifier};

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
            Err(_) => Ok(None),
        }
    }
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
                    self.buf.extend_from_slice(&tmp[..n]);
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
            // Danger: accept any certificate
            let verifier = Arc::new(AcceptAllVerifier::new());
            rustls::ClientConfig::builder_with_provider(provider.clone())
                .with_protocol_versions(&[&rustls::version::TLS13])?
                .dangerous()
                .with_custom_certificate_verifier(verifier)
                .with_no_client_auth()
        } else if let Some(captured_cert) = tls_config.captured_cert {
            // Interactive verification: captures unknown certs for user confirmation
            let verifier = Arc::new(InteractiveCertVerifier::new(
                root_store,
                provider.clone(),
                captured_cert,
            ));
            rustls::ClientConfig::builder_with_provider(provider.clone())
                .with_protocol_versions(&[&rustls::version::TLS13])?
                .dangerous()
                .with_custom_certificate_verifier(verifier)
                .with_no_client_auth()
        } else if !tls_config.accepted_fingerprints.is_empty() {
            // Fingerprint-pinned verification
            let verifier =
                Arc::new(FingerprintVerifier::new(tls_config.accepted_fingerprints).with_additional_roots(root_store));
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

        // Use "localhost" as the SNI server name (matching existing backend behavior)
        let connection = endpoint
            .connect(socket_addr, "localhost")?
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
                    self.buf.extend_from_slice(&tmp[..n]);
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
            Err(_) => Ok(None),
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
