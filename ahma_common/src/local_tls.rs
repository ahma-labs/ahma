//! Persistent local TLS certificate management for Ahma.
//!
//! When `ahma serve http` starts QUIC, it uses a self-signed certificate.  By
//! default this certificate is ephemeral (regenerated on every restart), which
//! means QUIC clients (including `ahma tui`) need to re-trust the cert on every
//! server restart.
//!
//! This module provides **persistent** local TLS material: a self-signed cert
//! plus private key saved under `~/.ahma/tls/`.  The QUIC server loads the
//!   persisted cert on startup (generating it on first use) and emits a warning
//!   when the cert is approaching its 30-day rotation window.
//!
//! The directory can be overridden with the `AHMA_TLS_DIR` environment variable.
//!
//! ## Directory layout
//!
//! ```text
//! ~/.ahma/tls/
//!   cert.der   — DER-encoded self-signed certificate (public; safe to distribute)
//!   key.der    — DER-encoded private key (chmod 0600 on Unix)
//! ```

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tracing::{info, warn};

/// Rotation warning window: warn when the cert is older than this many days.
const ROTATION_WARNING_DAYS: u64 = 30;

/// Process-wide TLS directory override set from the `--tls-dir` CLI flag.
static TLS_DIR_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

// ─────────────────────────────────────────────────────────────────────────────
// Configuration
// ─────────────────────────────────────────────────────────────────────────────

/// Directory for local TLS material.
#[derive(Debug, Clone)]
pub struct LocalTlsConfig {
    /// Directory under which `cert.der` and `key.der` are stored.
    pub dir: PathBuf,
}

/// A loaded / generated TLS certificate + key pair.
#[derive(Clone)]
pub struct LocalTlsCerts {
    /// DER-encoded certificate (public; distribute to clients for trust).
    pub cert_der: Vec<u8>,
    /// DER-encoded private key (keep secret; chmod 0600 on Unix).
    pub key_der: Vec<u8>,
}

impl LocalTlsConfig {
    /// Default TLS directory: `~/.ahma/tls`.
    pub fn default_dir() -> PathBuf {
        crate::config::ahma_home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".ahma")
            .join("tls")
    }

    /// Set the TLS directory from the `--tls-dir` CLI flag.
    /// Call once, early in startup. Takes precedence over `AHMA_TLS_DIR`.
    pub fn set_dir_override(dir: PathBuf) {
        let _ = TLS_DIR_OVERRIDE.set(dir);
    }

    /// Construct a `LocalTlsConfig`: `--tls-dir` flag override first, then
    /// `~/.ahma/tls`.
    ///
    /// `AHMA_TLS_DIR` is security-tier and **retired** (R-CFG2.3): the TLS
    /// material directory must not be redirectable via ambient environment
    /// state. If the variable is set it is warned-about and ignored; use the
    /// `--tls-dir` flag instead.
    pub fn from_env() -> Self {
        // R-CFG1.2.1: one function states the verdict. The extra sentence about
        // *why* this one is security-tier stays here, where the risk lives.
        if crate::config::warn_retired_env("AHMA_TLS_DIR") {
            warn!(
                "AHMA_TLS_DIR is security-tier (R-CFG2.3): redirecting TLS material via ambient \
                 environment state is a tamper risk. Use the --tls-dir flag."
            );
        }
        if let Some(dir) = TLS_DIR_OVERRIDE.get() {
            return Self { dir: dir.clone() };
        }
        Self {
            dir: Self::default_dir(),
        }
    }

    /// Path to the DER-encoded certificate file.
    pub fn cert_path(&self) -> PathBuf {
        self.dir.join("cert.der")
    }

    /// Path to the DER-encoded private key file.
    pub fn key_path(&self) -> PathBuf {
        self.dir.join("key.der")
    }

    /// Returns `true` when both cert and key files exist on disk.
    pub fn exists(&self) -> bool {
        self.cert_path().exists() && self.key_path().exists()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Lifecycle
// ─────────────────────────────────────────────────────────────────────────────

/// Load the persistent TLS certificate and key from disk, generating new material
/// if either file is missing.
///
/// Also logs a warning when the certificate is approaching its rotation window
/// (age > [`ROTATION_WARNING_DAYS`]).
pub fn provision_if_needed(config: &LocalTlsConfig) -> Result<LocalTlsCerts> {
    if config.exists() {
        let certs = load_certs(config)?;
        if check_rotation_needed(config) {
            warn!(
                "Ahma local TLS certificate is approaching expiry ({}+ days old). \
                 Run `ahma tls rotate` to renew it.",
                ROTATION_WARNING_DAYS
            );
        }
        Ok(certs)
    } else {
        info!(
            "No local TLS certificate found at {}; generating new material.",
            config.dir.display()
        );
        generate_and_save(config)
    }
}

/// Load existing TLS material from disk without generating new material if absent.
///
/// Returns an error when either file is missing or unreadable.
pub fn load_certs(config: &LocalTlsConfig) -> Result<LocalTlsCerts> {
    let cert_der = std::fs::read(config.cert_path())
        .with_context(|| format!("Failed to read {}", config.cert_path().display()))?;
    let key_der = std::fs::read(config.key_path())
        .with_context(|| format!("Failed to read {}", config.key_path().display()))?;
    Ok(LocalTlsCerts { cert_der, key_der })
}

/// Returns `true` when the certificate file is older than [`ROTATION_WARNING_DAYS`] days.
///
/// Returns `false` if the cert does not exist (no rotation needed for something that
/// hasn't been generated yet).
pub fn check_rotation_needed(config: &LocalTlsConfig) -> bool {
    let Ok(meta) = std::fs::metadata(config.cert_path()) else {
        return false;
    };
    let Ok(created) = meta.created().or_else(|_| meta.modified()) else {
        return false;
    };
    let age = SystemTime::now()
        .duration_since(created)
        .unwrap_or(Duration::ZERO);
    age > Duration::from_secs(60 * 60 * 24 * ROTATION_WARNING_DAYS)
}

/// Generate a new self-signed certificate + key and persist them to `config.dir`.
///
/// Creates the directory (including parents) if it does not exist.
/// On Unix the private key file is written with mode 0600.
pub fn generate_and_save(config: &LocalTlsConfig) -> Result<LocalTlsCerts> {
    std::fs::create_dir_all(&config.dir)
        .with_context(|| format!("Failed to create TLS directory {}", config.dir.display()))?;

    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string(), "localhost".to_string()])
            .context("Failed to generate self-signed certificate")?;

    let cert_der = cert.der().to_vec();
    let key_der = signing_key.serialize_der();

    std::fs::write(config.cert_path(), &cert_der)
        .with_context(|| format!("Failed to write {}", config.cert_path().display()))?;
    write_private_key(config.key_path(), &key_der)?;

    info!(
        "Generated new local TLS certificate at {}",
        config.cert_path().display()
    );
    Ok(LocalTlsCerts { cert_der, key_der })
}

/// Delete and regenerate the local TLS certificate and private key.
pub fn rotate(config: &LocalTlsConfig) -> Result<LocalTlsCerts> {
    if config.cert_path().exists() {
        std::fs::remove_file(config.cert_path())
            .with_context(|| format!("Failed to remove {}", config.cert_path().display()))?;
    }
    if config.key_path().exists() {
        std::fs::remove_file(config.key_path())
            .with_context(|| format!("Failed to remove {}", config.key_path().display()))?;
    }
    generate_and_save(config)
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Write the private key with restrictive permissions (0600 on Unix).
fn write_private_key(path: impl AsRef<Path>, key_der: &[u8]) -> Result<()> {
    let path = path.as_ref();
    std::fs::write(path, key_der)
        .with_context(|| format!("Failed to write private key to {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("Failed to set permissions 0600 on {}", path.display()))?;
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn cfg(dir: &TempDir) -> LocalTlsConfig {
        LocalTlsConfig {
            dir: dir.path().to_owned(),
        }
    }

    #[test]
    fn default_dir_contains_ahma_tls() {
        let dir = LocalTlsConfig::default_dir();
        let s = dir.to_string_lossy();
        assert!(
            s.contains(".ahma") && s.contains("tls"),
            "expected ~/.ahma/tls in path, got {s}"
        );
    }

    #[test]
    fn exists_false_when_empty() {
        let tmp = TempDir::new().unwrap();
        assert!(!cfg(&tmp).exists());
    }

    #[test]
    fn generate_and_save_creates_files() {
        let tmp = TempDir::new().unwrap();
        let config = cfg(&tmp);
        let certs = generate_and_save(&config).unwrap();
        assert!(!certs.cert_der.is_empty());
        assert!(!certs.key_der.is_empty());
        assert!(config.cert_path().exists());
        assert!(config.key_path().exists());
        assert!(config.exists());
    }

    #[cfg(unix)]
    #[test]
    fn private_key_permissions_are_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let config = cfg(&tmp);
        generate_and_save(&config).unwrap();
        let meta = std::fs::metadata(config.key_path()).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "key.der permissions should be 0600, got {mode:o}"
        );
    }

    #[test]
    fn load_certs_round_trips() {
        let tmp = TempDir::new().unwrap();
        let config = cfg(&tmp);
        let certs = generate_and_save(&config).unwrap();
        let loaded = load_certs(&config).unwrap();
        assert_eq!(certs.cert_der, loaded.cert_der);
        assert_eq!(certs.key_der, loaded.key_der);
    }

    #[test]
    fn provision_if_needed_idempotent() {
        let tmp = TempDir::new().unwrap();
        let config = cfg(&tmp);
        let first = provision_if_needed(&config).unwrap();
        let second = provision_if_needed(&config).unwrap();
        // On second call the same cert should be returned (not regenerated)
        assert_eq!(first.cert_der, second.cert_der);
    }

    #[test]
    fn rotate_replaces_cert() {
        let tmp = TempDir::new().unwrap();
        let config = cfg(&tmp);
        let before = generate_and_save(&config).unwrap();
        let after = rotate(&config).unwrap();
        // A new self-signed cert is generated on each call, so DER bytes differ
        assert_ne!(
            before.cert_der, after.cert_der,
            "rotate should produce a new cert"
        );
    }

    #[test]
    fn check_rotation_needed_fresh_cert_returns_false() {
        let tmp = TempDir::new().unwrap();
        let config = cfg(&tmp);
        generate_and_save(&config).unwrap();
        assert!(
            !check_rotation_needed(&config),
            "freshly generated cert should not need rotation"
        );
    }
}
