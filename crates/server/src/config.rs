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
use clap::Parser;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde::{Deserialize, Serialize};
use std::{
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
"#;

/// Command-line arguments for the server.
#[derive(Parser, Debug)]
#[command(name = "rumble-server")]
#[command(about = "Rumble VOIP Server", long_about = None)]
pub struct CliArgs {
    /// Path to the configuration file.
    /// If the file doesn't exist, it will be created with default values.
    #[arg(short, long, default_value = "rumble-server.toml")]
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
    #[arg(short, long)]
    pub data_dir: Option<PathBuf>,

    /// Directory containing TLS certificates (overrides config file).
    #[arg(long)]
    pub cert_dir: Option<PathBuf>,

    /// Server domain name (overrides config file).
    #[arg(long)]
    pub domain: Option<String>,
}

/// TOML configuration file structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

    /// Directory containing TLS certificates.
    #[serde(default = "default_cert_dir")]
    pub cert_dir: PathBuf,

    /// Server domain name.
    #[serde(default = "default_domain")]
    pub domain: String,
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
            cert_dir: default_cert_dir(),
            domain: default_domain(),
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
    /// Directory containing TLS certificates.
    pub cert_dir: PathBuf,
    /// Server domain name.
    pub domain: String,
    /// Base directory for resolving relative paths (config file directory).
    pub base_dir: PathBuf,
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

        // Check for RUMBLE_NO_CONFIG env var to skip config file entirely (for testing)
        let file_config = if std::env::var("RUMBLE_NO_CONFIG").is_ok() {
            FileConfig::default()
        } else if config_path.exists() {
            let content = fs::read_to_string(config_path)
                .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;
            toml::from_str(&content)
                .with_context(|| format!("Failed to parse config file: {}", config_path.display()))?
        } else {
            info!("Config file not found, creating default: {}", config_path.display());
            fs::write(config_path, DEFAULT_CONFIG_CONTENT)
                .with_context(|| format!("Failed to create config file: {}", config_path.display()))?;
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

        let bind_str = args.bind.or(env_bind).unwrap_or(file_config.bind);
        let log_level = args.log_level.or(env_log_level).unwrap_or(file_config.log_level);
        let data_dir = args.data_dir.or(env_data_dir).unwrap_or(file_config.data_dir);
        let cert_dir = args.cert_dir.or(env_cert_dir).unwrap_or(file_config.cert_dir);
        let domain = args.domain.or(env_domain).unwrap_or(file_config.domain);

        // Parse bind address
        let bind = parse_bind_address(&bind_str)?;

        // Resolve relative paths
        let data_dir = resolve_path(&base_dir, &data_dir);
        let cert_dir = resolve_path(&base_dir, &cert_dir);

        Ok(Self {
            bind,
            log_level,
            data_dir,
            cert_dir,
            domain,
            base_dir,
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

    fs::write(&fullchain_path, &cert_pem)
        .with_context(|| format!("Failed to write certificate: {}", fullchain_path.display()))?;
    fs::write(&privkey_path, &key_pem)
        .with_context(|| format!("Failed to write private key: {}", privkey_path.display()))?;

    info!(
        "Generated self-signed certificate for '{}' in {}",
        domain,
        cert_dir.display()
    );

    // Parse the generated PEM files
    load_pem_certificates(&fullchain_path, &privkey_path)
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
    fn test_default_config() {
        let config = FileConfig::default();
        assert_eq!(config.bind, "[::]:5000");
        assert_eq!(config.log_level, "info");
        assert_eq!(config.domain, "localhost");
    }
}
