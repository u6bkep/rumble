//! QUIC transport implementation using quinn.
//!
//! Implements the `Transport` trait from `rumble-client` over a QUIC connection,
//! using reliable bi-directional streams for protocol messages and unreliable
//! datagrams for voice data.

use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use quinn::{Endpoint, crypto::rustls::QuicClientConfig};
use rumble_client::transport::{DatagramTransport, TlsConfig, Transport};
use rustls::RootCertStore;

use crate::cert_verifier::{AcceptAllVerifier, FingerprintVerifier};

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

/// QUIC transport backed by the quinn library.
pub struct QuinnTransport {
    connection: quinn::Connection,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    buf: BytesMut,
}

#[async_trait]
impl Transport for QuinnTransport {
    type Datagram = QuinnDatagramHandle;

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

        let mut client_cfg = if tls_config.accept_invalid_certs {
            // Danger: accept any certificate
            let verifier = Arc::new(AcceptAllVerifier::new());
            rustls::ClientConfig::builder_with_provider(provider)
                .with_protocol_versions(&[&rustls::version::TLS13])?
                .dangerous()
                .with_custom_certificate_verifier(verifier)
                .with_no_client_auth()
        } else if !tls_config.accepted_fingerprints.is_empty() {
            // Fingerprint-pinned verification
            let mut root_store = RootCertStore::empty();
            root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            for cert_der in &tls_config.additional_ca_certs {
                let cert = rustls::pki_types::CertificateDer::from(cert_der.clone());
                let _ = root_store.add(cert);
            }

            let verifier =
                Arc::new(FingerprintVerifier::new(tls_config.accepted_fingerprints).with_additional_roots(root_store));
            rustls::ClientConfig::builder_with_provider(provider)
                .with_protocol_versions(&[&rustls::version::TLS13])?
                .dangerous()
                .with_custom_certificate_verifier(verifier)
                .with_no_client_auth()
        } else {
            // Standard WebPKI verification with optional additional CAs
            let mut root_store = RootCertStore::empty();
            root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            for cert_der in &tls_config.additional_ca_certs {
                let cert = rustls::pki_types::CertificateDer::from(cert_der.clone());
                let _ = root_store.add(cert);
            }

            rustls::ClientConfig::builder_with_provider(provider)
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
            recv,
            buf: BytesMut::new(),
        })
    }

    async fn send(&mut self, data: &[u8]) -> anyhow::Result<()> {
        // Use the same varint length-delimited framing as api::encode_frame /
        // api::try_decode_frame so that QuinnTransport is wire-compatible with
        // the existing Rumble protocol.
        let frame = api::encode_frame_raw(data);
        self.send.write_all(&frame).await?;
        Ok(())
    }

    async fn recv(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        loop {
            // Try to extract a complete frame from the buffer
            if let Some(frame) = api::try_decode_frame(&mut self.buf) {
                return Ok(Some(frame));
            }

            // Read more data from the stream
            let mut tmp = [0u8; 8192];
            match self.recv.read(&mut tmp).await? {
                Some(0) => {
                    // No bytes read but stream still open — continue
                    continue;
                }
                Some(n) => {
                    self.buf.extend_from_slice(&tmp[..n]);
                }
                None => {
                    // Stream closed
                    return Ok(None);
                }
            }
        }
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

    fn peer_certificate_der(&self) -> Option<Vec<u8>> {
        let identity = self.connection.peer_identity()?;
        let certs = identity
            .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
            .ok()?;
        certs.first().map(|c| c.as_ref().to_vec())
    }

    async fn close(&self) {
        self.connection.close(quinn::VarInt::from_u32(0), b"bye");
    }
}
