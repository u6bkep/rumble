//! Server configuration management.
//!
//! This module handles loading configuration from:
//! 1. Default values
//! 2. TOML config file (rumble-server.toml)
//! 3. Command-line arguments (highest priority)
//!
//! Configuration options:
//! - `bind`: Socket address to bind to (IPv4/IPv6 with port)
//! - `log_level`: Logging level (trace, debug, info, warn, error)
//! - `data_dir`: Directory for the database
//! - `cert_dir`: Directory containing TLS certificates (certbot-style PEM files)
//! - `domain`: Server domain name (for certificate generation/ACME)

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    io::BufReader,
    net::SocketAddr,
    path::{Path, PathBuf},
};
use tracing::{info, warn};

/// Default configuration file content with comments.
pub const DEFAULT_CONFIG_CONTENT: &str = r#"# Rumble Server Configuration
# ============================
#
# This file configures the Rumble VOIP server. All options can be
# overridden via command-line arguments.

# Socket address to bind to.
# Supports both IPv4 and IPv6 addresses.
# If no port is specified, 5000 is used.
# Examples:
#   - "[::]:5000"      - All interfaces, IPv6 and IPv4 (default)
#   - "0.0.0.0:5000"   - All IPv4 interfaces only
#   - "127.0.0.1:5000" - Localhost only
#   - "[::1]:5000"     - IPv6 localhost only
bind = "[::]:5000"

# Logging level.
# Options: trace, debug, info, warn, error
log_level = "info"

# Directory for server data (database, etc.).
# Relative paths are resolved from the config file location.
data_dir = "data"

# Database persistence mode.
#   - "disk"      - persist to data_dir/rumble.db; survives restart (default).
#   - "ephemeral" - in-memory only; ALL state (rooms, groups, registrations,
#                   bans, sudo password) is discarded on restart. For throwaway
#                   instances and tests. Can be set via RUMBLE_PERSISTENCE.
persistence = "disk"

# Directory containing TLS certificates.
# The server expects certbot-style PEM files:
#   - fullchain.pem  - Certificate chain
#   - privkey.pem    - Private key
# If the directory doesn't exist or certificates are missing,
# self-signed development certificates will be generated.
cert_dir = "certs"

# Server domain name.
# Used for certificate generation and ACME support.
# For development, "localhost" is used by default.
domain = "localhost"

# Welcome message (MOTD) sent to clients after authentication.
# If not set, no welcome message is sent.
# Can also be set via RUMBLE_WELCOME_MESSAGE environment variable.
# welcome_message = "Welcome to the Rumble server!"

# Web Admin Control-Plane
# =======================
# An HTTP server for administering the running server from a browser
# (groups, ACLs, rooms, bans, live monitoring). Enabled by default on
# loopback only — this is how you complete first-run setup (the server
# logs a one-time bootstrap link on startup until a sudo password is set).
#
# Keep the loopback bind and front it with a reverse proxy / SSH tunnel
# for TLS and remote access. Can be toggled via RUMBLE_WEB_ENABLED=0/1
# and RUMBLE_WEB_BIND.
[web]
enabled = true
bind = "127.0.0.1:5001"
# assets_dir = "admin-web"   # serve the wasm UI bundle from here (dev);
#                            # omit to serve the bundle embedded in the binary.

# Plugin Configuration
# ====================
# Each plugin has its own [plugins.<name>] section.
# Omit a section to use the plugin's default settings.

# [plugins.file-relay]
# ttl = "30m"                    # Cache entry lifetime
# max_file_size = "256 MiB"      # Max single file upload
# max_total_size = "1 GiB"       # Max total cache size
# evict_on_room_clear = true     # Evict when room empties

# Link cleaner: a bot that strips tracking params from posted URLs and replies
# with the cleaned link. Omit rule fields to use sensible built-in defaults.
# [plugins.link-cleaner]
# username = "LinkCleaner"
# label = "bot"
# max_message_length = 5000
# global_strip_params = ["utm_*", "gclid", "fbclid", "igshid"]
#
# [plugins.link-cleaner.domains."youtube.com"]
# allowed_params = ["v", "t"]
#
# [plugins.link-cleaner.domains."twitter.com"]
# remove_all_params = true
"#;

/// Command-line arguments for the server.
#[derive(Parser, Debug)]
#[command(name = "rumble-server")]
#[command(about = "Rumble VOIP Server", long_about = None)]
#[command(
    after_help = "With no subcommand, starts the server. The admin subcommands run a one-shot mutation against the \
                  database (resolved from --config / --data-dir, same as the server) and then exit."
)]
pub struct CliArgs {
    /// Admin database subcommand to run instead of starting the server.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Path to the configuration file.
    /// If the file doesn't exist, it will be created with default values.
    #[arg(short, long, global = true, default_value = "rumble-server.toml")]
    pub config: PathBuf,

    /// Socket address to bind to (overrides config file).
    /// Examples: "[::]:5000", "0.0.0.0:5000", "127.0.0.1:8000"
    #[arg(short, long)]
    pub bind: Option<String>,

    /// Logging level (overrides config file).
    /// Options: trace, debug, info, warn, error
    #[arg(short, long)]
    pub log_level: Option<String>,

    /// Directory for server data (overrides config file).
    /// Also selects which database the admin subcommands operate on.
    #[arg(short, long, global = true)]
    pub data_dir: Option<PathBuf>,

    /// Directory containing TLS certificates (overrides config file).
    #[arg(long)]
    pub cert_dir: Option<PathBuf>,

    /// Server domain name (overrides config file).
    #[arg(long)]
    pub domain: Option<String>,
}

/// One-shot administrative subcommands that mutate the database and exit
/// (instead of starting the server). Public keys are base64-encoded 32-byte
/// Ed25519 keys. The target database is resolved exactly as for normal
/// startup, so these always operate on the same DB the running server uses.
#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Add a public key to the `admin` group (grants all permissions).
    AddAdmin {
        /// Base64-encoded Ed25519 public key.
        public_key: String,
    },
    /// Set the sudo elevation password — also the web-admin login credential.
    /// Stored bcrypt-hashed.
    SetSudoPassword {
        /// The password to set.
        password: String,
    },
    /// Grant a key MANAGE_PARTICIPANTS by adding it to the `controllers` group
    /// (for Mumble-bridge / bot controllers).
    AddController {
        /// Base64-encoded Ed25519 public key.
        public_key: String,
    },
    /// Set the default participant group that a controller's anonymous
    /// participants inherit.
    SetParticipantGroup {
        /// Base64-encoded Ed25519 public key of the controller.
        public_key: String,
        /// Group name to assign as the controller's participant default.
        group: String,
    },
}

/// How the server stores its database.
///
/// A server instance *always* has a database — there is no "no persistence"
/// mode. An ephemeral instance has an in-memory database, not a missing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PersistenceMode {
    /// Persist to `data_dir/rumble.db`; survives restarts. The default.
    #[default]
    Disk,
    /// In-memory only (sled temporary): discarded when the process exits.
    /// Intended for throwaway instances and tests.
    Ephemeral,
}

/// TOML configuration file structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    /// Socket address to bind to.
    #[serde(default = "default_bind")]
    pub bind: String,

    /// Logging level.
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Directory for server data.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,

    /// Database persistence mode: `"disk"` (default) or `"ephemeral"` (in-RAM).
    #[serde(default)]
    pub persistence: PersistenceMode,

    /// Directory containing TLS certificates.
    #[serde(default = "default_cert_dir")]
    pub cert_dir: PathBuf,

    /// Server domain name.
    #[serde(default = "default_domain")]
    pub domain: String,

    /// Welcome message (MOTD) sent to clients after authentication.
    #[serde(default)]
    pub welcome_message: Option<String>,

    /// Plugin configuration sections.
    /// Each key is a plugin name, value is plugin-specific TOML.
    #[serde(default)]
    pub plugins: HashMap<String, toml::Value>,

    /// Web admin control-plane settings (`[web]` section).
    #[serde(default)]
    pub web: WebFileConfig,
}

/// `[web]` configuration section: the HTTP admin control-plane.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebFileConfig {
    /// Enable the web admin server. Enabled by default on a loopback bind:
    /// it is the first-run bootstrap path, and loopback keeps remote exposure
    /// opt-in.
    #[serde(default = "default_web_enabled")]
    pub enabled: bool,

    /// Address to bind the web server to. Defaults to loopback so remote
    /// access requires an explicit bind change (or a reverse proxy / tunnel).
    #[serde(default = "default_web_bind")]
    pub bind: String,

    /// Directory to serve the wasm admin UI bundle from (dev). When unset, the
    /// embedded bundle baked into the binary is served.
    #[serde(default)]
    pub assets_dir: Option<PathBuf>,
}

fn default_web_bind() -> String {
    "127.0.0.1:5001".to_string()
}

fn default_web_enabled() -> bool {
    true
}

/// Parse a boolean-ish environment variable value. Returns `None` for anything
/// not clearly truthy or falsy, so callers can fall back rather than guess.
/// Case-insensitive; trims surrounding whitespace.
fn parse_bool_env(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

impl Default for WebFileConfig {
    fn default() -> Self {
        Self {
            enabled: default_web_enabled(),
            bind: default_web_bind(),
            assets_dir: None,
        }
    }
}

/// Resolved web admin settings (present only when enabled).
#[derive(Debug, Clone)]
pub struct WebSettings {
    /// Socket address the web admin server binds to.
    pub bind: SocketAddr,
    /// Optional directory to serve the admin UI from (dev override).
    pub assets_dir: Option<PathBuf>,
    /// Server data directory, used to persist the one-time bootstrap token to a
    /// `0600` file instead of logging the secret.
    pub data_dir: PathBuf,
}

fn default_bind() -> String {
    "[::]:5000".to_string()
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("data")
}

fn default_cert_dir() -> PathBuf {
    PathBuf::from("certs")
}

fn default_domain() -> String {
    "localhost".to_string()
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            log_level: default_log_level(),
            data_dir: default_data_dir(),
            persistence: PersistenceMode::default(),
            cert_dir: default_cert_dir(),
            domain: default_domain(),
            welcome_message: None,
            plugins: HashMap::new(),
            web: WebFileConfig::default(),
        }
    }
}

/// Resolved server configuration.
///
/// This is the final configuration after merging defaults, config file, and CLI args.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Socket address to bind to.
    pub bind: SocketAddr,
    /// Logging level.
    pub log_level: String,
    /// Directory for server data.
    pub data_dir: PathBuf,
    /// Database persistence mode (disk vs ephemeral in-RAM).
    pub persistence: PersistenceMode,
    /// Directory containing TLS certificates.
    pub cert_dir: PathBuf,
    /// Server domain name.
    pub domain: String,
    /// Base directory for resolving relative paths (config file directory).
    pub base_dir: PathBuf,
    /// Welcome message (MOTD) sent to clients after authentication.
    pub welcome_message: Option<String>,
    /// Plugin configuration sections (raw TOML, passed to plugin factories).
    pub plugins: HashMap<String, toml::Value>,
    /// Resolved web admin settings, present only when `[web].enabled`.
    pub web: Option<WebSettings>,
}

impl ServerConfig {
    /// Load configuration from CLI args and config file.
    ///
    /// Priority (highest to lowest):
    /// 1. Command-line arguments
    /// 2. Config file
    /// 3. Default values
    pub fn load() -> Result<Self> {
        let args = CliArgs::parse();
        Self::load_with_args(args)
    }

    /// Load configuration with the given CLI args.
    ///
    /// Priority (highest to lowest):
    /// 1. Command-line arguments
    /// 2. Environment variables (RUMBLE_*)
    /// 3. Config file
    /// 4. Default values
    ///
    /// This is useful for testing.
    pub fn load_with_args(args: CliArgs) -> Result<Self> {
        let config_path = &args.config;
        let base_dir = config_path
            .parent()
            .map(|p| if p.as_os_str().is_empty() { Path::new(".") } else { p })
            .unwrap_or(Path::new("."))
            .to_path_buf();

        // NOTE: config is loaded *before* the tracing subscriber is initialized
        // (the subscriber's level comes from this config), so `info!`/`warn!`
        // here would be dropped. Use `eprintln!` so the path is always visible —
        // this is the quickest way to spot "edited the wrong copy" / wrong-cwd.
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();

        // Check for RUMBLE_NO_CONFIG env var to skip config file entirely (for testing)
        let file_config = if std::env::var("RUMBLE_NO_CONFIG").is_ok() {
            eprintln!("[config] RUMBLE_NO_CONFIG set — skipping config file, using built-in defaults");
            FileConfig::default()
        } else if config_path.exists() {
            let shown = fs::canonicalize(config_path).unwrap_or_else(|_| config_path.clone());
            eprintln!("[config] loading {}", shown.display());
            let content = fs::read_to_string(config_path)
                .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;
            toml::from_str(&content)
                .with_context(|| format!("Failed to parse config file: {}", config_path.display()))?
        } else if args.command.is_some() {
            // One-shot admin subcommands must not seed a server config (or a
            // data/cert tree) as a side effect — run on defaults quietly.
            eprintln!(
                "[config] no config at {} — using defaults (admin subcommand; not creating one)",
                config_path.display()
            );
            FileConfig::default()
        } else {
            // Creating a config (and with it a data/cert tree) in an unintended
            // directory silently forks the deployment — make this unmissable.
            let abs = std::env::current_dir()
                .map(|p| p.join(config_path))
                .unwrap_or_else(|_| config_path.clone());
            eprintln!(
                "[config] ============================================================\n[config] no config file found \
                 — creating one with defaults at\n[config]   {}\n[config] (cwd: {cwd})\n[config] data and \
                 certificates will live next to it unless configured\n[config] otherwise. If you meant to use an \
                 existing setup, stop the\n[config] server, delete this file, and pass --config <path>.\n[config] \
                 ============================================================",
                abs.display(),
            );
            fs::write(config_path, DEFAULT_CONFIG_CONTENT).with_context(|| {
                format!(
                    "Failed to create config file at {} — is the directory writable? Pass --config to choose another \
                     location",
                    abs.display()
                )
            })?;
            FileConfig::default()
        };

        // Merge: CLI args > env vars > file config > defaults
        // Check environment variables for overrides
        let env_bind = std::env::var("RUMBLE_BIND")
            .ok()
            .or_else(|| std::env::var("RUMBLE_PORT").ok().map(|p| format!("[::]:{}", p)));
        let env_log_level = std::env::var("RUMBLE_LOG_LEVEL").ok();
        let env_data_dir = std::env::var("RUMBLE_DATA_DIR").ok().map(PathBuf::from);
        let env_cert_dir = std::env::var("RUMBLE_CERT_DIR").ok().map(PathBuf::from);
        let env_domain = std::env::var("RUMBLE_DOMAIN").ok();
        let env_welcome_message = std::env::var("RUMBLE_WELCOME_MESSAGE").ok();

        // RUMBLE_PERSISTENCE=disk|ephemeral (also accepts memory/in-memory).
        let env_persistence =
            std::env::var("RUMBLE_PERSISTENCE")
                .ok()
                .and_then(|v| match v.to_ascii_lowercase().as_str() {
                    "disk" => Some(PersistenceMode::Disk),
                    "ephemeral" | "memory" | "in-memory" => Some(PersistenceMode::Ephemeral),
                    other => {
                        eprintln!("[config] ignoring unknown RUMBLE_PERSISTENCE='{other}' (expected disk|ephemeral)");
                        None
                    }
                });

        let bind_str = args.bind.or(env_bind).unwrap_or(file_config.bind);
        let log_level = args.log_level.or(env_log_level).unwrap_or(file_config.log_level);
        let data_dir = args.data_dir.or(env_data_dir).unwrap_or(file_config.data_dir);
        let persistence = env_persistence.unwrap_or(file_config.persistence);
        let cert_dir = args.cert_dir.or(env_cert_dir).unwrap_or(file_config.cert_dir);
        let domain = args.domain.or(env_domain).unwrap_or(file_config.domain);
        let welcome_message = env_welcome_message
            .or(file_config.welcome_message)
            .filter(|s| !s.trim().is_empty());

        // Parse bind address
        let bind = parse_bind_address(&bind_str)?;

        // Resolve relative paths
        let data_dir = resolve_path(&base_dir, &data_dir);
        let cert_dir = resolve_path(&base_dir, &cert_dir);

        // Resolve web admin settings (env override for enable/bind to ease ops).
        // The override applies only when RUMBLE_WEB_ENABLED parses to a clear
        // truthy/falsy value; an unrecognized value (e.g. "TRUE", "on", a typo)
        // must NOT silently flip the file's setting either way.
        let web_enabled = match std::env::var("RUMBLE_WEB_ENABLED") {
            Ok(v) => match parse_bool_env(&v) {
                Some(b) => b,
                None => {
                    eprintln!(
                        "warning: RUMBLE_WEB_ENABLED='{v}' not recognized (use 1/true/yes/on or 0/false/no/off); \
                         falling back to the config file's web.enabled"
                    );
                    file_config.web.enabled
                }
            },
            Err(_) => file_config.web.enabled,
        };
        let web = if web_enabled {
            let web_bind_str = std::env::var("RUMBLE_WEB_BIND").unwrap_or(file_config.web.bind);
            let web_bind = parse_bind_address(&web_bind_str)
                .with_context(|| format!("Invalid web bind address: {}", web_bind_str))?;
            let assets_dir = file_config.web.assets_dir.map(|d| resolve_path(&base_dir, &d));
            Some(WebSettings {
                bind: web_bind,
                assets_dir,
                data_dir: data_dir.clone(),
            })
        } else {
            None
        };

        Ok(Self {
            bind,
            log_level,
            data_dir,
            persistence,
            cert_dir,
            domain,
            base_dir,
            welcome_message,
            plugins: file_config.plugins,
            web,
        })
    }

    /// Get the data directory path, creating it if necessary.
    pub fn data_dir(&self) -> Result<PathBuf> {
        fs::create_dir_all(&self.data_dir)?;
        Ok(self.data_dir.clone())
    }

    /// Get the cert directory path, creating it if necessary.
    pub fn cert_dir(&self) -> Result<PathBuf> {
        fs::create_dir_all(&self.cert_dir)?;
        Ok(self.cert_dir.clone())
    }

    /// Load TLS certificate and key.
    ///
    /// Looks for certbot-style PEM files in the cert directory:
    /// - fullchain.pem (certificate chain)
    /// - privkey.pem (private key)
    ///
    /// If not found, generates self-signed development certificates.
    pub fn load_certificates(&self) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
        let cert_dir = self.cert_dir()?;
        let fullchain_path = cert_dir.join("fullchain.pem");
        let privkey_path = cert_dir.join("privkey.pem");

        if fullchain_path.exists() && privkey_path.exists() {
            info!("Loading certificates from {}", cert_dir.display());
            load_pem_certificates(&fullchain_path, &privkey_path)
        } else {
            warn!(
                "Certificates not found in {}, generating self-signed dev cert",
                cert_dir.display()
            );
            generate_self_signed_cert(&cert_dir, &self.domain)
        }
    }
}

/// Parse a bind address string into a SocketAddr.
///
/// If no port is specified, 5000 is used.
fn parse_bind_address(s: &str) -> Result<SocketAddr> {
    // Try parsing as-is first
    if let Ok(addr) = s.parse() {
        return Ok(addr);
    }

    // Try adding default port
    let with_port = if s.contains('[') && !s.contains("]:") {
        // IPv6 without port: [::] -> [::]:5000
        format!("{}:5000", s)
    } else if !s.contains(':') {
        // IPv4 without port: 0.0.0.0 -> 0.0.0.0:5000
        format!("{}:5000", s)
    } else {
        s.to_string()
    };

    with_port
        .parse()
        .with_context(|| format!("Invalid bind address: {}", s))
}

/// Resolve a path relative to a base directory.
fn resolve_path(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Load certificates from PEM files (certbot-style).
pub fn load_pem_certificates(
    fullchain_path: &Path,
    privkey_path: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    // Load certificate chain
    let cert_file = fs::File::open(fullchain_path)
        .with_context(|| format!("Failed to open certificate file: {}", fullchain_path.display()))?;
    let mut cert_reader = BufReader::new(cert_file);
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("Failed to parse certificate file: {}", fullchain_path.display()))?;

    if certs.is_empty() {
        anyhow::bail!("No certificates found in {}", fullchain_path.display());
    }
    info!(
        "Loaded {} certificate(s) from {}",
        certs.len(),
        fullchain_path.display()
    );

    // Load private key
    let key_file =
        fs::File::open(privkey_path).with_context(|| format!("Failed to open key file: {}", privkey_path.display()))?;
    let mut key_reader = BufReader::new(key_file);

    let key = rustls_pemfile::private_key(&mut key_reader)
        .with_context(|| format!("Failed to parse key file: {}", privkey_path.display()))?
        .ok_or_else(|| anyhow::anyhow!("No private key found in {}", privkey_path.display()))?;

    info!("Loaded private key from {}", privkey_path.display());

    Ok((certs, key))
}

/// Generate a self-signed certificate for development.
pub fn generate_self_signed_cert(
    cert_dir: &Path,
    domain: &str,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    fs::create_dir_all(cert_dir)?;

    let fullchain_path = cert_dir.join("fullchain.pem");
    let privkey_path = cert_dir.join("privkey.pem");

    // Generate self-signed certificate
    let subject_alt_names = vec![domain.to_string()];
    let ck =
        rcgen::generate_simple_self_signed(subject_alt_names).context("Failed to generate self-signed certificate")?;

    // Write PEM files (certbot-style)
    let cert_pem = ck.cert.pem();
    let key_pem = ck.signing_key.serialize_pem();

    fs::write(&fullchain_path, &cert_pem).with_context(|| {
        format!(
            "Failed to write certificate to {} — the cert directory must be writable by the server's user",
            fullchain_path.display()
        )
    })?;
    write_private_key(&privkey_path, &key_pem).with_context(|| {
        format!(
            "Failed to write private key to {} — the cert directory must be writable by the server's user",
            privkey_path.display()
        )
    })?;

    info!(
        "Generated self-signed certificate for '{}' in {}",
        domain,
        cert_dir.display()
    );

    // Parse the generated PEM files
    load_pem_certificates(&fullchain_path, &privkey_path)
}

/// Write a PEM private key with owner-only (0600) permissions on Unix, so the
/// generated self-signed key isn't left world-readable under the default umask.
fn write_private_key(path: &std::path::Path, key_pem: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        // Re-assert mode in case the file pre-existed with looser perms.
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        file.write_all(key_pem.as_bytes())
    }
    #[cfg(not(unix))]
    {
        fs::write(path, key_pem)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bind_address() {
        // Full addresses
        assert_eq!(
            parse_bind_address("[::]:5000").unwrap(),
            "[::]:5000".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            parse_bind_address("0.0.0.0:8000").unwrap(),
            "0.0.0.0:8000".parse::<SocketAddr>().unwrap()
        );

        // Without port
        assert_eq!(
            parse_bind_address("[::]").unwrap(),
            "[::]:5000".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            parse_bind_address("0.0.0.0").unwrap(),
            "0.0.0.0:5000".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn parse_bool_env_recognizes_truthy_and_falsy() {
        for s in ["1", "true", "TRUE", "Yes", "on", " on "] {
            assert_eq!(parse_bool_env(s), Some(true), "{s:?} should be truthy");
        }
        for s in ["0", "false", "FALSE", "no", "off"] {
            assert_eq!(parse_bool_env(s), Some(false), "{s:?} should be falsy");
        }
        // Unrecognized values yield None so the caller falls back to the file.
        for s in ["", "maybe", "enable", "2"] {
            assert_eq!(parse_bool_env(s), None, "{s:?} should be unrecognized");
        }
    }

    #[test]
    fn unknown_config_key_is_rejected() {
        // A misspelled top-level key (e.g. `enable` instead of `[web] enabled`)
        // must error rather than silently keep the default.
        let toml_src = "bind = \"0.0.0.0:5000\"\nenable = true\n";
        let parsed: Result<FileConfig, _> = toml::from_str(toml_src);
        assert!(parsed.is_err(), "unknown key must be rejected, got {parsed:?}");
    }

    #[test]
    fn unknown_web_key_is_rejected() {
        let toml_src = "[web]\nenable = true\n";
        let parsed: Result<FileConfig, _> = toml::from_str(toml_src);
        assert!(parsed.is_err(), "unknown [web] key must be rejected, got {parsed:?}");
    }

    #[test]
    fn test_default_config() {
        let config = FileConfig::default();
        assert_eq!(config.bind, "[::]:5000");
        assert_eq!(config.log_level, "info");
        assert_eq!(config.domain, "localhost");
    }
}
