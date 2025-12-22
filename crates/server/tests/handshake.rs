use tokio::runtime::Runtime;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use quinn::{Endpoint, ClientConfig};
use quinn::crypto::rustls::QuicClientConfig;
use rcgen::generate_simple_self_signed;
use std::sync::Arc;

#[test]
fn quic_handshake_with_self_signed_cert() {
    let rt = Runtime::new().unwrap();
    rt.block_on(async move {
        // Generate cert/key
        let ck = generate_simple_self_signed(["localhost".into()]).unwrap();
        let cert_der = ck.cert; // DER
        let key_der = ck.signing_key.serialize_der();
        let pkcs8 = rustls::pki_types::PrivatePkcs8KeyDer::from(key_der);
        let cert = CertificateDer::from(cert_der.clone());
        let key = PrivateKeyDer::from(pkcs8);

        // Server config
        let mut rustls_server = rustls::ServerConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
            .with_protocol_versions(&[&rustls::version::TLS13]).unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key).unwrap();
        rustls_server.alpn_protocols = vec![b"rumble".to_vec()];
        let server_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(Arc::new(rustls_server)).unwrap();
        let server_cfg = quinn::ServerConfig::with_crypto(Arc::new(server_crypto));
        let server_endpoint = Endpoint::server(server_cfg, "127.0.0.1:0".parse().unwrap()).unwrap();
        let server_addr = server_endpoint.local_addr().unwrap();
        // Drive accept loop in background so handshake can complete.
        let server_ep_clone = server_endpoint.clone();
        tokio::spawn(async move {
            if let Some(connecting) = server_ep_clone.accept().await {
                let _ = connecting.await; // ignore result
            }
        });

        // Client root store with self-signed cert
        let mut root_store = rustls::RootCertStore::empty();
        root_store.add(CertificateDer::from(cert_der)).unwrap();
        let mut rustls_client_cfg = rustls::ClientConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
            .with_protocol_versions(&[&rustls::version::TLS13]).unwrap()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        rustls_client_cfg.alpn_protocols = vec![b"rumble".to_vec()];
        let rustls_client = Arc::new(rustls_client_cfg);
        let client_crypto = QuicClientConfig::try_from(rustls_client).unwrap();
        let mut client_endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap()).unwrap();
        client_endpoint.set_default_client_config(ClientConfig::new(Arc::new(client_crypto)));

        // Perform connect
        let conn = client_endpoint.connect(server_addr, "localhost").unwrap().await.unwrap();
        assert_eq!(conn.remote_address(), server_addr);
    });
}
