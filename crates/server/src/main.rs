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
use server::{
    Config, EchoBotFactory, FileTransferRelayFactory, LinkCleanerFactory, Persistence, Server, ServerConfig,
    plugin::PluginFactory,
};
use tracing::{info, warn};

/// Handle admin CLI subcommands that run against the database and then exit.
/// Returns `Some(())` if a subcommand was handled, `None` if normal startup should proceed.
fn handle_subcommand() -> Result<Option<()>> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() >= 3 && args[1] == "add-admin" {
        let key_b64 = &args[2];
        let data_dir = args.get(3).cloned().unwrap_or_else(|| "data".to_string());

        // Decode the base64 public key
        let key_bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, key_b64)
            .map_err(|e| anyhow::anyhow!("Invalid base64 public key: {e}"))?;

        if key_bytes.len() != 32 {
            anyhow::bail!(
                "Public key must be 32 bytes (got {}). Provide a base64-encoded Ed25519 public key.",
                key_bytes.len()
            );
        }

        let key: [u8; 32] = key_bytes.try_into().unwrap();

        // Open database
        std::fs::create_dir_all(&data_dir)?;
        let db_path = format!("{}/rumble.db", data_dir);
        let persistence = Persistence::open(&db_path)?;

        // Ensure default groups exist (creates admin group if needed)
        persistence.ensure_default_groups()?;

        // Add user to admin group
        persistence.add_user_to_group(&key, "admin")?;
        persistence.flush()?;

        println!("Added public key to admin group.");
        println!("Key: {key_b64}");
        println!("Database: {db_path}");

        return Ok(Some(()));
    }

    if args.len() >= 3 && args[1] == "set-sudo-password" {
        let password = &args[2];
        let data_dir = args.get(3).cloned().unwrap_or_else(|| "data".to_string());

        // Hash the password with bcrypt
        let hash = bcrypt::hash(password, bcrypt::DEFAULT_COST)
            .map_err(|e| anyhow::anyhow!("Failed to hash password: {e}"))?;

        // Open database
        std::fs::create_dir_all(&data_dir)?;
        let db_path = format!("{}/rumble.db", data_dir);
        let persistence = Persistence::open(&db_path)?;

        persistence.set_sudo_password(&hash)?;
        persistence.flush()?;

        println!("Sudo password set successfully.");
        println!("Database: {db_path}");

        return Ok(Some(()));
    }

    if args.len() >= 3 && args[1] == "add-controller" {
        let key_b64 = &args[2];
        let data_dir = args.get(3).cloned().unwrap_or_else(|| "data".to_string());

        let key_bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, key_b64)
            .map_err(|e| anyhow::anyhow!("Invalid base64 public key: {e}"))?;
        if key_bytes.len() != 32 {
            anyhow::bail!(
                "Public key must be 32 bytes (got {}). Provide a base64-encoded Ed25519 public key.",
                key_bytes.len()
            );
        }
        let key: [u8; 32] = key_bytes.try_into().unwrap();

        std::fs::create_dir_all(&data_dir)?;
        let db_path = format!("{}/rumble.db", data_dir);
        let persistence = Persistence::open(&db_path)?;
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
        println!("Key: {key_b64}");
        println!("Database: {db_path}");

        return Ok(Some(()));
    }

    if args.len() >= 4 && args[1] == "set-participant-group" {
        let key_b64 = &args[2];
        let group = &args[3];
        let data_dir = args.get(4).cloned().unwrap_or_else(|| "data".to_string());

        let key_bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, key_b64)
            .map_err(|e| anyhow::anyhow!("Invalid base64 public key: {e}"))?;
        if key_bytes.len() != 32 {
            anyhow::bail!(
                "Public key must be 32 bytes (got {}). Provide a base64-encoded Ed25519 public key.",
                key_bytes.len()
            );
        }
        let key: [u8; 32] = key_bytes.try_into().unwrap();

        std::fs::create_dir_all(&data_dir)?;
        let db_path = format!("{}/rumble.db", data_dir);
        let persistence = Persistence::open(&db_path)?;
        persistence.set_participant_default_group(&key, Some(group))?;
        persistence.flush()?;

        println!("Set default participant group for controller to '{group}'.");
        println!("Anonymous participants minted by this controller will inherit it.");
        println!("Database: {db_path}");

        return Ok(Some(()));
    }

    Ok(None)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Check for subcommands before normal startup
    if handle_subcommand()?.is_some() {
        return Ok(());
    }

    // Load configuration (CLI args + config file)
    let server_config = ServerConfig::load()?;

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

    // Build server config
    let data_dir = server_config.data_dir().ok().map(|p| p.to_string_lossy().to_string());

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
        data_dir,
        welcome_message: server_config.welcome_message,
        plugins,
        web: server_config.web,
    };

    let server = Server::new(config)?;
    server.run().await
}
