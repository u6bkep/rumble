//! TLS identity for the Mumble-facing listener.
//!
//! Mumble clients trust servers TOFU-style: they remember the certificate and
//! warn loudly whenever it changes. Generating a fresh self-signed cert on
//! every boot therefore re-prompts every user after each bridge restart, so
//! the cert + key are persisted (PEM, key included, `0600` on Unix) and reused
//! verbatim across restarts.

use std::{path::Path, sync::Arc};

use anyhow::{Context, Result};
use rcgen::{CertificateParams, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;
use tracing::info;

/// Load (or first-run generate) the Mumble TLS identity and return an acceptor.
pub fn make_tls_acceptor(persist_path: &Path) -> Result<TlsAcceptor> {
    let (cert_der, key_der) = load_or_create_cert(persist_path)?;

    let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .with_protocol_versions(rustls::ALL_VERSIONS)?
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert_der)],
            PrivateKeyDer::Pkcs8(key_der.into()),
        )?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Load the persisted cert + key, or generate a self-signed pair and persist it.
fn load_or_create_cert(path: &Path) -> Result<(Vec<u8>, Vec<u8>)> {
    if path.exists() {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading Mumble TLS {}", path.display()))?;
        let cert =
            pem_decode(&text, "CERTIFICATE").with_context(|| format!("no CERTIFICATE block in {}", path.display()))?;
        let key =
            pem_decode(&text, "PRIVATE KEY").with_context(|| format!("no PRIVATE KEY block in {}", path.display()))?;
        info!(path = %path.display(), "Loaded persisted Mumble TLS certificate");
        return Ok((cert, key));
    }

    info!(path = %path.display(), "Generating self-signed TLS certificate for Mumble server");
    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let mut params = CertificateParams::new(vec!["mumble-bridge".to_string()])?;
    params.distinguished_name.push(
        rcgen::DnType::CommonName,
        rcgen::DnValue::Utf8String("Rumble Mumble Bridge".to_string()),
    );
    let cert = params.self_signed(&key_pair)?;

    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialize_der();

    let pem = format!(
        "{}{}",
        pem_encode("CERTIFICATE", &cert_der),
        pem_encode("PRIVATE KEY", &key_der)
    );
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).with_context(|| format!("creating TLS dir {}", absolute(parent).display()))?;
    }
    write_private(path, &pem).with_context(|| {
        format!(
            "writing Mumble TLS cert to {} — the directory must be writable by the bridge's user; pass \
             --mumble-tls-file to relocate it (in containers, point it at the data volume)",
            absolute(path).display()
        )
    })?;

    Ok((cert_der, key_der))
}

/// Resolve a possibly-relative path against the cwd so error messages name the
/// real location (a bare relative path is misleading inside containers).
fn absolute(path: &Path) -> std::path::PathBuf {
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Write `contents` owner-only (`0600` on Unix) since the file holds a private key.
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::{
            io::Write,
            os::unix::fs::{OpenOptionsExt, PermissionsExt},
        };
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        // `.mode()` only applies on creation; fix perms if the file pre-existed.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        file.write_all(contents.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}

fn pem_encode(label: &str, der: &[u8]) -> String {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let b64 = STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ascii"));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

fn pem_decode(text: &str, label: &str) -> Option<Vec<u8>> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = text.find(&begin)? + begin.len();
    let stop = text[start..].find(&end)? + start;
    let b64: String = text[start..stop].chars().filter(|c| !c.is_whitespace()).collect();
    STANDARD.decode(b64).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pem_round_trip() {
        let der = vec![0u8, 1, 2, 255, 128, 64];
        let pem = pem_encode("CERTIFICATE", &der);
        assert_eq!(pem_decode(&pem, "CERTIFICATE").unwrap(), der);
    }

    #[test]
    fn persists_cert_across_loads() {
        let dir = std::env::temp_dir().join(format!("rumble-bridge-tls-test-{}", std::process::id()));
        let path = dir.join("mumble-tls.pem");
        let _ = std::fs::remove_dir_all(&dir);

        let first = load_or_create_cert(&path).expect("create");
        let second = load_or_create_cert(&path).expect("load");
        assert_eq!(first, second, "reloaded TLS identity must match the persisted one");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "TLS file must be 0600 (contains private key)");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
