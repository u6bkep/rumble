use std::sync::Arc;

use anyhow::Result;
use rcgen::{CertificateParams, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::TlsAcceptor;
use tracing::info;

/// Generate a self-signed TLS certificate and return a TLS acceptor.
pub fn make_tls_acceptor() -> Result<TlsAcceptor> {
    info!("Generating self-signed TLS certificate for Mumble server");

    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let mut params = CertificateParams::new(vec!["mumble-bridge".to_string()])?;
    params.distinguished_name.push(
        rcgen::DnType::CommonName,
        rcgen::DnValue::Utf8String("Rumble Mumble Bridge".to_string()),
    );
    let cert = params.self_signed(&key_pair)?;

    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));

    let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(rustls::ALL_VERSIONS)?
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}
