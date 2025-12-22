use anyhow::Result;
use api::proto::{self, envelope::Payload};
pub use api::proto::VoiceDatagram;
use api::{encode_frame, try_decode_frame};
use bytes::BytesMut;
use prost::Message;
use quinn::Endpoint;
use quinn::crypto::rustls::QuicClientConfig;
use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::sync::mpsc;
use tokio::task;
use tracing::{error, info, debug};

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
    conn: quinn::Connection,
    events_rx: mpsc::UnboundedReceiver<proto::ServerEvent>,
    pub snapshot: Arc<tokio::sync::Mutex<RoomSnapshot>>,
    /// Channel to signal the reader task to stop.
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    /// User ID assigned by the server (set after connection).
    user_id: Arc<tokio::sync::Mutex<Option<u64>>>,
    /// Sequence counter for voice datagrams.
    voice_sequence: AtomicU32,
    /// Channel for received voice datagrams.
    voice_rx: mpsc::UnboundedReceiver<VoiceDatagram>,
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

        // Create shutdown channel (not used yet, but available for graceful shutdown).
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();

        // Create voice datagram channel.
        let (voice_tx, voice_rx) = mpsc::unbounded_channel();
        
        // Spawn task to receive voice datagrams.
        let conn_for_voice = conn.clone();
        task::spawn(async move {
            loop {
                match conn_for_voice.read_datagram().await {
                    Ok(datagram) => {
                        match VoiceDatagram::decode(datagram.as_ref()) {
                            Ok(voice_dgram) => {
                                debug!(
                                    sender = voice_dgram.sender_id,
                                    seq = voice_dgram.sequence,
                                    data_len = voice_dgram.opus_data.len(),
                                    "backend: received voice datagram"
                                );
                                let _ = voice_tx.send(voice_dgram);
                            }
                            Err(e) => {
                                debug!(error = ?e, "backend: failed to decode voice datagram");
                            }
                        }
                    }
                    Err(e) => {
                        debug!(error = ?e, "backend: datagram receive ended");
                        break;
                    }
                }
            }
        });

        Ok(Client { 
            send: send_arc, 
            conn, 
            events_rx, 
            snapshot, 
            shutdown_tx: Some(shutdown_tx),
            user_id: Arc::new(tokio::sync::Mutex::new(None)),
            voice_sequence: AtomicU32::new(0),
            voice_rx,
        })
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

    /// Gracefully disconnect from the server.
    /// Sends a Disconnect message and closes the stream.
    pub async fn disconnect(&self) -> Result<()> {
        info!("backend: sending disconnect");
        let env = proto::Envelope {
            state_hash: Vec::new(),
            payload: Some(Payload::Disconnect(proto::Disconnect {
                reason: "client disconnect".to_string(),
            })),
        };
        let frame = encode_frame(&env);
        let mut send = self.send.lock().await;
        // Send the disconnect message.
        let _ = send.write_all(&frame).await;
        // Finish the stream to signal we're done sending.
        let _ = send.finish();
        // Close the connection explicitly.
        self.conn.close(quinn::VarInt::from_u32(0), b"disconnect");
        Ok(())
    }

    /// Set the user ID for this client (typically assigned by server).
    pub async fn set_user_id(&self, id: u64) {
        *self.user_id.lock().await = Some(id);
    }

    /// Get the user ID for this client.
    pub async fn get_user_id(&self) -> Option<u64> {
        *self.user_id.lock().await
    }

    /// Send a voice datagram with the given opus data.
    /// Uses QUIC datagrams for low-latency unreliable delivery.
    /// 
    /// # Arguments
    /// * `opus_data` - Opus-encoded audio frame bytes
    /// 
    /// # Returns
    /// The sequence number used for this datagram.
    pub fn send_voice_datagram(&self, opus_data: Vec<u8>) -> Result<u32> {
        let seq = self.voice_sequence.fetch_add(1, Ordering::Relaxed);
        let snap = self.snapshot.blocking_lock();
        let room_id = snap.current_room_id.unwrap_or(1);
        drop(snap);
        
        // Get user_id synchronously for the datagram.
        // We use blocking_lock here since this is intended for audio threads.
        let user_id = self.user_id.blocking_lock();
        let sender_id = user_id.unwrap_or(0);
        drop(user_id);
        
        let timestamp_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);
        
        let datagram = VoiceDatagram {
            sender_id,
            room_id,
            sequence: seq,
            timestamp_us,
            opus_data,
        };
        
        let bytes = datagram.encode_to_vec();
        self.conn.send_datagram(bytes.into())?;
        
        debug!(seq, room_id, sender_id, "backend: sent voice datagram");
        Ok(seq)
    }

    /// Send a voice datagram asynchronously.
    pub async fn send_voice_datagram_async(&self, opus_data: Vec<u8>) -> Result<u32> {
        let seq = self.voice_sequence.fetch_add(1, Ordering::Relaxed);
        let snap = self.snapshot.lock().await;
        let room_id = snap.current_room_id.unwrap_or(1);
        drop(snap);
        
        let user_id = self.user_id.lock().await;
        let sender_id = user_id.unwrap_or(0);
        drop(user_id);
        
        let timestamp_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);
        
        let datagram = VoiceDatagram {
            sender_id,
            room_id,
            sequence: seq,
            timestamp_us,
            opus_data,
        };
        
        let bytes = datagram.encode_to_vec();
        self.conn.send_datagram(bytes.into())?;
        
        debug!(seq, room_id, sender_id, "backend: sent voice datagram async");
        Ok(seq)
    }

    /// Take ownership of the voice datagram receiver for custom handling.
    pub fn take_voice_receiver(&mut self) -> mpsc::UnboundedReceiver<VoiceDatagram> {
        let (_dummy_tx, dummy_rx) = mpsc::unbounded_channel();
        std::mem::replace(&mut self.voice_rx, dummy_rx)
    }

    /// Try to receive a voice datagram without blocking.
    pub fn try_recv_voice(&mut self) -> Option<VoiceDatagram> {
        self.voice_rx.try_recv().ok()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Signal shutdown.
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        // Close the QUIC connection immediately.
        // This is synchronous and will signal to the server that we're done.
        self.conn.close(quinn::VarInt::from_u32(0), b"client dropped");
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
    let mut client_config = quinn::ClientConfig::new(Arc::new(crypto));
    
    // Configure transport for faster disconnect detection.
    let mut transport_config = quinn::TransportConfig::default();
    // Idle timeout: close connection if no activity for this duration.
    transport_config.max_idle_timeout(Some(std::time::Duration::from_secs(30).try_into().unwrap()));
    // Keep-alive: send QUIC-level pings to keep the connection alive.
    transport_config.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
    // Enable datagrams for low-latency voice data.
    transport_config.datagram_receive_buffer_size(Some(65536));
    client_config.transport_config(Arc::new(transport_config));
    
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}
