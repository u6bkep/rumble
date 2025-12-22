use tokio::runtime::Runtime;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use quinn::{Endpoint, ClientConfig};
use quinn::crypto::rustls::QuicClientConfig;
use std::sync::Arc;
use std::fs;
use anyhow::Result;
use tracing::{info};

// Bring server functions into scope.
use server::load_or_create_dev_cert;

fn read_der_cert(path: &str) -> CertificateDer<'static> {
    let bytes = fs::read(path).expect("read cert der");
    CertificateDer::from(bytes)
}
fn read_der_key(path: &str) -> PrivateKeyDer<'static> {
    let bytes = fs::read(path).expect("read key der");
    let pkcs8 = rustls::pki_types::PrivatePkcs8KeyDer::from(bytes);
    PrivateKeyDer::from(pkcs8)
}

#[test]
fn persisted_dev_cert_quic_handshake() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    let (_cert, _key) = load_or_create_dev_cert().expect("create cert");
    let rt = Runtime::new().unwrap();
    rt.block_on(async move {
        let cert = read_der_cert("dev-certs/server-cert.der");
        let key = read_der_key("dev-certs/server-key.der");

        let mut rustls_server = rustls::ServerConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
            .with_protocol_versions(&[&rustls::version::TLS13]).unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key).unwrap();
        rustls_server.alpn_protocols = vec![b"rumble".to_vec()];
        let server_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(Arc::new(rustls_server)).unwrap();
        let server_cfg = quinn::ServerConfig::with_crypto(Arc::new(server_crypto));
        let server_endpoint = Endpoint::server(server_cfg, "127.0.0.1:0".parse().unwrap()).unwrap();
        let server_addr = server_endpoint.local_addr().unwrap();
        let server_ep_clone = server_endpoint.clone();
        tokio::spawn(async move {
            if let Some(connecting) = server_ep_clone.accept().await {
                let _ = connecting.await;
            }
        });

        let mut root_store = rustls::RootCertStore::empty();
        root_store.add(cert).unwrap();
        let mut rustls_client_cfg = rustls::ClientConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
            .with_protocol_versions(&[&rustls::version::TLS13]).unwrap()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        rustls_client_cfg.alpn_protocols = vec![b"rumble".to_vec()];
        let client_crypto = QuicClientConfig::try_from(Arc::new(rustls_client_cfg)).unwrap();
        let mut client_endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap()).unwrap();
        client_endpoint.set_default_client_config(ClientConfig::new(Arc::new(client_crypto)));

        let conn = client_endpoint.connect(server_addr, "localhost").unwrap().await.unwrap();
        assert_eq!(conn.remote_address(), server_addr, "handshake should succeed with persisted cert");
    });
}
