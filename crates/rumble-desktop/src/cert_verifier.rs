//! TLS certificate verification for QUIC connections.
//!
//! Two verifiers:
//! - [`InteractiveCertVerifier`]: the production path. It runs standard WebPKI
//!   verification, but layers two extra behaviours on top for self-signed
//!   servers:
//!     1. **Fingerprint pinning (TOFU re-trust).** Any leaf cert whose SHA-256
//!        fingerprint is in the caller-supplied pin set is accepted regardless
//!        of issuer *or* hostname. This is how a previously-accepted self-signed
//!        cert is trusted on reconnect — independent of the cert's SAN, which a
//!        self-signed dev cert often does not match the dialed host.
//!     2. **Capture for confirmation.** An unknown (untrusted-issuer) cert is
//!        stored in `captured_cert` so the caller can prompt the user, then
//!        retry with the fingerprint pinned.
//! - [`AcceptAllVerifier`]: a danger verifier that accepts any certificate
//!   without verification. Compiled **only** when the
//!   `dangerous-accept-invalid-certs` feature is enabled (server-to-server
//!   daemons like the Mumble bridge opt in); never present in the desktop
//!   client binary.

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

/// A certificate verifier that pins accepted self-signed certs by fingerprint
/// and captures unknown certs for interactive user confirmation.
///
/// See the module docs for the full trust model.
pub struct InteractiveCertVerifier {
    root_store: Arc<RootCertStore>,
    provider: Arc<CryptoProvider>,
    /// SHA-256 fingerprints the user has already accepted. A leaf cert matching
    /// one of these is trusted regardless of issuer or hostname (TOFU pin).
    pinned_fingerprints: Vec<[u8; 32]>,
    /// Storage for an unknown cert captured for user confirmation. `None` when
    /// the caller cannot prompt (e.g. a headless daemon) — unknown certs then
    /// simply fail verification.
    captured_cert: Option<CapturedCert>,
}

impl std::fmt::Debug for InteractiveCertVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InteractiveCertVerifier")
            .field("root_store", &"<RootCertStore>")
            .field("provider", &"<CryptoProvider>")
            .field("pinned_fingerprints", &self.pinned_fingerprints.len())
            .field("captured_cert", &self.captured_cert)
            .finish()
    }
}

impl InteractiveCertVerifier {
    /// Create a new interactive verifier.
    ///
    /// - `root_store` — WebPKI roots (system roots plus any file-provided CAs).
    /// - `pinned_fingerprints` — SHA-256 fingerprints to accept name-independently.
    /// - `captured_cert` — where to stash an unknown cert for the user prompt;
    ///   pass `None` for non-interactive callers.
    pub fn new(
        root_store: RootCertStore,
        provider: Arc<CryptoProvider>,
        pinned_fingerprints: Vec<[u8; 32]>,
        captured_cert: Option<CapturedCert>,
    ) -> Self {
        Self {
            root_store: Arc::new(root_store),
            provider,
            pinned_fingerprints,
            captured_cert,
        }
    }

    fn clear_captured(&self) {
        if let Some(captured) = &self.captured_cert
            && let Ok(mut slot) = captured.lock()
        {
            *slot = None;
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
        // 1. TOFU pin: a fingerprint the user already approved is trusted
        //    regardless of issuer or hostname. This is what makes reconnects to
        //    self-signed servers work even when the cert's SAN doesn't match the
        //    dialed host (e.g. an IP literal, or the dev default SAN "localhost").
        let fingerprint = compute_sha256_fingerprint(end_entity.as_ref());
        if self.pinned_fingerprints.contains(&fingerprint) {
            self.clear_captured();
            return Ok(ServerCertVerified::assertion());
        }

        // 2. Standard WebPKI verification (CA-signed certs for the real host).
        let inner =
            rustls::client::WebPkiServerVerifier::builder_with_provider(self.root_store.clone(), self.provider.clone())
                .build()
                .map_err(|e| Error::General(format!("Failed to build verifier: {}", e)))?;

        match inner.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now) {
            Ok(verified) => {
                self.clear_captured();
                Ok(verified)
            }
            // 3. Untrusted issuer or name mismatch: capture for the user prompt.
            //    Name mismatch is included so a self-signed cert presented for a
            //    host not in its SAN (e.g. dialing by IP a cert whose SAN is
            //    "localhost") becomes a TOFU prompt rather than a hard failure.
            Err(err @ Error::InvalidCertificate(CertificateError::UnknownIssuer))
            | Err(err @ Error::InvalidCertificate(CertificateError::BadSignature))
            | Err(err @ Error::InvalidCertificate(CertificateError::NotValidForName))
            | Err(err @ Error::InvalidCertificate(CertificateError::NotValidForNameContext { .. })) => {
                let server_name_str = match server_name {
                    ServerName::DnsName(name) => name.as_ref().to_string(),
                    ServerName::IpAddress(ip) => format!("{:?}", ip),
                    _ => "unknown".to_string(),
                };

                let cert_info = ServerCertInfo::new(end_entity.as_ref(), &server_name_str);

                tracing::warn!(
                    "Untrusted server certificate for '{}' (fingerprint: {})",
                    cert_info.server_name,
                    cert_info.fingerprint_hex()
                );

                if let Some(captured) = &self.captured_cert
                    && let Ok(mut slot) = captured.lock()
                {
                    *slot = Some(cert_info);
                }

                Err(err)
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

/// A danger verifier that accepts any certificate without verification.
///
/// Compiled only when the `dangerous-accept-invalid-certs` feature is enabled.
/// Intended for server-to-server daemons (e.g. the Mumble bridge) that opt into
/// an unverified link, never for the interactive desktop client.
#[cfg(feature = "dangerous-accept-invalid-certs")]
#[derive(Debug)]
pub struct AcceptAllVerifier {
    provider: Arc<CryptoProvider>,
}

#[cfg(feature = "dangerous-accept-invalid-certs")]
impl Default for AcceptAllVerifier {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "dangerous-accept-invalid-certs")]
impl AcceptAllVerifier {
    pub fn new() -> Self {
        Self {
            provider: Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
        }
    }
}

#[cfg(feature = "dangerous-accept-invalid-certs")]
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
