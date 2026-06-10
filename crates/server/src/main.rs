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

/// Run an admin [`Command`] against the database under `data_dir`, then return.
/// `data_dir` is the fully-resolved server data directory, so subcommands act
/// on the same database the running server uses.
fn handle_subcommand(command: Command, data_dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let db_path = format!("{}/rumble.db", data_dir.display());
    let persistence = Persistence::open(&db_path)?;

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

    // Admin subcommands mutate the database and exit before normal startup.
    if let Some(command) = command {
        return handle_subcommand(command, &server_config.data_dir);
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
