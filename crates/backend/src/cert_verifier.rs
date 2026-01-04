//! Custom TLS certificate verification with interactive self-signed certificate acceptance.
//!
//! This module provides a certificate verifier that:
//! 1. First attempts normal WebPKI verification
//! 2. If verification fails with an unknown issuer (self-signed), captures the certificate
//!    and returns a special error that allows the caller to prompt the user for acceptance
//!
//! # Usage
//!
//! When a connection fails with a self-signed certificate, the error can be inspected
//! to extract the `ServerCertInfo`, which contains the certificate fingerprint and details
//! for user confirmation.

use rustls::{
    CertificateError, DigitallySignedStruct, Error, RootCertStore, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

/// Shared storage for a captured certificate during verification.
///
/// This is used to pass the certificate info out of the verifier without
/// relying on error message parsing, since errors get serialized through
/// the TLS alert mechanism and lose structured data.
pub type CapturedCert = Arc<Mutex<Option<ServerCertInfo>>>;

/// Create a new captured certificate storage.
pub fn new_captured_cert() -> CapturedCert {
    Arc::new(Mutex::new(None))
}

/// Information about a server certificate that failed verification.
///
/// This is returned when a self-signed or unknown CA certificate is encountered,
/// allowing the UI to prompt the user for acceptance.
#[derive(Debug, Clone)]
pub struct ServerCertInfo {
    /// The DER-encoded certificate that failed verification.
    pub certificate_der: Vec<u8>,
    /// SHA256 fingerprint of the certificate.
    pub fingerprint: [u8; 32],
    /// The server name that was being verified.
    pub server_name: String,
}

impl ServerCertInfo {
    /// Create a new ServerCertInfo from a certificate and server name.
    pub fn new(cert_der: &[u8], server_name: &str) -> Self {
        Self {
            certificate_der: cert_der.to_vec(),
            fingerprint: compute_sha256_fingerprint(cert_der),
            server_name: server_name.to_string(),
        }
    }

    /// Get a hex-encoded fingerprint string for display.
    pub fn fingerprint_hex(&self) -> String {
        self.fingerprint
            .iter()
            .map(|b| format!("{:02X}", b))
            .collect::<Vec<_>>()
            .chunks(2)
            .map(|c| c.join(""))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// Get a shortened fingerprint for compact display.
    pub fn fingerprint_short(&self) -> String {
        self.fingerprint
            .iter()
            .take(8)
            .map(|b| format!("{:02X}", b))
            .collect::<Vec<_>>()
            .join(":")
    }
}

impl std::fmt::Display for ServerCertInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Certificate for '{}' (fingerprint: {}...)",
            self.server_name,
            self.fingerprint_short()
        )
    }
}

impl std::error::Error for ServerCertInfo {}

/// Compute the SHA256 fingerprint of a DER-encoded certificate.
pub fn compute_sha256_fingerprint(cert_der: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(cert_der);
    hasher.finalize().into()
}

/// A certificate verifier that captures self-signed certificates for user confirmation.
///
/// This verifier:
/// 1. Delegates to the standard WebPKI verifier for normal certificates
/// 2. When verification fails due to unknown issuer (self-signed cert), it stores
///    the certificate info in `captured_cert` for later retrieval by the caller
/// 3. The caller can then prompt the user and retry with the certificate added to trust
pub struct InteractiveCertVerifier {
    /// Root certificates to trust.
    root_store: Arc<RootCertStore>,
    /// Crypto provider for signature verification.
    provider: Arc<CryptoProvider>,
    /// Storage for the captured certificate when verification fails.
    /// The caller should check this after a connection error.
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

    /// Get a reference to the root store.
    pub fn root_store(&self) -> &RootCertStore {
        &self.root_store
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
        // Build a WebPKI verifier for delegation
        let inner =
            rustls::client::WebPkiServerVerifier::builder_with_provider(self.root_store.clone(), self.provider.clone())
                .build()
                .map_err(|e| Error::General(format!("Failed to build verifier: {}", e)))?;

        // Try normal verification
        match inner.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now) {
            Ok(verified) => {
                // Clear any previously captured cert on success
                if let Ok(mut captured) = self.captured_cert.lock() {
                    *captured = None;
                }
                Ok(verified)
            }
            Err(Error::InvalidCertificate(CertificateError::UnknownIssuer)) => {
                // Self-signed or unknown issuer - capture the cert info
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

                // Store the captured cert for the caller to retrieve
                if let Ok(mut captured) = self.captured_cert.lock() {
                    *captured = Some(cert_info.clone());
                }

                // Return an error to fail the TLS handshake
                Err(Error::InvalidCertificate(CertificateError::UnknownIssuer))
            }
            Err(Error::InvalidCertificate(CertificateError::BadSignature)) => {
                // Also treat BadSignature as potentially a self-signed cert issue
                // This can happen when the cert is not signed by a trusted CA
                let server_name_str = match server_name {
                    ServerName::DnsName(name) => name.as_ref().to_string(),
                    ServerName::IpAddress(ip) => format!("{:?}", ip),
                    _ => "unknown".to_string(),
                };

                let cert_info = ServerCertInfo::new(end_entity.as_ref(), &server_name_str);

                tracing::warn!(
                    "Certificate with bad signature for '{}' - may be self-signed (fingerprint: {})",
                    cert_info.server_name,
                    cert_info.fingerprint_hex()
                );

                // Store the captured cert for the caller to retrieve
                if let Ok(mut captured) = self.captured_cert.lock() {
                    *captured = Some(cert_info.clone());
                }

                Err(Error::InvalidCertificate(CertificateError::BadSignature))
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

/// Retrieve the captured certificate from the shared state.
///
/// This should be called after a connection attempt fails to get the
/// certificate that caused the verification failure.
pub fn take_captured_cert(captured: &CapturedCert) -> Option<ServerCertInfo> {
    captured.lock().ok()?.take()
}

/// Check if there's a captured certificate without removing it.
pub fn peek_captured_cert(captured: &CapturedCert) -> Option<ServerCertInfo> {
    captured.lock().ok()?.clone()
}

/// Check if an error indicates a certificate verification failure that we should handle.
///
/// This checks for both UnknownIssuer and BadSignature errors, which are the cases
/// where we would have captured a certificate for user confirmation.
pub fn is_cert_verification_error(error: &anyhow::Error) -> bool {
    for cause in error.chain() {
        // Check for rustls errors
        if let Some(rustls_err) = cause.downcast_ref::<Error>() {
            match rustls_err {
                Error::InvalidCertificate(CertificateError::UnknownIssuer)
                | Error::InvalidCertificate(CertificateError::BadSignature) => {
                    return true;
                }
                _ => {}
            }
        }

        // Check for quinn connection errors that wrap TLS errors
        if let Some(conn_err) = cause.downcast_ref::<quinn::ConnectionError>() {
            if let quinn::ConnectionError::TransportError(te) = conn_err {
                let reason = te.to_string();
                if reason.contains("UnknownIssuer")
                    || reason.contains("BadSignature")
                    || reason.contains("invalid peer certificate")
                {
                    return true;
                }
            }
        }

        // Check error message as a fallback
        let msg = cause.to_string();
        if msg.contains("UnknownIssuer") || msg.contains("BadSignature") || msg.contains("invalid peer certificate") {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fingerprint_formatting() {
        let cert_der = [0u8; 100]; // Dummy cert data
        let info = ServerCertInfo::new(&cert_der, "example.com");

        // Fingerprint should be 32 bytes (SHA256)
        assert_eq!(info.fingerprint.len(), 32);

        // Hex fingerprint should be formatted with colons
        let hex = info.fingerprint_hex();
        assert!(hex.contains(':'));

        // Short fingerprint should be truncated
        let short = info.fingerprint_short();
        assert!(short.len() < hex.len());
    }

    #[test]
    fn test_display() {
        let cert_der = [0u8; 100];
        let info = ServerCertInfo::new(&cert_der, "test.server.com");
        let display = format!("{}", info);
        assert!(display.contains("test.server.com"));
        assert!(display.contains("fingerprint"));
    }

    #[test]
    fn test_captured_cert() {
        let captured = new_captured_cert();

        // Initially empty
        assert!(take_captured_cert(&captured).is_none());

        // Store a cert
        let cert_info = ServerCertInfo::new(&[1, 2, 3, 4, 5], "test.server.com");
        {
            let mut lock = captured.lock().unwrap();
            *lock = Some(cert_info);
        }

        // Peek without removing
        let peeked = peek_captured_cert(&captured);
        assert!(peeked.is_some());
        assert_eq!(peeked.unwrap().server_name, "test.server.com");

        // Should still be there
        let peeked2 = peek_captured_cert(&captured);
        assert!(peeked2.is_some());

        // Take removes it
        let taken = take_captured_cert(&captured);
        assert!(taken.is_some());
        assert_eq!(taken.unwrap().server_name, "test.server.com");

        // Now it's gone
        assert!(take_captured_cert(&captured).is_none());
    }
}
