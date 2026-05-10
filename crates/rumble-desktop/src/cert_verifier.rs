//! TLS certificate verification for QUIC connections.
//!
//! Provides three verifiers:
//! - `InteractiveCertVerifier`: captures unknown certs for user confirmation
//! - `FingerprintVerifier`: accepts certs whose SHA-256 fingerprint is in a known set
//! - `AcceptAllVerifier`: danger verifier that accepts any certificate (for testing)

use std::sync::Arc;

use rumble_client_traits::cert::{CapturedCert, ServerCertInfo};
use rustls::{
    CertificateError, DigitallySignedStruct, Error, RootCertStore, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use sha2::{Digest, Sha256};

/// Compute the SHA-256 fingerprint of a DER-encoded certificate.
pub fn compute_sha256_fingerprint(cert_der: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(cert_der);
    hasher.finalize().into()
}

/// A certificate verifier that captures self-signed certificates for user confirmation.
///
/// This verifier:
/// 1. Delegates to the standard WebPKI verifier for normal certificates
/// 2. When verification fails due to unknown issuer (self-signed cert), stores
///    the certificate info in `captured_cert` for later retrieval by the caller
/// 3. The caller can then prompt the user and retry with the fingerprint accepted
pub struct InteractiveCertVerifier {
    root_store: Arc<RootCertStore>,
    provider: Arc<CryptoProvider>,
    captured_cert: CapturedCert,
}

impl std::fmt::Debug for InteractiveCertVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InteractiveCertVerifier")
            .field("root_store", &"<RootCertStore>")
            .field("provider", &"<CryptoProvider>")
            .field("captured_cert", &self.captured_cert)
            .finish()
    }
}

impl InteractiveCertVerifier {
    /// Create a new interactive certificate verifier with shared captured cert storage.
    pub fn new(root_store: RootCertStore, provider: Arc<CryptoProvider>, captured_cert: CapturedCert) -> Self {
        Self {
            root_store: Arc::new(root_store),
            provider,
            captured_cert,
        }
    }
}

impl ServerCertVerifier for InteractiveCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let inner =
            rustls::client::WebPkiServerVerifier::builder_with_provider(self.root_store.clone(), self.provider.clone())
                .build()
                .map_err(|e| Error::General(format!("Failed to build verifier: {}", e)))?;

        match inner.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now) {
            Ok(verified) => {
                if let Ok(mut captured) = self.captured_cert.lock() {
                    *captured = None;
                }
                Ok(verified)
            }
            Err(Error::InvalidCertificate(CertificateError::UnknownIssuer))
            | Err(Error::InvalidCertificate(CertificateError::BadSignature)) => {
                let server_name_str = match server_name {
                    ServerName::DnsName(name) => name.as_ref().to_string(),
                    ServerName::IpAddress(ip) => format!("{:?}", ip),
                    _ => "unknown".to_string(),
                };

                let cert_info = ServerCertInfo::new(end_entity.as_ref(), &server_name_str);

                tracing::warn!(
                    "Self-signed certificate detected for '{}' (fingerprint: {})",
                    cert_info.server_name,
                    cert_info.fingerprint_hex()
                );

                if let Ok(mut captured) = self.captured_cert.lock() {
                    *captured = Some(cert_info);
                }

                Err(Error::InvalidCertificate(CertificateError::UnknownIssuer))
            }
            Err(other_error) => Err(other_error),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// A certificate verifier that accepts certificates whose SHA-256 fingerprint
/// is in a provided set.
#[derive(Debug)]
pub struct FingerprintVerifier {
    fingerprints: Vec<[u8; 32]>,
    root_store: Arc<RootCertStore>,
    provider: Arc<CryptoProvider>,
}

impl FingerprintVerifier {
    pub fn new(fingerprints: Vec<[u8; 32]>) -> Self {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        Self {
            fingerprints,
            root_store: Arc::new(RootCertStore::empty()),
            provider,
        }
    }

    pub fn with_additional_roots(mut self, roots: RootCertStore) -> Self {
        self.root_store = Arc::new(roots);
        self
    }
}

impl ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let fingerprint = compute_sha256_fingerprint(end_entity.as_ref());

        if self.fingerprints.iter().any(|fp| fp == &fingerprint) {
            return Ok(ServerCertVerified::assertion());
        }

        Err(Error::General(format!(
            "certificate fingerprint {:02X}{:02X}{:02X}{:02X}... not in accepted set",
            fingerprint[0], fingerprint[1], fingerprint[2], fingerprint[3],
        )))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// A danger verifier that accepts any certificate without verification.
#[derive(Debug)]
pub struct AcceptAllVerifier {
    provider: Arc<CryptoProvider>,
}

impl Default for AcceptAllVerifier {
    fn default() -> Self {
        Self::new()
    }
}

impl AcceptAllVerifier {
    pub fn new() -> Self {
        Self {
            provider: Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
        }
    }
}

impl ServerCertVerifier for AcceptAllVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// Check if an error indicates a certificate verification failure.
///
/// This checks for both standard rustls errors and quinn-wrapped TLS errors.
pub fn is_cert_verification_error(error: &anyhow::Error) -> bool {
    for cause in error.chain() {
        if let Some(Error::InvalidCertificate(CertificateError::UnknownIssuer | CertificateError::BadSignature)) =
            cause.downcast_ref::<Error>()
        {
            return true;
        }

        if let Some(conn_err) = cause.downcast_ref::<quinn::ConnectionError>()
            && let quinn::ConnectionError::TransportError(te) = conn_err
        {
            let reason = te.to_string();
            if reason.contains("UnknownIssuer")
                || reason.contains("BadSignature")
                || reason.contains("invalid peer certificate")
            {
                return true;
            }
        }

        let msg = cause.to_string();
        if msg.contains("UnknownIssuer") || msg.contains("BadSignature") || msg.contains("invalid peer certificate") {
            return true;
        }
    }
    false
}
