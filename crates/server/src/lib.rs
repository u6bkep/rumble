use anyhow::Result;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use tracing::info;

/// Generate or load a persistent development self-signed certificate/key pair.
/// Stored as DER in `dev-certs/server-cert.der` and key DER in `dev-certs/server-key.der`.
pub fn load_or_create_dev_cert() -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let cert_path = std::path::Path::new("dev-certs/server-cert.der");
    let key_path = std::path::Path::new("dev-certs/server-key.der");
    if cert_path.exists() && key_path.exists() {
        let cert_bytes = std::fs::read(cert_path)?; // raw DER
        let key_bytes = std::fs::read(key_path)?;   // PKCS8 DER
        info!("Loaded existing dev cert and key (DER)");
        let cert = CertificateDer::from(cert_bytes);
        let pkcs8 = rustls::pki_types::PrivatePkcs8KeyDer::from(key_bytes);
        return Ok((cert, PrivateKeyDer::from(pkcs8)));
    }
    std::fs::create_dir_all("dev-certs").ok();
    let ck = rcgen::generate_simple_self_signed(["localhost".into()])?;
    let pem = ck.cert.pem();
    let der = pem_to_der(&pem)?;
    let key_der = ck.signing_key.serialize_der();
    std::fs::write(cert_path, &der).ok();
    std::fs::write("dev-certs/server-cert.pem", pem.as_bytes()).ok();
    std::fs::write(key_path, key_der.as_slice()).ok();
    let pkcs8 = rustls::pki_types::PrivatePkcs8KeyDer::from(key_der.clone());
    info!("Generated new dev cert and key (DER + PEM)");
    Ok((CertificateDer::from(der), PrivateKeyDer::from(pkcs8)))
}

fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    let mut b64 = String::new();
    for line in pem.lines() {
        if line.starts_with("-----") { continue; }
        b64.push_str(line.trim());
    }
    let der = B64.decode(b64).map_err(|e| anyhow::anyhow!("PEM decode error: {e}"))?;
    Ok(der)
}
