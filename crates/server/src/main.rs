use anyhow::Result;
use api::proto::{self, envelope::Payload, RoomId, UserId, RoomInfo, UserPresence, VoiceDatagram};
use api::{encode_frame, try_decode_frame};
use bytes::BytesMut;
use prost::Message;
use quinn::{Endpoint, ServerConfig};
use server::load_or_create_dev_cert;
use std::net::Ipv4Addr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info, debug};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("RUST_LOG"))
        .init();

    let port: u16 = std::env::var("RUMBLE_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(5000);
    let endpoint = make_server_endpoint(port)?;
    let state = Arc::new(ServerState::new());
    info!("server_listen_addr = {}", endpoint.local_addr()?);

    while let Some(connecting) = endpoint.accept().await {
        match connecting.await {
            Ok(new_conn) => {
                info!("new connection from {}", new_conn.remote_address());
                let st = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(new_conn, st).await {
                        error!("connection error: {e:?}");
                    }
                });
            }
            Err(e) => {
                error!("incoming connection failed: {e:?}");
            }
        }
    }

    endpoint.wait_idle().await;
    Ok(())
}

fn make_server_endpoint(port: u16) -> Result<Endpoint> {
    let (cert, key) = load_or_create_dev_cert()?;

    let mut rustls_config = rustls::ServerConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;
    rustls_config.alpn_protocols = vec![b"rumble".to_vec()];

    let mut server_config = ServerConfig::with_crypto(Arc::new(quinn::crypto::rustls::QuicServerConfig::try_from(
        Arc::new(rustls_config),
    )?));
    
    // Configure transport for faster disconnect detection.
    let mut transport_config = quinn::TransportConfig::default();
    // Idle timeout: close connection if no activity for this duration.
    transport_config.max_idle_timeout(Some(std::time::Duration::from_secs(30).try_into().unwrap()));
    // Keep-alive: send QUIC-level pings to keep the connection alive and detect dead peers.
    transport_config.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
    // Enable datagrams for low-latency voice data.
    // Set a reasonable max size for Opus frames (MTU ~1200 bytes minus overhead).
    transport_config.datagram_receive_buffer_size(Some(65536));
    server_config.transport_config(Arc::new(transport_config));

    let addr = (Ipv4Addr::UNSPECIFIED, port).into();
    let endpoint = Endpoint::server(server_config, addr)?;
    Ok(endpoint)
}

// cert persistence logic moved to lib.rs (load_or_create_dev_cert)

#[derive(Clone)]
struct ClientHandle { 
    send: Arc<Mutex<quinn::SendStream>>, 
    username: Arc<Mutex<String>>, 
    user_id: u64,
    conn: quinn::Connection,
}

struct ServerState {
    clients: Mutex<Vec<Arc<ClientHandle>>>,
    rooms: Mutex<Vec<RoomInfo>>, // simple list, id starts at 1
    memberships: Mutex<Vec<(u64 /*user_id*/, u64 /*room_id*/)>>,
    next_user_id: Mutex<u64>,
}

impl ServerState {
    fn new() -> Self {
        Self {
            clients: Mutex::new(Vec::new()),
            rooms: Mutex::new(vec![RoomInfo { id: Some(RoomId { value: 1 }), name: "Root".to_string() }]),
            memberships: Mutex::new(Vec::new()),
            next_user_id: Mutex::new(1),
        }
    }
}

async fn handle_connection(conn: quinn::Connection, state: Arc<ServerState>) -> Result<()> {
    // Track all client handles for this connection so we can clean up when connection closes.
    let mut connection_clients: Vec<Arc<ClientHandle>> = Vec::new();
    
    // Spawn a task to handle incoming datagrams for voice relay.
    let conn_for_datagrams = conn.clone();
    let state_for_datagrams = state.clone();
    tokio::spawn(async move {
        handle_datagrams(conn_for_datagrams, state_for_datagrams).await;
    });
    
    loop {
        match conn.accept_bi().await {
            Ok((send_stream, mut recv)) => {
                info!("new bi stream opened");
                let user_id = {
                    let mut ctr = state.next_user_id.lock().await; let id = *ctr; *ctr += 1; id
                };
                let client_handle = Arc::new(ClientHandle { 
                    send: Arc::new(Mutex::new(send_stream)), 
                    username: Arc::new(Mutex::new(String::new())), 
                    user_id,
                    conn: conn.clone(),
                });
                {
                    let mut clients = state.clients.lock().await;
                    clients.push(client_handle.clone());
                    debug!(total_clients = clients.len(), "server: client registered");
                }
                
                // Track this client for connection-level cleanup.
                connection_clients.push(client_handle.clone());
                
                let mut buf = BytesMut::new();
                
                // Read loop - handle errors gracefully instead of propagating with ?
                // Use a read timeout to detect dead connections faster.
                loop {
                    let mut chunk = [0u8; 1024];
                    let read_result = tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        recv.read(&mut chunk)
                    ).await;
                    
                    match read_result {
                        Ok(Ok(Some(n))) => {
                            debug!(bytes = n, "server: received bytes on stream");
                            buf.extend_from_slice(&chunk[..n]);
                            while let Some(frame) = try_decode_frame(&mut buf) {
                                match proto::Envelope::decode(&*frame) {
                                    Ok(env) => {
                                        debug!(frame_len = frame.len(), "server: decoded envelope frame");
                                        if let Err(e) = handle_envelope(env, client_handle.clone(), state.clone()).await {
                                            error!("handle_envelope error: {e:?}");
                                        }
                                    }
                                    Err(e) => error!("failed to decode envelope: {e:?}"),
                                }
                            }
                        }
                        Ok(Ok(None)) => {
                            info!("stream closed by peer");
                            break;
                        }
                        Ok(Err(e)) => {
                            info!("stream read error (likely disconnect): {e:?}");
                            break;
                        }
                        Err(_) => {
                            // Read timeout - check if connection is still alive by trying to write.
                            info!("read timeout, checking connection health");
                            let env = proto::Envelope { state_hash: Vec::new(), payload: None };
                            let frame = api::encode_frame(&env);
                            let mut send = client_handle.send.lock().await;
                            if send.write_all(&frame).await.is_err() {
                                info!("connection dead after read timeout");
                                break;
                            }
                        }
                    }
                }
                
                // Stream closed - clean up this client.
                cleanup_client(&client_handle, &state).await;
                
                // Remove from connection tracking.
                connection_clients.retain(|h| !Arc::ptr_eq(h, &client_handle));
            }
            Err(e) => {
                // Connection closed (timeout, error, or graceful close).
                info!("connection closed: {e:?}");
                break;
            }
        }
    }
    
    // Connection-level cleanup: clean up any remaining clients from this connection.
    for client_handle in connection_clients {
        cleanup_client(&client_handle, &state).await;
    }
    
    Ok(())
}

/// Clean up a client: remove from clients list, remove memberships, broadcast update.
async fn cleanup_client(client_handle: &Arc<ClientHandle>, state: &Arc<ServerState>) {
    let user_id = client_handle.user_id;
    {
        let mut clients = state.clients.lock().await;
        clients.retain(|h| !Arc::ptr_eq(h, client_handle));
        debug!(remaining_clients = clients.len(), "server: client removed");
    }
    {
        let mut memberships = state.memberships.lock().await;
        memberships.retain(|(uid, _)| *uid != user_id);
        debug!(user_id, "server: removed user membership");
    }
    if let Err(e) = broadcast_room_state(state).await {
        error!("failed to broadcast room state after disconnect: {e:?}");
    }
}

async fn handle_envelope(env: proto::Envelope, sender: Arc<ClientHandle>, state: Arc<ServerState>) -> Result<()> {
    match env.payload {
        Some(Payload::ClientHello(ch)) => {
            info!("ClientHello from {}", ch.client_name);
            if let Ok(required) = std::env::var("RUMBLE_SERVER_PASSWORD") {
                if !required.is_empty() && ch.password != required {
                    error!("authentication failed for {}", ch.client_name);
                    let mut send = sender.send.lock().await;
                    send.reset(quinn::VarInt::from_u32(0)).ok();
                    return Ok(());
                }
            }
            {
                let mut name = sender.username.lock().await; *name = ch.client_name.clone();
            }
            // auto-join Lobby
            {
                let mut m = state.memberships.lock().await; m.push((sender.user_id, 1));
            }
            let reply = proto::Envelope { state_hash: Vec::new(), payload: Some(Payload::ServerHello(proto::ServerHello { server_name: "Rumble Server".to_string() })) };
            let frame = encode_frame(&reply);
            debug!(bytes = frame.len(), "server: sending ServerHello frame");
            {
                let mut send = sender.send.lock().await;
                send.write_all(&frame).await?;
            }

            // send initial RoomState
            let rooms = state.rooms.lock().await.clone();
            let mut users = Vec::new();
            for (uid, rid) in state.memberships.lock().await.iter() {
                // find username for uid
                if let Some(h) = state.clients.lock().await.iter().find(|h| h.user_id == *uid) {
                    let uname = h.username.lock().await.clone();
                    users.push(UserPresence { user_id: Some(UserId { value: *uid }), room_id: Some(RoomId { value: *rid }), username: uname });
                }
            }
            let env = proto::Envelope { state_hash: Vec::new(), payload: Some(Payload::ServerEvent(proto::ServerEvent { kind: Some(proto::server_event::Kind::RoomStateUpdate(proto::RoomState { rooms: rooms.clone(), users: users.clone() })) })) };
            let frame = encode_frame(&env);
            {
                let mut send = sender.send.lock().await;
                info!(rooms = rooms.len(), users = users.len(), "server: sending initial RoomStateUpdate");
                send.write_all(&frame).await?;
            }
        }
        Some(Payload::ChatMessage(msg)) => {
            info!("chat from {}: {}", msg.sender, msg.text);
            // deliver only to members of sender's current room
            let sender_room = state.memberships.lock().await.iter().find(|(uid, _)| *uid == sender.user_id).map(|(_, rid)| *rid).unwrap_or(1);
            let broadcast = proto::Envelope { state_hash: Vec::new(), payload: Some(Payload::ServerEvent(proto::ServerEvent { kind: Some(proto::server_event::Kind::ChatBroadcast(proto::ChatBroadcast { sender: msg.sender, text: msg.text })) })) };
            let frame = encode_frame(&broadcast);
            let clients = state.clients.lock().await;
            for h in clients.iter() {
                let is_member = state.memberships.lock().await.iter().any(|(uid, rid)| *uid == h.user_id && *rid == sender_room);
                if !is_member { continue; }
                let mut send = h.send.lock().await;
                if let Err(e) = send.write_all(&frame).await { error!("broadcast write failed: {e:?}"); }
            }
        }
        Some(Payload::JoinRoom(jr)) => {
            let rid = jr.room_id.and_then(|r| Some(r.value)).unwrap_or(1);
            {
                let mut m = state.memberships.lock().await;
                // replace membership
                m.retain(|(uid, _)| *uid != sender.user_id);
                m.push((sender.user_id, rid));
            }
            // notify room state update
            let rooms = state.rooms.lock().await.clone();
            let mut users = Vec::new();
            for (uid, rid) in state.memberships.lock().await.iter() {
                if let Some(h) = state.clients.lock().await.iter().find(|h| h.user_id == *uid) {
                    let uname = h.username.lock().await.clone();
                    users.push(UserPresence { user_id: Some(UserId { value: *uid }), room_id: Some(RoomId { value: *rid }), username: uname });
                }
            }
            let env = proto::Envelope { state_hash: Vec::new(), payload: Some(Payload::ServerEvent(proto::ServerEvent { kind: Some(proto::server_event::Kind::RoomStateUpdate(proto::RoomState { rooms, users })) })) };
            let frame = encode_frame(&env);
            let clients = state.clients.lock().await;
            for h in clients.iter() {
                let mut send = h.send.lock().await;
                if let Err(e) = send.write_all(&frame).await { error!("room update write failed: {e:?}"); }
            }
        }
        Some(Payload::VoiceFrame(vf)) => {
            // relay datagram-like payload over stream for now to room members
            let rid = vf.room_id.and_then(|r| Some(r.value)).unwrap_or(1);
            let frame_env = proto::Envelope { state_hash: Vec::new(), payload: Some(Payload::ServerEvent(proto::ServerEvent { kind: Some(proto::server_event::Kind::VoiceFrame(vf)) })) };
            let frame = encode_frame(&frame_env);
            let clients = state.clients.lock().await;
            for h in clients.iter() {
                let is_member = state.memberships.lock().await.iter().any(|(uid, r)| *uid == h.user_id && *r == rid);
                if !is_member { continue; }
                let mut send = h.send.lock().await;
                if let Err(e) = send.write_all(&frame).await { error!("voice relay write failed: {e:?}"); }
            }
        }
        Some(Payload::Disconnect(d)) => {
            info!("peer requested disconnect: {}", d.reason);
            let mut send = sender.send.lock().await;
            // Gracefully close our side.
            let _ = send.finish();
        }
        Some(Payload::CreateRoom(cr)) => {
            info!("CreateRoom: {}", cr.name);
            {
                let mut rooms = state.rooms.lock().await;
                let new_id = (rooms.iter().filter_map(|r| r.id.as_ref().map(|i| i.value)).max().unwrap_or(1)) + 1;
                rooms.push(RoomInfo { id: Some(RoomId { value: new_id }), name: cr.name });
            }
            broadcast_room_state(&state).await?;
        }
        Some(Payload::DeleteRoom(dr)) => {
            {
                let mut rooms = state.rooms.lock().await;
                rooms.retain(|r| r.id.as_ref().map(|i| i.value) != Some(dr.room_id));
            }
            {
                let mut memberships = state.memberships.lock().await;
                for (_, rid) in memberships.iter_mut() { if *rid == dr.room_id { *rid = 1; } }
            }
            broadcast_room_state(&state).await?;
        }
        Some(Payload::RenameRoom(rr)) => {
            let mut rooms = state.rooms.lock().await;
            for r in rooms.iter_mut() { if r.id.as_ref().map(|i| i.value) == Some(rr.room_id) { r.name = rr.new_name.clone(); } }
            drop(rooms);
            broadcast_room_state(&state).await?;
        }
        Some(Payload::ServerHello(_) | Payload::ServerEvent(_)) | None => {}
        Some(Payload::Login(_)) | Some(Payload::LeaveRoom(_)) | Some(Payload::RoomStateMsg(_)) => {}
    }
    Ok(())
}

async fn broadcast_room_state(state: &Arc<ServerState>) -> Result<()> {
    let rooms = state.rooms.lock().await.clone();
    let mut users = Vec::new();
    for (uid, rid) in state.memberships.lock().await.iter() {
        if let Some(h) = state.clients.lock().await.iter().find(|h| h.user_id == *uid) {
            let uname = h.username.lock().await.clone();
            users.push(UserPresence { user_id: Some(UserId { value: *uid }), room_id: Some(RoomId { value: *rid }), username: uname });
        }
    }
    let env = proto::Envelope { state_hash: Vec::new(), payload: Some(Payload::ServerEvent(proto::ServerEvent { kind: Some(proto::server_event::Kind::RoomStateUpdate(proto::RoomState { rooms, users })) })) };
    let frame = encode_frame(&env);
    let clients = state.clients.lock().await;
    for h in clients.iter() { let mut send = h.send.lock().await; let _ = send.write_all(&frame).await; }
    Ok(())
}

/// Handle incoming QUIC datagrams for voice relay.
/// Datagrams are relayed to all other clients in the same room.
async fn handle_datagrams(conn: quinn::Connection, state: Arc<ServerState>) {
    loop {
        match conn.read_datagram().await {
            Ok(datagram) => {
                // Decode the VoiceDatagram protobuf.
                match VoiceDatagram::decode(datagram.as_ref()) {
                    Ok(voice_dgram) => {
                        debug!(
                            sender = voice_dgram.sender_id,
                            room = voice_dgram.room_id,
                            seq = voice_dgram.sequence,
                            data_len = voice_dgram.opus_data.len(),
                            "server: received voice datagram"
                        );
                        
                        // Find all clients in the same room (excluding sender).
                        let sender_room = voice_dgram.room_id;
                        let sender_id = voice_dgram.sender_id;
                        
                        // Re-encode for relay (could optimize by just forwarding raw bytes).
                        let relay_bytes = voice_dgram.encode_to_vec();
                        
                        // Get list of clients to relay to.
                        let clients = state.clients.lock().await;
                        let memberships = state.memberships.lock().await;
                        
                        for client in clients.iter() {
                            // Skip the sender.
                            if client.user_id == sender_id {
                                continue;
                            }
                            
                            // Check if this client is in the same room.
                            let is_in_room = memberships.iter()
                                .any(|(uid, rid)| *uid == client.user_id && *rid == sender_room);
                            
                            if is_in_room {
                                // Send datagram to this client.
                                if let Err(e) = client.conn.send_datagram(relay_bytes.clone().into()) {
                                    debug!(
                                        user_id = client.user_id,
                                        error = ?e,
                                        "server: failed to relay voice datagram"
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        debug!(error = ?e, "server: failed to decode voice datagram");
                    }
                }
            }
            Err(e) => {
                // Connection closed or error.
                debug!(error = ?e, "server: datagram receive ended");
                break;
            }
        }
    }
}
