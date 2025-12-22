use anyhow::Result;
use api::proto::{self, envelope::Payload};
use api::{encode_frame, try_decode_frame};
use bytes::BytesMut;
use prost::Message;
use quinn::Endpoint;
use quinn::crypto::rustls::QuicClientConfig;
use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task;
use tracing::{error, info};

/// Configuration for the backend client.
#[derive(Clone, Debug, Default)]
pub struct ConnectConfig {
    /// Additional certificate paths (DER format) to trust for server verification.
    /// These are added to the system root certificates (webpki_roots).
    /// Use this to add self-signed or development certificates.
    pub additional_certs: Vec<PathBuf>,
}

impl ConnectConfig {
    /// Create a new config with default settings (only webpki system roots trusted).
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an additional certificate path to trust (DER format).
    pub fn with_cert(mut self, path: impl Into<PathBuf>) -> Self {
        self.additional_certs.push(path.into());
        self
    }
}

#[derive(Clone, Debug, Default)]
pub struct RoomSnapshot {
    pub rooms: Vec<proto::RoomInfo>,
    pub users: Vec<proto::UserPresence>,
    pub current_room_id: Option<u64>,
}

/// Handle to a connected client.
pub struct Client {
    send: Arc<tokio::sync::Mutex<quinn::SendStream>>,
    events_rx: mpsc::UnboundedReceiver<proto::ServerEvent>,
    pub snapshot: Arc<tokio::sync::Mutex<RoomSnapshot>>, 
}

impl Client {
    /// Connect to the QUIC server at `addr`, open a control stream and send `ClientHello`.
    /// 
    /// # Arguments
    /// * `addr` - Server address (e.g., "127.0.0.1:5000")
    /// * `client_name` - Name to identify this client
    /// * `password` - Optional password for server authentication
    /// * `config` - Connection configuration (certificates, etc.)
    pub async fn connect(addr: &str, client_name: &str, password: Option<&str>, config: ConnectConfig) -> Result<Self> {
        // Initialize logging if not already set up by the application.
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        info!(server_addr = %addr, client_name, "backend: starting connect");
        let endpoint = make_client_endpoint(&config)?;

        let addr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| anyhow::anyhow!("invalid address"))?;

        let conn = endpoint
            .connect(addr, "localhost")?
            .await?;

        info!(remote = %conn.remote_address(), "backend: connected");

        let (send, recv) = conn.open_bi().await?;
        info!("backend: opened bi stream");

        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let snapshot = Arc::new(tokio::sync::Mutex::new(RoomSnapshot::default()));

        // Shared send stream to allow echoing keep-alives from the reader task.
        let send_arc = Arc::new(tokio::sync::Mutex::new(send));

        // Spawn task to read events and echo keep-alives.
        let send_for_reader = send_arc.clone();
        let snapshot_for_reader = snapshot.clone();
        task::spawn(async move {
            let mut buf = BytesMut::new();
            let mut local_recv = recv;
            let _ = async move {
                loop {
                    let mut chunk = [0u8; 1024];
                    let n_opt = local_recv.read(&mut chunk).await?;
                    if n_opt.is_none() {
                        info!("backend: server closed stream");
                        break;
                    }
                    let n = n_opt.unwrap();
                    info!(bytes = n, "backend: received bytes");
                    buf.extend_from_slice(&chunk[..n]);
                    while let Some(frame) = try_decode_frame(&mut buf) {
                        if let Ok(env) = proto::Envelope::decode(&*frame) {
                            if let Some(Payload::ServerEvent(ev)) = env.payload {
                                info!(event = ?ev, "backend: received ServerEvent");
                                // Echo back keep-alive messages to the server.
                                if let Some(proto::server_event::Kind::KeepAlive(k)) = ev.kind.clone() {
                                    let echo_env = proto::Envelope {
                                        state_hash: Vec::new(),
                                        payload: Some(Payload::ServerEvent(proto::ServerEvent {
                                            kind: Some(proto::server_event::Kind::KeepAlive(proto::KeepAlive { epoch_ms: k.epoch_ms })),
                                        })),
                                    };
                                    let echo_frame = encode_frame(&echo_env);
                                    let mut s = send_for_reader.lock().await;
                                    if let Err(e) = s.write_all(&echo_frame).await {
                                        error!("backend: keep-alive echo write failed: {e:?}");
                                    }
                                }
                                // Update local snapshot if room state changes.
                                if let Some(proto::server_event::Kind::RoomStateUpdate(rs)) = ev.kind.clone() {
                                    let mut snap = snapshot_for_reader.lock().await;
                                    snap.rooms = rs.rooms;
                                    snap.users = rs.users;
                                    if snap.current_room_id.is_none() {
                                        // default to Root if present
                                        snap.current_room_id = snap.rooms.iter().find_map(|r| r.id.as_ref().map(|i| i.value)).or(Some(1));
                                    }
                                    info!(rooms = snap.rooms.len(), users = snap.users.len(), "backend: updated snapshot from RoomStateUpdate");
                                }
                                let _ = events_tx.send(ev);
                            }
                        }
                    }
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;
        });

        // Send ClientHello.
        let hello = proto::Envelope {
            state_hash: Vec::new(),
            payload: Some(Payload::ClientHello(proto::ClientHello {
                client_name: client_name.to_string(),
                password: password.unwrap_or("").to_string(),
            })),
        };
        let frame = encode_frame(&hello);
        {
            let mut send_stream = send_arc.lock().await;
            info!(bytes = frame.len(), "backend: sending ClientHello frame");
            send_stream.write_all(&frame).await?;
        }

        // Auto-join Root (room id 1) immediately so server pushes room state.
        {
            let join_env = proto::Envelope {
                state_hash: Vec::new(),
                payload: Some(Payload::JoinRoom(proto::JoinRoom { room_id: Some(proto::RoomId { value: 1 }) })),
            };
            let join_frame = encode_frame(&join_env);
            let mut send_stream = send_arc.lock().await;
            info!("backend: auto-joining room 1 on connect");
            send_stream.write_all(&join_frame).await?;
            let mut snap = snapshot.lock().await;
            snap.current_room_id = Some(1);
        }

        Ok(Client { send: send_arc, events_rx, snapshot })
    }

    /// Send a chat message to the server.
    pub async fn send_chat(&self, sender: &str, text: &str) -> Result<()> {
        let msg = proto::Envelope {
            state_hash: Vec::new(),
            payload: Some(Payload::ChatMessage(proto::ChatMessage {
                sender: sender.to_string(),
                text: text.to_string(),
            })),
        };
        let frame = encode_frame(&msg);
        let mut send = self.send.lock().await;
        info!(bytes = frame.len(), sender, "backend: sending ChatMessage frame");
        send.write_all(&frame).await?;
        Ok(())
    }

    /// Receive the next `ServerEvent` if available.
    pub async fn recv_event(&mut self) -> Option<proto::ServerEvent> {
        self.events_rx.recv().await
    }

    /// Non-blocking attempt to receive a `ServerEvent` if one is queued.
    pub fn try_recv_event(&mut self) -> Option<proto::ServerEvent> {
        self.events_rx.try_recv().ok()
    }

    /// Detach and take ownership of the internal event receiver for custom handling.
    /// After calling this the client no longer exposes events via `recv_event`/`try_recv_event`.
    pub fn take_events_receiver(&mut self) -> mpsc::UnboundedReceiver<proto::ServerEvent> {
        let (_dummy_tx, dummy_rx) = mpsc::unbounded_channel();
        std::mem::replace(&mut self.events_rx, dummy_rx)
    }

    /// Request to join a room by id.
    pub async fn join_room(&self, room_id: u64) -> Result<()> {
        let env = proto::Envelope {
            state_hash: Vec::new(),
            payload: Some(Payload::JoinRoom(proto::JoinRoom { room_id: Some(proto::RoomId { value: room_id }) })),
        };
        let frame = encode_frame(&env);
        let mut send = self.send.lock().await;
        send.write_all(&frame).await?;
        {
            let mut snap = self.snapshot.lock().await;
            snap.current_room_id = Some(room_id);
        }
        Ok(())
    }

    /// Access the current room/user snapshot.
    pub async fn get_snapshot(&self) -> RoomSnapshot {
        self.snapshot.lock().await.clone()
    }

    pub async fn create_room(&self, name: &str) -> Result<()> {
        let env = proto::Envelope { state_hash: Vec::new(), payload: Some(Payload::CreateRoom(proto::CreateRoom { name: name.to_string() })) };
        let frame = encode_frame(&env);
        let mut send = self.send.lock().await; send.write_all(&frame).await?; Ok(())
    }

    pub async fn delete_room(&self, room_id: u64) -> Result<()> {
        let env = proto::Envelope { state_hash: Vec::new(), payload: Some(Payload::DeleteRoom(proto::DeleteRoom { room_id })) };
        let frame = encode_frame(&env);
        let mut send = self.send.lock().await; send.write_all(&frame).await?; Ok(())
    }

    pub async fn rename_room(&self, room_id: u64, new_name: &str) -> Result<()> {
        let env = proto::Envelope { state_hash: Vec::new(), payload: Some(Payload::RenameRoom(proto::RenameRoom { room_id, new_name: new_name.to_string() })) };
        let frame = encode_frame(&env);
        let mut send = self.send.lock().await; send.write_all(&frame).await?; Ok(())
    }
}

fn make_client_endpoint(config: &ConnectConfig) -> Result<Endpoint> {
    let mut endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap())?;

    // Start with webpki system roots.
    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    
    // Load additional configured certificates.
    for cert_path in &config.additional_certs {
        match std::fs::read(cert_path) {
            Ok(cert_bytes) => {
                let cert = rustls::pki_types::CertificateDer::from(cert_bytes);
                let _ = root_store.add(cert);
                info!("backend: loaded additional cert from {:?}", cert_path);
            }
            Err(e) => {
                error!("backend: failed to load cert from {:?}: {}", cert_path, e);
            }
        }
    }

    let mut client_cfg = rustls::ClientConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_root_certificates(root_store)
        .with_no_client_auth();
    client_cfg.alpn_protocols = vec![b"rumble".to_vec()];
    let rustls_config = Arc::new(client_cfg);
    let crypto = QuicClientConfig::try_from(rustls_config)?;
    let client_config = quinn::ClientConfig::new(Arc::new(crypto));
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}
