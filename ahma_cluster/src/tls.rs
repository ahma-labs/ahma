//! mTLS certificate management for ahma cluster peers.
//!
//! Provides helpers to:
//! - Generate a self-signed CA + leaf certificate pair (`generate_self_signed_cluster_certs`)
//! - Load an existing cert bundle from a directory (`load_from_dir`)
//!
//! Generated PEM files:
//! - `ca.pem`   — self-signed CA certificate (share with all peers)
//! - `cert.pem` — leaf certificate signed by the local CA
//! - `key.pem`  — private key for the leaf certificate (**keep secret**)

use std::path::Path;

use anyhow::{Context, Result};
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
use tracing::info;

/// mTLS configuration loaded from a directory containing `ca.pem`, `cert.pem`,
/// and `key.pem`.
#[derive(Debug, Clone)]
pub struct ClusterTlsConfig {
    /// PEM-encoded CA certificate (used to verify peer certificates).
    pub ca_pem: String,
    /// PEM-encoded leaf certificate for this peer.
    pub cert_pem: String,
    /// PEM-encoded private key for the leaf certificate.
    pub key_pem: String,
}

/// Generate a self-signed CA and a leaf certificate, writing three PEM files
/// to `out_dir`.
pub fn generate_self_signed_cluster_certs(out_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("Failed to create cert directory {}", out_dir.display()))?;

    // CA key + certificate
    let ca_key = KeyPair::generate().context("Failed to generate CA key pair")?;
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let mut ca_dn = DistinguishedName::new();
    ca_dn.push(DnType::OrganizationName, "ahma-cluster");
    ca_dn.push(DnType::CommonName, "ahma cluster CA");
    ca_params.distinguished_name = ca_dn;
    ca_params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    let ca_cert = ca_params
        .self_signed(&ca_key)
        .context("Failed to self-sign CA certificate")?;

    // Leaf key + certificate signed by the CA
    let leaf_key = KeyPair::generate().context("Failed to generate leaf key pair")?;
    let hostname = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "ahma-worker".to_owned());

    let mut leaf_params = CertificateParams::default();
    leaf_params.subject_alt_names.push(SanType::DnsName(
        hostname
            .clone()
            .try_into()
            .map_err(|e| anyhow::anyhow!("Invalid DNS SAN '{hostname}': {e:?}"))?,
    ));
    let mut leaf_dn = DistinguishedName::new();
    leaf_dn.push(DnType::OrganizationName, "ahma-cluster");
    leaf_dn.push(DnType::CommonName, hostname);
    leaf_params.distinguished_name = leaf_dn;
    leaf_params.extended_key_usages = vec![
        rcgen::ExtendedKeyUsagePurpose::ClientAuth,
        rcgen::ExtendedKeyUsagePurpose::ServerAuth,
    ];
    // rcgen 0.14 signs against an `Issuer` (DN + key usages + signing key) rather
    // than a (certificate, key) pair; `from_params` borrows the CA params we just
    // self-signed, so the issuer identity is identical to the written ca.pem.
    let ca_issuer = rcgen::Issuer::from_params(&ca_params, &ca_key);
    let leaf_cert = leaf_params
        .signed_by(&leaf_key, &ca_issuer)
        .context("Failed to sign leaf certificate with CA")?;

    // Write PEM files
    let ca_path = out_dir.join("ca.pem");
    let cert_path = out_dir.join("cert.pem");
    let key_path = out_dir.join("key.pem");

    std::fs::write(&ca_path, ca_cert.pem())
        .with_context(|| format!("Failed to write {}", ca_path.display()))?;
    std::fs::write(&cert_path, leaf_cert.pem())
        .with_context(|| format!("Failed to write {}", cert_path.display()))?;
    std::fs::write(&key_path, leaf_key.serialize_pem())
        .with_context(|| format!("Failed to write {}", key_path.display()))?;

    // Restrict key file permissions on Unix so only the owner can read it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("Failed to chmod 600 {}", key_path.display()))?;
    }

    info!("mTLS certificates written to {}", out_dir.display());
    Ok(())
}

/// Load an existing mTLS certificate bundle from a directory.
///
/// Expects `ca.pem`, `cert.pem`, and `key.pem` to exist in `dir`.
pub fn load_from_dir(dir: &Path) -> Result<ClusterTlsConfig> {
    let ca_pem = std::fs::read_to_string(dir.join("ca.pem"))
        .with_context(|| format!("Failed to read {}/ca.pem", dir.display()))?;
    let cert_pem = std::fs::read_to_string(dir.join("cert.pem"))
        .with_context(|| format!("Failed to read {}/cert.pem", dir.display()))?;
    let key_pem = std::fs::read_to_string(dir.join("key.pem"))
        .with_context(|| format!("Failed to read {}/key.pem", dir.display()))?;
    Ok(ClusterTlsConfig {
        ca_pem,
        cert_pem,
        key_pem,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_generate_and_load_certs() {
        let dir = tempdir().unwrap();
        generate_self_signed_cluster_certs(dir.path()).unwrap();

        assert!(dir.path().join("ca.pem").exists());
        assert!(dir.path().join("cert.pem").exists());
        assert!(dir.path().join("key.pem").exists());

        let cfg = load_from_dir(dir.path()).unwrap();
        assert!(cfg.ca_pem.contains("BEGIN CERTIFICATE"));
        assert!(cfg.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(cfg.key_pem.contains("PRIVATE KEY"));
    }

    #[test]
    fn test_load_from_nonexistent_dir_errors() {
        let result = load_from_dir(std::path::Path::new("/nonexistent/path/certs"));
        assert!(result.is_err());
    }

    /// Key file must be chmod 600 so the private key is not world-readable.
    #[cfg(unix)]
    #[test]
    fn test_key_file_has_restricted_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        generate_self_signed_cluster_certs(dir.path()).unwrap();
        let meta = std::fs::metadata(dir.path().join("key.pem")).unwrap();
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o600,
            "key.pem must be chmod 600 (got {:o})",
            meta.permissions().mode() & 0o777
        );
    }

    /// The leaf certificate PEM must be parseable as a single DER certificate.
    #[test]
    fn test_leaf_cert_parseable_as_der() {
        use rustls::pki_types::{CertificateDer, pem::PemObject};
        let dir = tempdir().unwrap();
        generate_self_signed_cluster_certs(dir.path()).unwrap();
        let cfg = load_from_dir(dir.path()).unwrap();
        let certs: Vec<CertificateDer<'static>> =
            CertificateDer::pem_slice_iter(cfg.cert_pem.as_bytes())
                .collect::<Result<Vec<_>, _>>()
                .expect("leaf cert PEM should be valid DER");
        assert_eq!(
            certs.len(),
            1,
            "leaf cert PEM should contain exactly one certificate"
        );
    }

    /// The CA and leaf certificates must be distinct (different public keys / DER).
    #[test]
    fn test_ca_cert_is_different_from_leaf() {
        let dir = tempdir().unwrap();
        generate_self_signed_cluster_certs(dir.path()).unwrap();
        let cfg = load_from_dir(dir.path()).unwrap();
        assert_ne!(
            cfg.ca_pem, cfg.cert_pem,
            "CA certificate must be different from the leaf certificate"
        );
    }

    /// `key.pem` must contain a parseable private key.
    #[test]
    fn test_private_key_parseable() {
        use rustls::pki_types::{PrivateKeyDer, pem::PemObject};
        let dir = tempdir().unwrap();
        generate_self_signed_cluster_certs(dir.path()).unwrap();
        let cfg = load_from_dir(dir.path()).unwrap();
        PrivateKeyDer::from_pem_slice(cfg.key_pem.as_bytes())
            .expect("key.pem must contain a parseable private key");
    }
}
