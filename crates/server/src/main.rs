//! Rumble VOIP Server Binary
//!
//! This is a thin wrapper around the server library that sets up logging
//! and runs the server.
//!
//! # Configuration
//!
//! The server can be configured via:
//! 1. Configuration file (rumble-server.toml) - created with defaults if missing
//! 2. Command-line arguments (override config file)
//!
//! # Subcommands
//!
//! - `add-admin <base64-public-key>` - Add a public key to the admin group
//! - `set-sudo-password <password>` - Set the sudo elevation password
//! - `add-controller <base64-public-key>` - Grant a key MANAGE_PARTICIPANTS (controllers group)
//! - `set-participant-group <base64-public-key> <group>` - Set a controller's default participant group
//!
//! Run with `--help` for available options.

use anyhow::Result;
use clap::Parser;
use server::{
    Config, EchoBotFactory, FileTransferRelayFactory, LinkCleanerFactory, Persistence, Server, ServerConfig,
    config::{CliArgs, Command},
    plugin::PluginFactory,
};
use tracing::{info, warn};

/// Decode a base64-encoded 32-byte Ed25519 public key.
fn decode_ed25519_key(key_b64: &str) -> Result<[u8; 32]> {
    let key_bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, key_b64)
        .map_err(|e| anyhow::anyhow!("Invalid base64 public key: {e}"))?;
    if key_bytes.len() != 32 {
        anyhow::bail!(
            "Public key must be 32 bytes (got {}). Provide a base64-encoded Ed25519 public key.",
            key_bytes.len()
        );
    }
    Ok(key_bytes.try_into().unwrap())
}

/// Outcome of attempting an admin command over the running server's unix
/// admin socket.
#[cfg(unix)]
enum SocketAttempt {
    /// The server handled it (Ok = success message, Err = server-side refusal).
    Handled(std::result::Result<String, String>),
    /// Couldn't talk to a server over the socket (absent/stale/unreadable) —
    /// fall back to opening the database directly.
    Unavailable,
}

/// Try to apply `command` against a running server via its admin socket
/// (`<data_dir>/admin.sock`). The socket speaks the same HTTP admin API as the
/// web admin; local access to the 0600 socket is the credential.
#[cfg(unix)]
async fn try_admin_socket(command: &Command, sock: &std::path::Path) -> SocketAttempt {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    // Re-encode a CLI-provided (standard-base64) key the way route paths
    // expect it (URL-safe, no padding). Invalid keys are rejected here so we
    // never fall back to sled just because the key was malformed.
    let url_key = |key_b64: &str| -> std::result::Result<String, String> {
        decode_ed25519_key(key_b64)
            .map(|k| base64::Engine::encode(&URL_SAFE_NO_PAD, k))
            .map_err(|e| e.to_string())
    };

    let (method, path, body) = match command {
        Command::AddAdmin { public_key } => match url_key(public_key) {
            Ok(key) => (
                "POST",
                format!("/api/registered-users/{key}/groups"),
                serde_json::json!({ "group": "admin", "add": true }),
            ),
            Err(e) => return SocketAttempt::Handled(Err(e)),
        },
        Command::SetSudoPassword { password } => (
            "POST",
            "/api/sudo-password".to_string(),
            serde_json::json!({ "password": password }),
        ),
        Command::AddController { public_key } => (
            "POST",
            "/api/controllers".to_string(),
            serde_json::json!({ "public_key_b64": public_key }),
        ),
        Command::SetParticipantGroup { public_key, group } => match url_key(public_key) {
            Ok(key) => (
                "PUT",
                format!("/api/controllers/{key}/participant-group"),
                serde_json::json!({ "group": group }),
            ),
            Err(e) => return SocketAttempt::Handled(Err(e)),
        },
    };

    // Bounded so a wedged server can't hang a provisioning script forever
    // (the slowest legitimate request is a ~300ms bcrypt hash).
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    let attempt = tokio::time::timeout(TIMEOUT, unix_http_request(sock, method, &path, &body)).await;
    let (status, body) = match attempt {
        Ok(Ok(r)) => r,
        // Connect/IO failure: no live server behind the socket (stale file).
        Ok(Err(_)) => return SocketAttempt::Unavailable,
        // Connected but no answer: a live-but-unresponsive server. Don't fall
        // back to sled — it holds the DB lock, and "unavailable" would lie.
        Err(_) => {
            return SocketAttempt::Handled(Err(format!(
                "the admin socket at {} accepted the connection but did not respond within {}s — the server appears \
                 to be running but unresponsive",
                sock.display(),
                TIMEOUT.as_secs()
            )));
        }
    };

    // 2xx carries {"message": ...}, errors carry {"error": ...}.
    let parsed: Option<serde_json::Value> = serde_json::from_str(&body).ok();
    let field = |name: &str| {
        parsed
            .as_ref()
            .and_then(|v| v.get(name))
            .and_then(|m| m.as_str())
            .map(str::to_string)
    };
    if (200..300).contains(&status) {
        SocketAttempt::Handled(Ok(field("message").unwrap_or_else(|| "OK".to_string())))
    } else {
        SocketAttempt::Handled(Err(
            field("error").unwrap_or_else(|| format!("server returned HTTP {status}"))
        ))
    }
}

/// Minimal one-shot HTTP/1.0 request over a unix socket. HTTP/1.0 keeps the
/// response un-chunked and close-delimited, so "read to EOF" is the whole
/// body — no HTTP client dependency needed for four fixed endpoints.
#[cfg(unix)]
async fn unix_http_request(
    sock: &std::path::Path,
    method: &str,
    path: &str,
    body: &serde_json::Value,
) -> std::io::Result<(u16, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::UnixStream::connect(sock).await?;
    let body = serde_json::to_vec(body)?;
    let head = format!(
        "{method} {path} HTTP/1.0\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: \
         {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&body).await?;

    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    let text = String::from_utf8_lossy(&response);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_ref(), ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::other("malformed HTTP response from admin socket"))?;
    Ok((status, body.to_string()))
}

/// Run an admin [`Command`] against the server's data under `data_dir`.
///
/// If a server is running (its admin socket at `<data_dir>/admin.sock`
/// answers), the command is applied live through it. Otherwise the database is
/// opened directly — sled's exclusive lock makes the two paths mutually safe.
async fn handle_subcommand(command: Command, data_dir: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        let sock = data_dir.join("admin.sock");
        if sock.exists() {
            match try_admin_socket(&command, &sock).await {
                SocketAttempt::Handled(Ok(msg)) => {
                    println!("{msg}");
                    println!("(applied live via admin socket {})", sock.display());
                    return Ok(());
                }
                SocketAttempt::Handled(Err(e)) => anyhow::bail!("{e}"),
                SocketAttempt::Unavailable => {} // stale socket — use the DB directly
            }
        }
    }

    std::fs::create_dir_all(data_dir)?;
    let db_path = format!("{}/rumble.db", data_dir.display());
    let persistence = Persistence::open(&db_path).map_err(|e| {
        // sled's exclusive lock means "a server is already running here" — say
        // that instead of surfacing a cryptic IO error.
        let msg = e.to_string();
        if msg.contains("lock") || msg.contains("WouldBlock") || msg.contains("Resource temporarily unavailable") {
            anyhow::anyhow!(
                "the database at {db_path} is locked — a server appears to be running.\nIts admin socket ({}) wasn't \
                 reachable, so the command could not be applied live.\nEither stop the server and re-run this \
                 command, or use the web admin.",
                data_dir.join("admin.sock").display()
            )
        } else {
            e
        }
    })?;

    match command {
        Command::AddAdmin { public_key } => {
            let key = decode_ed25519_key(&public_key)?;
            // Ensure default groups exist (creates admin group if needed).
            persistence.ensure_default_groups()?;
            persistence.add_user_to_group(&key, "admin")?;
            persistence.flush()?;
            println!("Added public key to admin group.");
            println!("Key: {public_key}");
            println!("Database: {db_path}");
        }
        Command::SetSudoPassword { password } => {
            let hash = bcrypt::hash(&password, bcrypt::DEFAULT_COST)
                .map_err(|e| anyhow::anyhow!("Failed to hash password: {e}"))?;
            persistence.set_sudo_password(&hash)?;
            persistence.flush()?;
            println!("Sudo password set successfully.");
            println!("Database: {db_path}");
        }
        Command::AddController { public_key } => {
            let key = decode_ed25519_key(&public_key)?;
            persistence.ensure_default_groups()?;
            // Ensure a "controllers" group exists granting only MANAGE_PARTICIPANTS —
            // the authority to mint participants, kept separate from the group whose
            // permissions minted participants inherit.
            if persistence.get_group("controllers").is_none() {
                persistence.create_group(
                    "controllers",
                    rumble_protocol::permissions::Permissions::MANAGE_PARTICIPANTS.bits(),
                )?;
            }
            persistence.add_user_to_group(&key, "controllers")?;
            persistence.flush()?;
            println!("Added public key to controllers group (grants MANAGE_PARTICIPANTS).");
            println!("Key: {public_key}");
            println!("Database: {db_path}");
        }
        Command::SetParticipantGroup { public_key, group } => {
            let key = decode_ed25519_key(&public_key)?;
            persistence.ensure_default_groups()?;
            // Match the live endpoint's validation: a typo'd group would
            // otherwise be persisted silently and never match anything.
            if persistence.get_group(&group).is_none() {
                anyhow::bail!("Group '{group}' does not exist");
            }
            persistence.set_participant_default_group(&key, Some(&group))?;
            persistence.flush()?;
            println!("Set default participant group for controller to '{group}'.");
            println!("Anonymous participants minted by this controller will inherit it.");
            println!("Database: {db_path}");
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = CliArgs::parse();
    let command = args.command.clone();

    // Resolve configuration (CLI args + env + config file). Doing this for
    // subcommands too means they operate on the same data dir / database the
    // server would use, rather than a hardcoded "./data".
    let server_config = ServerConfig::load_with_args(args)?;

    // Admin subcommands mutate the server (live via its admin socket when
    // running, else directly in the database) and exit before normal startup.
    if let Some(command) = command {
        return handle_subcommand(command, &server_config.data_dir).await;
    }

    // Initialize logging with configured level
    let env_filter = tracing_subscriber::EnvFilter::try_new(&server_config.log_level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    info!("Rumble server starting...");
    info!("Bind address: {}", server_config.bind);
    info!("Data directory: {}", server_config.data_dir.display());
    info!("Cert directory: {}", server_config.cert_dir.display());
    info!("Domain: {}", server_config.domain);

    // Load certificates
    let (certs, key) = server_config.load_certificates()?;

    // Resolve the database location, failing closed: if a disk DB is requested
    // and its directory can't be created, abort rather than silently run without
    // persistence (which would grant all permissions and skip ban checks).
    let persistence = match server_config.persistence {
        server::config::PersistenceMode::Disk => server::PersistenceMode::Disk(server_config.data_dir()?),
        server::config::PersistenceMode::Ephemeral => server::PersistenceMode::Ephemeral,
    };

    if let Some(ref msg) = server_config.welcome_message {
        info!("Welcome message: {}", msg);
    }

    // Construct plugins via factories, passing config sections from TOML
    let factories: Vec<Box<dyn PluginFactory>> = vec![
        Box::new(FileTransferRelayFactory),
        Box::new(LinkCleanerFactory),
        Box::new(EchoBotFactory),
    ];

    let mut plugins: Vec<Box<dyn server::ServerPlugin>> = Vec::new();
    for factory in &factories {
        let section = server_config.plugins.get(factory.name()).cloned();
        match factory.create(section) {
            Ok(plugin) => {
                info!("loaded plugin: {}", factory.name());
                plugins.push(plugin);
            }
            Err(e) => {
                anyhow::bail!("failed to configure plugin '{}': {}", factory.name(), e);
            }
        }
    }

    // Warn about unknown plugin sections in config
    for key in server_config.plugins.keys() {
        if !factories.iter().any(|f| f.name() == key) {
            warn!("unknown plugin in config: [plugins.{}]", key);
        }
    }

    let config = Config {
        bind: server_config.bind,
        certs,
        key,
        cert_dir: server_config.cert_dir.clone(),
        persistence,
        welcome_message: server_config.welcome_message,
        plugins,
        web: server_config.web,
    };

    let server = Server::new(config)?;
    server.run().await
}
