//! TLS certificate generation for the HTTP/3 (QUIC) server.
//!
//! Prefers loading the persistent certificate from `~/.ahma/tls/` (managed by
//! [`ahma_common::local_tls`]).  Falls back to generating a fresh ephemeral
//! self-signed certificate when the persistent store is unavailable (e.g. on
//! first run before `ahma tls init` has been called, or when the directory is
//! not writable).
//!
//! The certificate DER bytes are exported so test clients can add them to their
//! trust stores via `reqwest::ClientBuilder::add_root_certificate()`.

use anyhow::{Context, Result};
use std::sync::Arc;

/// Self-signed TLS certificate and private key for the QUIC server.
pub struct SelfSignedCert {
    /// DER-encoded certificate bytes (for rustls and for exporting to clients).
    pub cert_der: Vec<u8>,
    /// DER-encoded private key bytes (for rustls).
    pub key_der: Vec<u8>,
}

/// Load the persistent TLS certificate from `~/.ahma/tls/`, generating new
/// material if the directory is empty.
///
/// Falls back to an ephemeral self-signed certificate on any I/O or generation
/// error so that the QUIC endpoint always starts.
pub fn load_or_generate() -> SelfSignedCert {
    let config = ahma_common::local_tls::LocalTlsConfig::from_env();
    match ahma_common::local_tls::provision_if_needed(&config) {
        Ok(certs) => {
            tracing::info!(
                "QUIC: using persistent TLS certificate from {}",
                config.dir.display()
            );
            SelfSignedCert {
                cert_der: certs.cert_der,
                key_der: certs.key_der,
            }
        }
        Err(e) => {
            tracing::warn!(
                "QUIC: could not load/generate persistent TLS cert ({}); \
                 using ephemeral certificate. Run `ahma tls init` to persist it.",
                e
            );
            generate_self_signed_cert().unwrap_or_else(|e2| {
                // This should never happen — rcgen generation is infallible in practice.
                panic!("QUIC: failed to generate ephemeral self-signed cert: {e2}");
            })
        }
    }
}

/// Generate a self-signed TLS certificate valid for `127.0.0.1` and `localhost`.
pub fn generate_self_signed_cert() -> Result<SelfSignedCert> {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string(), "localhost".to_string()])
            .context("Failed to generate self-signed certificate")?;

    let cert_der = cert.der().to_vec();
    let key_der = signing_key.serialize_der();

    Ok(SelfSignedCert { cert_der, key_der })
}

/// Build a rustls `ServerConfig` suitable for HTTP/3 (QUIC) with the given certificate.
pub fn build_quic_tls_config(cert: &SelfSignedCert) -> Result<Arc<rustls::ServerConfig>> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    let cert_der = CertificateDer::from(cert.cert_der.clone());
    let key_der = PrivateKeyDer::try_from(cert.key_der.clone())
        .map_err(|e| anyhow::anyhow!("Invalid private key DER: {}", e))?;

    // Explicitly select the ring crypto provider to avoid ambiguity when both
    // `ring` and `aws-lc-rs` features are enabled by transitive dependencies.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls_config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .context("Failed to build TLS server config with TLS 1.3")?
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .context("Failed to build TLS server config")?;

    // HTTP/3 requires the h3 ALPN token; 0-RTT reduces latency on reconnect.
    tls_config.max_early_data_size = u32::MAX;
    tls_config.alpn_protocols = vec![b"h3".to_vec()];

    Ok(Arc::new(tls_config))
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Install the ring crypto provider as process default if not already set.
    /// `build_quic_tls_config` passes the provider explicitly, so this is only a
    /// defensive no-op guard; harmless if a provider is already installed.
    fn ensure_crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    #[test]
    fn generate_self_signed_cert_produces_valid_der() {
        let cert = generate_self_signed_cert().expect("generation should succeed");
        assert!(!cert.cert_der.is_empty(), "cert_der must be non-empty");
        assert!(!cert.key_der.is_empty(), "key_der must be non-empty");
        // A DER-encoded X.509 certificate is a SEQUENCE, whose tag byte is 0x30.
        assert_eq!(
            cert.cert_der[0], 0x30,
            "cert_der should start with the DER SEQUENCE tag 0x30"
        );
    }

    #[test]
    fn build_quic_tls_config_sets_h3_alpn_and_early_data() {
        ensure_crypto_provider();
        let cert = generate_self_signed_cert().expect("generation should succeed");
        let config = build_quic_tls_config(&cert).expect("valid cert should build a config");

        assert!(
            config.alpn_protocols.contains(&b"h3".to_vec()),
            "alpn_protocols must contain the h3 token"
        );
        assert_eq!(
            config.max_early_data_size,
            u32::MAX,
            "max_early_data_size must be u32::MAX for 0-RTT"
        );
    }

    #[test]
    fn build_quic_tls_config_rejects_invalid_key() {
        ensure_crypto_provider();
        let cert = generate_self_signed_cert().expect("generation should succeed");
        // Replace the private key with garbage DER so the build chain
        // (PrivateKeyDer::try_from / with_single_cert) fails.
        let bad = SelfSignedCert {
            cert_der: cert.cert_der,
            key_der: vec![0, 1, 2, 3],
        };
        let result = build_quic_tls_config(&bad);
        assert!(
            result.is_err(),
            "invalid private key DER must produce an Err"
        );
    }

    #[test]
    fn load_or_generate_returns_non_empty_cert() {
        // `LocalTlsConfig::from_env()` deliberately has NO env injection seam
        // (AHMA_TLS_DIR is retired/ignored). The only injectable seam is the
        // process-wide `set_dir_override`, which we point at a tempdir so the
        // Ok branch (load/generate persistent material) is exercised
        // deterministically without touching the real ~/.ahma/tls directory.
        let tmp = tempfile::tempdir().expect("tempdir");
        ahma_common::local_tls::LocalTlsConfig::set_dir_override(tmp.path().join("tls"));

        let cert = load_or_generate();
        assert!(!cert.cert_der.is_empty(), "cert_der must be non-empty");
        assert!(!cert.key_der.is_empty(), "key_der must be non-empty");
        assert_eq!(
            cert.cert_der[0], 0x30,
            "cert_der should start with the DER SEQUENCE tag 0x30"
        );

        // Keep the tempdir alive until after the cert has been read.
        drop(tmp);
    }
}
