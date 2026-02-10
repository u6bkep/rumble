/// Parse and encode Mumble UDPTunnel voice packets (Opus over TCP).
///
/// UDPTunnel format (raw bytes, NOT protobuf):
/// ```text
/// byte 0: header byte
///   bits 7-5: type (4 = Opus)
///   bits 4-0: target (0 = normal talking)
/// byte 1+: varint-encoded session ID (sender)
/// byte N+: varint-encoded opus payload length (bottom 13 bits = size, top bit = terminator)
/// byte M+: opus frame data
/// ```

/// Opus type code in the header byte (bits 7-5).
const OPUS_TYPE: u8 = 4;

/// Read a Mumble-style varint from a byte slice, returning (value, bytes_consumed).
fn read_varint(data: &[u8]) -> Option<(u64, usize)> {
    if data.is_empty() {
        return None;
    }

    let first = data[0];
    if first & 0x80 == 0 {
        // 7-bit positive number
        Some((first as u64, 1))
    } else if first & 0xC0 == 0x80 {
        // 14-bit positive number
        if data.len() < 2 {
            return None;
        }
        let val = ((first as u64 & 0x3F) << 8) | data[1] as u64;
        Some((val, 2))
    } else if first & 0xE0 == 0xC0 {
        // 21-bit positive number
        if data.len() < 3 {
            return None;
        }
        let val = ((first as u64 & 0x1F) << 16) | (data[1] as u64) << 8 | data[2] as u64;
        Some((val, 3))
    } else if first & 0xF0 == 0xE0 {
        // 28-bit positive number
        if data.len() < 4 {
            return None;
        }
        let val = ((first as u64 & 0x0F) << 24) | (data[1] as u64) << 16 | (data[2] as u64) << 8 | data[3] as u64;
        Some((val, 4))
    } else if first & 0xF0 == 0xF0 {
        match first & 0x0C {
            0x00 => {
                // 32-bit positive, 4 bytes follow
                if data.len() < 5 {
                    return None;
                }
                let val = (data[1] as u64) << 24 | (data[2] as u64) << 16 | (data[3] as u64) << 8 | data[4] as u64;
                Some((val, 5))
            }
            0x04 => {
                // 64-bit number, 8 bytes follow
                if data.len() < 9 {
                    return None;
                }
                let val = (data[1] as u64) << 56
                    | (data[2] as u64) << 48
                    | (data[3] as u64) << 40
                    | (data[4] as u64) << 32
                    | (data[5] as u64) << 24
                    | (data[6] as u64) << 16
                    | (data[7] as u64) << 8
                    | data[8] as u64;
                Some((val, 9))
            }
            _ => None, // Negative varints not needed
        }
    } else {
        None
    }
}

/// Encode a value as a Mumble-style varint, returning the encoded bytes.
fn encode_varint(val: u64) -> Vec<u8> {
    if val < 0x80 {
        vec![val as u8]
    } else if val < 0x4000 {
        vec![((val >> 8) as u8) | 0x80, val as u8]
    } else if val < 0x200000 {
        vec![((val >> 16) as u8) | 0xC0, (val >> 8) as u8, val as u8]
    } else if val < 0x10000000 {
        vec![
            ((val >> 24) as u8) | 0xE0,
            (val >> 16) as u8,
            (val >> 8) as u8,
            val as u8,
        ]
    } else if val <= u32::MAX as u64 {
        vec![0xF0, (val >> 24) as u8, (val >> 16) as u8, (val >> 8) as u8, val as u8]
    } else {
        vec![
            0xF4,
            (val >> 56) as u8,
            (val >> 48) as u8,
            (val >> 40) as u8,
            (val >> 32) as u8,
            (val >> 24) as u8,
            (val >> 16) as u8,
            (val >> 8) as u8,
            val as u8,
        ]
    }
}

/// A parsed Mumble Opus voice packet.
#[derive(Debug)]
pub struct MumbleVoicePacket {
    /// Target (0 = normal talking, other values = whisper targets).
    pub target: u8,
    /// Session ID of the sender.
    pub session_id: u32,
    /// Raw Opus frame data.
    pub opus_data: Vec<u8>,
    /// Whether this is the last frame in the packet (terminator bit set).
    pub is_last: bool,
}

/// Parse a Mumble UDPTunnel voice packet (raw bytes, not protobuf).
pub fn parse_voice_packet(data: &[u8]) -> Option<MumbleVoicePacket> {
    if data.is_empty() {
        return None;
    }

    let header = data[0];
    let voice_type = (header >> 5) & 0x07;
    let target = header & 0x1F;

    // We only handle Opus (type 4)
    if voice_type != OPUS_TYPE {
        return None;
    }

    let rest = &data[1..];

    // Read session ID
    let (session_id, consumed) = read_varint(rest)?;
    let rest = &rest[consumed..];

    // Read opus payload length (bottom 13 bits = size, bit 13 = terminator)
    let (opus_header, consumed) = read_varint(rest)?;
    let opus_len = (opus_header & 0x1FFF) as usize;
    let is_last = (opus_header & 0x2000) != 0;
    let rest = &rest[consumed..];

    if rest.len() < opus_len {
        return None;
    }

    let opus_data = rest[..opus_len].to_vec();

    Some(MumbleVoicePacket {
        target,
        session_id: session_id as u32,
        opus_data,
        is_last,
    })
}

/// Encode a Mumble Opus voice packet for sending via UDPTunnel.
pub fn encode_voice_packet(session_id: u32, opus_data: &[u8], is_last: bool) -> Vec<u8> {
    let header: u8 = (OPUS_TYPE << 5) | 0; // target 0 = normal
    let session_varint = encode_varint(session_id as u64);

    let mut opus_len = opus_data.len() as u64;
    if is_last {
        opus_len |= 0x2000; // Set terminator bit
    }
    let len_varint = encode_varint(opus_len);

    let mut packet = Vec::with_capacity(1 + session_varint.len() + len_varint.len() + opus_data.len());
    packet.push(header);
    packet.extend_from_slice(&session_varint);
    packet.extend_from_slice(&len_varint);
    packet.extend_from_slice(opus_data);
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_varint_roundtrip() {
        for val in [
            0u64,
            1,
            127,
            128,
            16383,
            16384,
            2097151,
            2097152,
            268435455,
            268435456,
            u32::MAX as u64,
        ] {
            let encoded = encode_varint(val);
            let (decoded, consumed) = read_varint(&encoded).unwrap();
            assert_eq!(decoded, val, "Failed roundtrip for {val}");
            assert_eq!(consumed, encoded.len());
        }
    }

    #[test]
    fn test_voice_packet_roundtrip() {
        let opus_data = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let encoded = encode_voice_packet(42, &opus_data, false);
        let parsed = parse_voice_packet(&encoded).unwrap();
        assert_eq!(parsed.session_id, 42);
        assert_eq!(parsed.opus_data, opus_data);
        assert!(!parsed.is_last);
    }

    #[test]
    fn test_voice_packet_with_terminator() {
        let opus_data = vec![0x01, 0x02];
        let encoded = encode_voice_packet(100, &opus_data, true);
        let parsed = parse_voice_packet(&encoded).unwrap();
        assert_eq!(parsed.session_id, 100);
        assert_eq!(parsed.opus_data, opus_data);
        assert!(parsed.is_last);
    }
}
