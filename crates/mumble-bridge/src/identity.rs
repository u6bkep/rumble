//! Persistent Ed25519 identity for the bridge.
//!
//! A production Rumble server authorizes a controller by its public key
//! (`server add-controller <b64>`), so the bridge must present a *stable*
//! identity across restarts rather than minting a fresh random key each boot
//! (which could never be durably authorized).
//!
//! The identity file stores the 32-byte Ed25519 seed as hex, written owner-only
//! (`0600`) on Unix since it is a private key.

use std::path::Path;

use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;

/// Load the bridge signing key from `path`, generating and persisting a new one
/// if the file does not exist.
pub fn load_or_create_identity(path: &Path) -> Result<SigningKey> {
    if path.exists() {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading bridge identity {}", path.display()))?;
        let seed = decode_seed(text.trim()).with_context(|| format!("parsing bridge identity {}", path.display()))?;
        Ok(SigningKey::from_bytes(&seed))
    } else {
        let signing_key = SigningKey::from_bytes(&rand::random());
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).with_context(|| format!("creating identity dir {}", parent.display()))?;
        }
        write_seed(path, &signing_key.to_bytes())
            .with_context(|| format!("writing bridge identity {}", path.display()))?;
        Ok(signing_key)
    }
}

/// STANDARD-base64 the public key for `server add-controller`.
pub fn public_key_b64(signing_key: &SigningKey) -> String {
    use base64::{Engine, engine::general_purpose::STANDARD};
    STANDARD.encode(signing_key.verifying_key().to_bytes())
}

fn decode_seed(hex_str: &str) -> Result<[u8; 32]> {
    if hex_str.len() != 64 {
        anyhow::bail!("identity seed must be 64 hex chars (32 bytes), got {}", hex_str.len());
    }
    let bytes = (0..hex_str.len())
        .step_by(2)
        .map(|i| {
            hex_str
                .get(i..i + 2)
                .and_then(|b| u8::from_str_radix(b, 16).ok())
                .ok_or_else(|| anyhow::anyhow!("invalid hex in identity seed"))
        })
        .collect::<Result<Vec<u8>>>()?;
    Ok(bytes.try_into().expect("validated 32-byte length above"))
}

fn write_seed(path: &Path, seed: &[u8; 32]) -> std::io::Result<()> {
    let hex: String = seed.iter().map(|b| format!("{b:02x}")).collect();
    #[cfg(unix)]
    {
        use std::{
            io::Write,
            os::unix::fs::{OpenOptionsExt, PermissionsExt},
        };
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        // `.mode()` only applies on creation; fix perms if the file pre-existed.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        writeln!(file, "{hex}")
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, format!("{hex}\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_seed() {
        let dir = std::env::temp_dir().join(format!("rumble-bridge-id-test-{}", std::process::id()));
        let path = dir.join("identity.key");
        let _ = std::fs::remove_dir_all(&dir);

        let first = load_or_create_identity(&path).expect("create");
        let second = load_or_create_identity(&path).expect("load");
        assert_eq!(
            first.to_bytes(),
            second.to_bytes(),
            "reloaded identity must match the persisted one"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "identity file must be 0600");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
