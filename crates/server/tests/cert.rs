use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rcgen::generate_simple_self_signed;

#[test]
fn generate_self_signed_der_is_valid_for_rustls() {
    let ck = generate_simple_self_signed(["localhost".into()]).expect("generate cert");
    let cert_der = ck.cert; // DER bytes
    let key_der = ck.signing_key.serialize_der();
    let pkcs8 = rustls::pki_types::PrivatePkcs8KeyDer::from(key_der);
    let cert = CertificateDer::from(cert_der);
    let key = PrivateKeyDer::from(pkcs8);
    let server_cfg = rustls::ServerConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(vec![cert], key);
    assert!(server_cfg.is_ok(), "rustls accepted self signed cert DER bytes");
}
