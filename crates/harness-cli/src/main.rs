//! CLI for the Rumble GUI test harness daemon.
//!
//! This tool provides a command-line interface for automated GUI testing.
//! It runs as a daemon that manages GUI client instances and provides
//! screenshot and interaction capabilities.
//!
//! # Usage
//!
//! ```bash
//! # Start the daemon (forks to background)
//! rumble-harness daemon start
//!
//! # Start the server
//! rumble-harness server start
//!
//! # Create a client
//! rumble-harness client new --name bot1 --server 127.0.0.1:5000
//!
//! # Take a screenshot
//! rumble-harness client screenshot 1 --output screenshot.png
//!
//! # Run interaction
//! rumble-harness client click 1 100 200
//! rumble-harness client type 1 "Hello world"
//!
//! # Stop everything
//! rumble-harness daemon stop
//! ```

mod daemon;
mod protocol;
mod renderer;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

use crate::protocol::{Command, CropRect, Response, ResponseData};

#[derive(Parser)]
#[command(name = "rumble-harness")]
#[command(about = "CLI for automated GUI testing of Rumble")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Socket path (default: $XDG_RUNTIME_DIR/rumble-harness.sock)
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Commands {
    /// Daemon management
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },

    /// Server management
    Server {
        #[command(subcommand)]
        action: ServerAction,
    },

    /// Client management and interaction
    Client {
        #[command(subcommand)]
        action: ClientAction,
    },

    /// Get daemon status
    Status,

    /// One-shot setup: start daemon, server, and create a connected client
    Up {
        /// Server port
        #[arg(short, long, default_value = "5000")]
        port: u16,

        /// Client display name
        #[arg(short, long, default_value = "agent")]
        name: String,

        /// Take initial screenshot (saves to specified path)
        #[arg(short, long)]
        screenshot: Option<String>,

        /// Crop region as "x,y,width,height" (e.g., "100,50,400,300")
        #[arg(long)]
        crop: Option<String>,
    },

    /// Clean teardown: close all clients, stop server, stop daemon
    Down,

    /// Rebuild egui-test and take a new screenshot (agent iteration loop)
    Iterate {
        /// Client ID (default: 1)
        #[arg(short, long, default_value = "1")]
        client: u32,

        /// Output screenshot path
        #[arg(short, long, default_value = "/tmp/ui.png")]
        output: String,

        /// Client name for recreation
        #[arg(short, long, default_value = "agent")]
        name: String,

        /// Server address
        #[arg(short, long, default_value = "127.0.0.1:5000")]
        server: String,

        /// Crop region as "x,y,width,height" (e.g., "100,50,400,300")
        #[arg(long)]
        crop: Option<String>,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start the daemon (runs in foreground by default)
    Start {
        /// Fork to background
        #[arg(short, long)]
        background: bool,
    },

    /// Stop the daemon
    Stop,

    /// Check if daemon is running
    Status,
}

#[derive(Subcommand)]
enum ServerAction {
    /// Start the Rumble server
    Start {
        /// Port to listen on
        #[arg(short, long, default_value = "5000")]
        port: u16,
    },

    /// Stop the Rumble server
    Stop,
}

#[derive(Subcommand)]
enum ClientAction {
    /// Create a new GUI client instance
    New {
        /// Client display name
        #[arg(short, long)]
        name: Option<String>,

        /// Server address to connect to
        #[arg(short, long)]
        server: Option<String>,
    },

    /// List all active clients
    List,

    /// Close a client instance
    Close {
        /// Client ID
        id: u32,
    },

    /// Take a screenshot
    Screenshot {
        /// Client ID
        id: u32,

        /// Output file path
        #[arg(short, long)]
        output: Option<String>,

        /// Crop region as "x,y,width,height" (e.g., "100,50,400,300")
        #[arg(long)]
        crop: Option<String>,
    },

    /// Click at a position
    Click {
        /// Client ID
        id: u32,
        /// X coordinate
        x: f32,
        /// Y coordinate
        y: f32,
    },

    /// Move mouse to position
    MouseMove {
        /// Client ID
        id: u32,
        /// X coordinate
        x: f32,
        /// Y coordinate
        y: f32,
    },

    /// Press a key
    KeyPress {
        /// Client ID
        id: u32,
        /// Key name (e.g., "space", "enter", "a")
        key: String,
    },

    /// Release a key
    KeyRelease {
        /// Client ID
        id: u32,
        /// Key name
        key: String,
    },

    /// Tap a key (press and release)
    KeyTap {
        /// Client ID
        id: u32,
        /// Key name
        key: String,
    },

    /// Type text
    Type {
        /// Client ID
        id: u32,
        /// Text to type
        text: String,
    },

    /// Run frames to advance the UI
    Frames {
        /// Client ID
        id: u32,
        /// Number of frames to run
        #[arg(default_value = "1")]
        count: u32,
    },

    /// Check if connected to server
    Connected {
        /// Client ID
        id: u32,
    },

    /// Get backend state as JSON
    State {
        /// Client ID
        id: u32,
    },

    /// Click a widget by its accessible label
    ClickWidget {
        /// Client ID
        id: u32,
        /// Widget label text
        label: String,
    },

    /// Check if a widget with the given label exists
    HasWidget {
        /// Client ID
        id: u32,
        /// Widget label text
        label: String,
    },

    /// Get the bounding rectangle of a widget by label
    WidgetRect {
        /// Client ID
        id: u32,
        /// Widget label text
        label: String,
    },

    /// Run frames until UI settles (animations complete)
    Run {
        /// Client ID
        id: u32,
    },

    /// Enable or disable auto-download
    SetAutoDownload {
        /// Client ID
        id: u32,
        /// Enable auto-download (true/false)
        #[arg(action = clap::ArgAction::Set, value_parser = clap::builder::BoolishValueParser::new())]
        enabled: bool,
    },

    /// Set auto-download rules (JSON array)
    SetAutoDownloadRules {
        /// Client ID
        id: u32,
        /// Rules as JSON: [{"mime_pattern": "image/*", "max_size_bytes": 10485760}]
        rules_json: String,
    },

    /// Get file transfer settings
    GetFileTransferSettings {
        /// Client ID
        id: u32,
    },

    /// Set a keyboard shortcut (hotkey)
    SetHotkey {
        /// Client ID
        id: u32,
        /// Action: "ptt", "mute", or "deafen"
        action: String,
        /// Key name (e.g., "Space", "M", "F1") or empty to clear
        #[arg(default_value = "")]
        key: String,
    },

    /// Share a file
    ShareFile {
        /// Client ID
        id: u32,
        /// Path to the file to share
        path: String,
    },

    /// Get list of file transfers
    GetFileTransfers {
        /// Client ID
        id: u32,
    },

    /// Show or hide the file transfers window
    ShowTransfers {
        /// Client ID
        id: u32,
        /// Show the window (true) or hide it (false)
        #[arg(action = clap::ArgAction::Set, value_parser = clap::builder::BoolishValueParser::new())]
        show: bool,
    },
}

/// Parse a crop string like "x,y,width,height" into a CropRect.
fn parse_crop(s: &str) -> Result<CropRect> {
    let parts: Vec<&str> = s.split(',').collect();
    anyhow::ensure!(
        parts.len() == 4,
        "Crop must be 4 comma-separated values: x,y,width,height"
    );
    Ok(CropRect {
        x: parts[0].trim().parse().context("Invalid crop x value")?,
        y: parts[1].trim().parse().context("Invalid crop y value")?,
        width: parts[2].trim().parse().context("Invalid crop width value")?,
        height: parts[3].trim().parse().context("Invalid crop height value")?,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let socket_path = cli.socket.unwrap_or_else(daemon::socket_path);

    match cli.command {
        Commands::Daemon { action } => handle_daemon(action, &socket_path).await,
        Commands::Server { action } => handle_server(action, &socket_path).await,
        Commands::Client { action } => handle_client(action, &socket_path).await,
        Commands::Status => handle_status(&socket_path).await,
        Commands::Up {
            port,
            name,
            screenshot,
            crop,
        } => {
            let crop = crop.map(|s| parse_crop(&s)).transpose()?;
            handle_up(&socket_path, port, name, screenshot, crop).await
        }
        Commands::Down => handle_down(&socket_path).await,
        Commands::Iterate {
            client,
            output,
            name,
            server,
            crop,
        } => {
            let crop = crop.map(|s| parse_crop(&s)).transpose()?;
            handle_iterate(&socket_path, client, output, name, server, crop).await
        }
    }
}

async fn handle_daemon(action: DaemonAction, socket_path: &PathBuf) -> Result<()> {
    match action {
        DaemonAction::Start { background } => {
            if background {
                // Fork to background using double-fork
                println!("Starting daemon in background...");

                let exe = std::env::current_exe()?;

                std::process::Command::new(&exe)
                    .args(["--socket", &socket_path.to_string_lossy(), "daemon", "start"])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()?;

                // Wait a moment for daemon to start
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                // Check if it's running
                if socket_path.exists() {
                    println!("Daemon started. Socket: {}", socket_path.display());
                } else {
                    eprintln!("Warning: Daemon may not have started correctly");
                }
            } else {
                // Run in foreground
                tracing_subscriber::fmt()
                    .with_env_filter(
                        tracing_subscriber::EnvFilter::from_default_env()
                            .add_directive("harness_cli=debug".parse().unwrap())
                            .add_directive("info".parse().unwrap()),
                    )
                    .init();

                println!("Starting daemon (foreground)...");
                println!("Socket: {}", socket_path.display());
                println!("Press Ctrl+C to stop");

                let daemon = daemon::Daemon::new(socket_path.clone());
                daemon.run().await?;
            }
            Ok(())
        }

        DaemonAction::Stop => {
            let response = send_command(socket_path, Command::Shutdown).await?;
            print_response(&response);
            Ok(())
        }

        DaemonAction::Status => {
            match send_command(socket_path, Command::Ping).await {
                Ok(_) => println!("Daemon is running"),
                Err(_) => println!("Daemon is not running"),
            }
            Ok(())
        }
    }
}

async fn handle_server(action: ServerAction, socket_path: &PathBuf) -> Result<()> {
    let cmd = match action {
        ServerAction::Start { port } => Command::ServerStart { port },
        ServerAction::Stop => Command::ServerStop,
    };

    let response = send_command(socket_path, cmd).await?;
    print_response(&response);
    Ok(())
}

async fn handle_client(action: ClientAction, socket_path: &PathBuf) -> Result<()> {
    let cmd = match action {
        ClientAction::New { name, server } => Command::ClientNew { name, server },
        ClientAction::List => Command::ClientList,
        ClientAction::Close { id } => Command::ClientClose { id },
        ClientAction::Screenshot { id, output, crop } => {
            let crop = crop.map(|s| parse_crop(&s)).transpose()?;
            Command::Screenshot { id, output, crop }
        }
        ClientAction::Click { id, x, y } => Command::Click { id, x, y },
        ClientAction::MouseMove { id, x, y } => Command::MouseMove { id, x, y },
        ClientAction::KeyPress { id, key } => Command::KeyPress { id, key },
        ClientAction::KeyRelease { id, key } => Command::KeyRelease { id, key },
        ClientAction::KeyTap { id, key } => Command::KeyTap { id, key },
        ClientAction::Type { id, text } => Command::TypeText { id, text },
        ClientAction::Frames { id, count } => Command::RunFrames { id, count },
        ClientAction::Connected { id } => Command::IsConnected { id },
        ClientAction::State { id } => Command::GetState { id },
        ClientAction::ClickWidget { id, label } => Command::ClickWidget { id, label },
        ClientAction::HasWidget { id, label } => Command::HasWidget { id, label },
        ClientAction::WidgetRect { id, label } => Command::WidgetRect { id, label },
        ClientAction::Run { id } => Command::Run { id },
        ClientAction::SetAutoDownload { id, enabled } => Command::SetAutoDownload { id, enabled },
        ClientAction::SetAutoDownloadRules { id, rules_json } => {
            let rules: Vec<protocol::AutoDownloadRuleConfig> = serde_json::from_str(&rules_json).context(
                "Invalid JSON for rules. Expected: [{\"mime_pattern\": \"image/*\", \"max_size_bytes\": 10485760}]",
            )?;
            Command::SetAutoDownloadRules { id, rules }
        }
        ClientAction::GetFileTransferSettings { id } => Command::GetFileTransferSettings { id },
        ClientAction::SetHotkey { id, action, key } => Command::SetHotkey {
            id,
            action,
            key: if key.is_empty() { None } else { Some(key) },
        },
        ClientAction::ShareFile { id, path } => Command::ShareFile { id, path },
        ClientAction::GetFileTransfers { id } => Command::GetFileTransfers { id },
        ClientAction::ShowTransfers { id, show } => Command::ShowTransfers { id, show },
    };

    let response = send_command(socket_path, cmd).await?;
    print_response(&response);
    Ok(())
}

/// Send a command to the daemon and get the response.
async fn send_command(socket_path: &PathBuf, cmd: Command) -> Result<Response> {
    let stream = UnixStream::connect(socket_path)
        .await
        .context("Failed to connect to daemon. Is it running? (rumble-harness daemon start)")?;

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Send command
    let cmd_json = serde_json::to_string(&cmd)?;
    writer.write_all(cmd_json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;

    // Read response
    let mut line = String::new();
    reader.read_line(&mut line).await?;

    let response: Response = serde_json::from_str(&line)?;
    Ok(response)
}

/// Handle the `status` command with enhanced output.
async fn handle_status(socket_path: &PathBuf) -> Result<()> {
    // Check if daemon is running
    let daemon_running = send_command(socket_path, Command::Ping).await.is_ok();

    if !daemon_running {
        println!("Daemon:  not running");
        println!("Server:  unknown (daemon not running)");
        println!("Clients: unknown (daemon not running)");
        println!("\nRun `rumble-harness up` to start everything.");
        return Ok(());
    }

    // Get detailed status
    let response = send_command(socket_path, Command::Status).await?;
    if let Response::Ok {
        data: ResponseData::Status {
            server_running,
            client_count,
        },
    } = &response
    {
        println!("Daemon:  running (socket: {})", socket_path.display());
        println!("Server:  {}", if *server_running { "running" } else { "not running" });
        println!("Clients: {}", client_count);

        // List clients if any
        if *client_count > 0 {
            let list_response = send_command(socket_path, Command::ClientList).await?;
            if let Response::Ok {
                data: ResponseData::ClientList { clients },
            } = list_response
            {
                for client in clients {
                    println!("  [{}] {} (connected: {})", client.id, client.name, client.connected);
                }
            }
        }

        if !server_running {
            println!("\nHint: Run `rumble-harness server start` to start the server.");
        }
    } else {
        print_response(&response);
    }

    Ok(())
}

/// Handle the `up` command - one-shot setup.
async fn handle_up(
    socket_path: &PathBuf,
    port: u16,
    name: String,
    screenshot: Option<String>,
    crop: Option<CropRect>,
) -> Result<()> {
    // Step 1: Start daemon if not running
    let daemon_running = send_command(socket_path, Command::Ping).await.is_ok();
    if !daemon_running {
        println!("Starting daemon...");
        let exe = std::env::current_exe()?;
        std::process::Command::new(&exe)
            .args(["--socket", &socket_path.to_string_lossy(), "daemon", "start"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;

        // Wait for daemon to be ready
        for i in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if send_command(socket_path, Command::Ping).await.is_ok() {
                break;
            }
            if i == 19 {
                anyhow::bail!("Daemon failed to start within 2 seconds");
            }
        }
        println!("  Daemon started.");
    } else {
        println!("Daemon already running.");
    }

    // Step 2: Start server if not running
    let status_response = send_command(socket_path, Command::Status).await?;
    let server_running = if let Response::Ok {
        data: ResponseData::Status { server_running, .. },
    } = &status_response
    {
        *server_running
    } else {
        false
    };

    if !server_running {
        println!("Starting server on port {}...", port);
        let response = send_command(socket_path, Command::ServerStart { port }).await?;
        if let Response::Error { message } = response {
            anyhow::bail!("Failed to start server: {}", message);
        }
        // Give server time to bind
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        println!("  Server started.");
    } else {
        println!("Server already running.");
    }

    // Step 3: Create client
    println!("Creating client '{}'...", name);
    let server_addr = format!("127.0.0.1:{}", port);
    let response = send_command(
        socket_path,
        Command::ClientNew {
            name: Some(name),
            server: Some(server_addr),
        },
    )
    .await?;

    let client_id = if let Response::Ok {
        data: ResponseData::ClientCreated { id },
    } = &response
    {
        *id
    } else {
        print_response(&response);
        anyhow::bail!("Failed to create client");
    };
    println!("  Client created (id: {}).", client_id);

    // Step 4: Wait for UI to stabilize and optionally take screenshot
    println!("Waiting for UI to stabilize...");
    let _ = send_command(socket_path, Command::Run { id: client_id }).await?;

    // Wait a bit for connection
    for _ in 0..10 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let _ = send_command(
            socket_path,
            Command::RunFrames {
                id: client_id,
                count: 5,
            },
        )
        .await;
    }

    // Check connection status
    let conn_response = send_command(socket_path, Command::IsConnected { id: client_id }).await?;
    let connected = if let Response::Ok {
        data: ResponseData::Connected { connected },
    } = &conn_response
    {
        *connected
    } else {
        false
    };

    if connected {
        println!("  Client connected to server.");
    } else {
        println!("  Client not yet connected (may still be connecting).");
    }

    // Step 5: Take screenshot if requested
    if let Some(output_path) = screenshot {
        let response = send_command(
            socket_path,
            Command::Screenshot {
                id: client_id,
                output: Some(output_path.clone()),
                crop,
            },
        )
        .await?;
        if let Response::Ok {
            data: ResponseData::Screenshot { path, width, height },
        } = &response
        {
            println!("Screenshot: {} ({}x{})", path, width, height);
        } else {
            print_response(&response);
        }
    }

    println!("\nReady! Client ID: {}", client_id);
    println!(
        "  Take screenshot: rumble-harness client screenshot {} -o ui.png",
        client_id
    );
    println!("  Rebuild & screenshot: rumble-harness iterate -c {}", client_id);

    Ok(())
}

/// Handle the `down` command - clean teardown.
async fn handle_down(socket_path: &PathBuf) -> Result<()> {
    // Check if daemon is running
    if send_command(socket_path, Command::Ping).await.is_err() {
        println!("Daemon not running, nothing to do.");
        return Ok(());
    }

    // Get list of clients and close them
    let list_response = send_command(socket_path, Command::ClientList).await?;
    if let Response::Ok {
        data: ResponseData::ClientList { clients },
    } = list_response
    {
        for client in clients {
            println!("Closing client {}...", client.id);
            let _ = send_command(socket_path, Command::ClientClose { id: client.id }).await;
        }
    }

    // Stop server
    println!("Stopping server...");
    let _ = send_command(socket_path, Command::ServerStop).await;

    // Stop daemon
    println!("Stopping daemon...");
    let _ = send_command(socket_path, Command::Shutdown).await;

    println!("Done.");
    Ok(())
}

/// Handle the `iterate` command - rebuild and screenshot.
async fn handle_iterate(
    socket_path: &PathBuf,
    client_id: u32,
    output: String,
    name: String,
    server: String,
    crop: Option<CropRect>,
) -> Result<()> {
    // Check daemon is running
    if send_command(socket_path, Command::Ping).await.is_err() {
        anyhow::bail!("Daemon not running. Run `rumble-harness up` first, or `rumble-harness daemon start`.");
    }

    // Step 1: Close the client (ignore errors if it doesn't exist)
    println!("Closing client {}...", client_id);
    let _ = send_command(socket_path, Command::ClientClose { id: client_id }).await;

    // Step 2: Rebuild egui-test
    println!("Building egui-test...");
    let build_status = std::process::Command::new("cargo")
        .args(["build", "-p", "egui-test"])
        .status()
        .context("Failed to run cargo build")?;

    if !build_status.success() {
        anyhow::bail!("Build failed! Fix errors and try again.");
    }
    println!("  Build succeeded.");

    // Step 3: Create new client
    println!("Creating new client '{}'...", name);
    let response = send_command(
        socket_path,
        Command::ClientNew {
            name: Some(name),
            server: Some(server),
        },
    )
    .await?;

    let new_client_id = if let Response::Ok {
        data: ResponseData::ClientCreated { id },
    } = &response
    {
        *id
    } else {
        print_response(&response);
        anyhow::bail!("Failed to create client");
    };
    println!("  Client created (id: {}).", new_client_id);

    // Step 4: Wait for UI to stabilize
    println!("Waiting for UI to stabilize...");
    let _ = send_command(socket_path, Command::Run { id: new_client_id }).await?;

    // Additional frames for connection
    for _ in 0..10 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let _ = send_command(
            socket_path,
            Command::RunFrames {
                id: new_client_id,
                count: 5,
            },
        )
        .await;
    }

    // Step 5: Take screenshot
    let response = send_command(
        socket_path,
        Command::Screenshot {
            id: new_client_id,
            output: Some(output.clone()),
            crop,
        },
    )
    .await?;

    if let Response::Ok {
        data: ResponseData::Screenshot { path, width, height },
    } = &response
    {
        println!("\nScreenshot: {} ({}x{})", path, width, height);
        println!("Client ID: {}", new_client_id);
    } else {
        print_response(&response);
    }

    Ok(())
}

/// Print a response to stdout.
fn print_response(response: &Response) {
    match response {
        Response::Ok { data } => match data {
            ResponseData::Ack => {
                println!("OK");
            }
            ResponseData::Pong => {
                println!("pong");
            }
            ResponseData::Status {
                server_running,
                client_count,
            } => {
                println!("Server running: {}", server_running);
                println!("Active clients: {}", client_count);
            }
            ResponseData::ClientCreated { id } => {
                println!("Client created: {}", id);
            }
            ResponseData::ClientList { clients } => {
                if clients.is_empty() {
                    println!("No active clients");
                } else {
                    println!("Active clients:");
                    for client in clients {
                        println!("  [{}] {} (connected: {})", client.id, client.name, client.connected);
                    }
                }
            }
            ResponseData::Screenshot { path, width, height } => {
                println!("Screenshot saved: {} ({}x{})", path, width, height);
            }
            ResponseData::Connected { connected } => {
                println!("{}", connected);
            }
            ResponseData::State { state } => {
                println!("{}", serde_json::to_string_pretty(state).unwrap());
            }
            ResponseData::FramesRun { count } => {
                println!("Ran {} frames", count);
            }
            ResponseData::WidgetExists { exists } => {
                println!("{}", exists);
            }
            ResponseData::WidgetRect {
                found,
                x,
                y,
                width,
                height,
            } => {
                if *found {
                    println!(
                        "x: {}, y: {}, width: {}, height: {}",
                        x.unwrap_or(0.0),
                        y.unwrap_or(0.0),
                        width.unwrap_or(0.0),
                        height.unwrap_or(0.0)
                    );
                } else {
                    println!("Widget not found");
                }
            }
            ResponseData::FileTransferSettings {
                auto_download_enabled,
                auto_download_rules,
            } => {
                println!("Auto-download enabled: {}", auto_download_enabled);
                println!("Rules:");
                for rule in auto_download_rules {
                    println!("  {} (max {} bytes)", rule.mime_pattern, rule.max_size_bytes);
                }
            }
            ResponseData::FileShared { infohash, magnet_link } => {
                println!("File shared!");
                println!("Infohash: {}", infohash);
                println!("Magnet: {}", magnet_link);
            }
            ResponseData::FileTransfers { transfers } => {
                if transfers.is_empty() {
                    println!("No file transfers");
                } else {
                    println!("File transfers:");
                    for t in transfers {
                        println!(
                            "  [{}] {} ({:.1}% - {})",
                            &t.infohash[..8],
                            t.name,
                            t.progress * 100.0,
                            t.state
                        );
                    }
                }
            }
        },
        Response::Error { message } => {
            eprintln!("Error: {}", message);
            std::process::exit(1);
        }
    }
}
