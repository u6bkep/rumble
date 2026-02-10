//! CLI RPC client for sending commands to a running Rumble instance.

use std::path::PathBuf;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

/// Run an RPC command against a running Rumble instance.
pub async fn run_rpc_command(socket_path: PathBuf, command: &str) -> anyhow::Result<()> {
    let request = match command {
        "status" => r#"{"method":"get_status"}"#.to_string(),
        "state" => r#"{"method":"get_state"}"#.to_string(),
        "mute" => r#"{"method":"set_muted","muted":true}"#.to_string(),
        "unmute" => r#"{"method":"set_muted","muted":false}"#.to_string(),
        "deafen" => r#"{"method":"set_deafened","deafened":true}"#.to_string(),
        "undeafen" => r#"{"method":"set_deafened","deafened":false}"#.to_string(),
        "disconnect" => r#"{"method":"disconnect"}"#.to_string(),
        "start-transmit" => r#"{"method":"start_transmit"}"#.to_string(),
        "stop-transmit" => r#"{"method":"stop_transmit"}"#.to_string(),
        cmd if cmd.starts_with("join-room ") => {
            let uuid = cmd.strip_prefix("join-room ").unwrap();
            serde_json::json!({"method": "join_room", "room_id": uuid}).to_string()
        }
        cmd if cmd.starts_with("send-chat ") => {
            let text = cmd.strip_prefix("send-chat ").unwrap();
            serde_json::json!({"method": "send_chat", "text": text}).to_string()
        }
        cmd if cmd.starts_with("create-room ") => {
            let name = cmd.strip_prefix("create-room ").unwrap();
            serde_json::json!({"method": "create_room", "name": name}).to_string()
        }
        cmd if cmd.starts_with("delete-room ") => {
            let uuid = cmd.strip_prefix("delete-room ").unwrap();
            serde_json::json!({"method": "delete_room", "room_id": uuid}).to_string()
        }
        cmd if cmd.starts_with("share-file ") => {
            let path = cmd.strip_prefix("share-file ").unwrap();
            serde_json::json!({"method": "share_file", "path": path}).to_string()
        }
        cmd if cmd.starts_with("download ") => {
            let magnet = cmd.strip_prefix("download ").unwrap();
            serde_json::json!({"method": "download_file", "magnet": magnet}).to_string()
        }
        cmd if cmd.starts_with("mute-user ") => {
            let user_id: u64 = cmd
                .strip_prefix("mute-user ")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid user ID: {}", e))?;
            serde_json::json!({"method": "mute_user", "user_id": user_id}).to_string()
        }
        cmd if cmd.starts_with("unmute-user ") => {
            let user_id: u64 = cmd
                .strip_prefix("unmute-user ")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid user ID: {}", e))?;
            serde_json::json!({"method": "unmute_user", "user_id": user_id}).to_string()
        }
        _ => anyhow::bail!("Unknown RPC command: {}", command),
    };

    let stream = UnixStream::connect(&socket_path).await?;
    let (reader, mut writer) = stream.into_split();

    let mut line_buf = String::new();
    let mut buf_reader = BufReader::new(reader);

    // Send the request
    writer.write_all(request.as_bytes()).await?;
    writer.write_all(b"\n").await?;

    // Read the response
    buf_reader.read_line(&mut line_buf).await?;
    print!("{}", line_buf);

    Ok(())
}
