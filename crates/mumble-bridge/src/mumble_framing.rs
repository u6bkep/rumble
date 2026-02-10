use anyhow::{Result, bail};
use bytes::{BufMut, BytesMut};
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::mumble_proto::MessageType;

/// A raw Mumble protocol message: type ID + payload bytes.
#[derive(Debug)]
pub struct MumbleMessage {
    pub msg_type: u16,
    pub payload: Vec<u8>,
}

/// Maximum message size (8 MB, same as Mumble default).
const MAX_MESSAGE_SIZE: u32 = 8 * 1024 * 1024;

/// Read a single Mumble-framed message from a TLS stream.
///
/// Frame format: [2-byte type BE][4-byte len BE][payload]
pub async fn read_message<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<MumbleMessage> {
    let msg_type = reader.read_u16().await?;
    let length = reader.read_u32().await?;

    if length > MAX_MESSAGE_SIZE {
        bail!("Message too large: {} bytes (max {})", length, MAX_MESSAGE_SIZE);
    }

    let mut payload = vec![0u8; length as usize];
    reader.read_exact(&mut payload).await?;

    Ok(MumbleMessage { msg_type, payload })
}

/// Write a single Mumble-framed message to a TLS stream.
///
/// Frame format: [2-byte type BE][4-byte len BE][payload]
pub async fn write_message<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    msg_type: MessageType,
    msg: &impl Message,
) -> Result<()> {
    let payload = msg.encode_to_vec();
    let mut frame = BytesMut::with_capacity(6 + payload.len());
    frame.put_u16(msg_type as u16);
    frame.put_u32(payload.len() as u32);
    frame.put_slice(&payload);
    writer.write_all(&frame).await?;
    Ok(())
}

/// Write raw bytes as a UDPTunnel message (voice data is NOT protobuf-encoded).
pub async fn write_udp_tunnel<W: AsyncWriteExt + Unpin>(writer: &mut W, voice_data: &[u8]) -> Result<()> {
    let mut frame = BytesMut::with_capacity(6 + voice_data.len());
    frame.put_u16(MessageType::UdpTunnel as u16);
    frame.put_u32(voice_data.len() as u32);
    frame.put_slice(voice_data);
    writer.write_all(&frame).await?;
    Ok(())
}
