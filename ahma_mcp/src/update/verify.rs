//! Single source of truth for release artifact verification.
//!
//! Verifies SLSA Build Provenance Attestations issued by Sigstore on behalf of
//! GitHub Actions. No private keys, no embedded public keys, no rotation.
//!
//! ## Trust model
//!
//! Every release archive and raw binary is attested by [`actions/attest-build-provenance`](
//! https://github.com/actions/attest-build-provenance) which issues an ephemeral X.509
//! certificate from Sigstore's Fulcio CA. The certificate is bound to the GitHub Actions
//! OIDC identity (`paulirotta/ahma` repo, `refs/heads/main`). Every attestation is recorded
//! in Sigstore's Rekor transparency log.
//!
//! ## Callers
//!
//! - `ahma verify <path>` — explicit CLI verification.
//! - `ahma verify --self` — runs against the running binary; called by install scripts
//!   as a post-install smoke test.
//! - `ahma update` — runs against each downloaded archive before installing.
//!
//! ## Escape hatch
//!
//! Set `AHMA_INSECURE_SKIP_VERIFY=1` (or `AHMA_INSECURE_SKIP_SIGNATURE=1` for backward
//! compatibility) to bypass attestation verification. This should only be used in
//! air-gapped environments or when Sigstore/GitHub API is unreachable.

use anyhow::{Context, Result, bail};
use clap::Args;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const OWNER: &str = "paulirotta";
const REPO: &str = "ahma";

/// CLI arguments for `ahma verify`.
#[derive(Args, Debug, Clone)]
#[command(
    about = "Verify an artifact's GitHub Build Provenance Attestation (Sigstore SLSA Level 3)",
    long_about = "Verify that an artifact was produced by the official paulirotta/ahma CI pipeline.\n\n\
        Uses GitHub's Sigstore-backed Build Provenance Attestations to prove that a binary or \
        archive was built from the paulirotta/ahma repository on the main branch.\n\n\
        Set AHMA_INSECURE_SKIP_VERIFY=1 to skip verification (offline/air-gapped use only).",
    after_help = "EXAMPLES:
  # Verify a downloaded archive
  ahma verify ahma-release-linux-x86_64.tar.gz

  # Verify the currently installed ahma binary
  ahma verify --self

  # Out-of-band verification with gh CLI
  gh attestation verify ahma-release-linux-x86_64.tar.gz --repo paulirotta/ahma"
)]
pub struct VerifyArgs {
    /// Path to the artifact to verify. Omit to verify the running ahma binary.
    pub path: Option<PathBuf>,

    /// Verify the currently running ahma binary (conflicts with PATH argument).
    #[arg(long = "self", conflicts_with = "path")]
    pub self_check: bool,
}

/// Verify an artifact was built by the official paulirotta/ahma workflow on main.
///
/// Returns `Ok(())` on success. On failure the error message describes exactly what
/// constraint failed (no attestation, wrong identity, expired cert, network error, etc.)
/// so install scripts can surface it verbatim.
///
/// Set `AHMA_INSECURE_SKIP_VERIFY=1` (or `AHMA_INSECURE_SKIP_SIGNATURE=1`) to skip.
pub async fn verify_artifact(path: &Path) -> Result<()> {
    if should_skip_verify() {
        eprintln!(
            "WARNING: Sigstore attestation verification bypassed via AHMA_INSECURE_SKIP_VERIFY. \
             Only use this in offline/air-gapped environments."
        );
        return Ok(());
    }

    let sha256 = sha256_hex(path)?;
    tracing::debug!(
        "Verifying attestation for {} (sha256:{})",
        path.display(),
        sha256
    );

    let verified =
        sigstore_verification::verify_github_attestation(path, OWNER, REPO, None, None)
            .await
            .with_context(|| {
                format!(
                    "Failed to query GitHub attestation API for {} (sha256:{}).\n\
                     Check your network connection or set AHMA_INSECURE_SKIP_VERIFY=1 for offline use.",
                    path.display(),
                    sha256
                )
            })?;

    if !verified {
        bail!(
            "No GitHub Build Provenance attestation found for {} (sha256:{}).\n\
             This artifact was not produced by the official paulirotta/ahma CI pipeline,\n\
             or the attestation is not yet available.\n\
             Out-of-band check: gh attestation verify {} --repo paulirotta/ahma\n\
             Set AHMA_INSECURE_SKIP_VERIFY=1 to bypass (only for offline/air-gapped use).",
            path.display(),
            sha256,
            path.display()
        );
    }

    tracing::info!(
        "Attestation verified: {} was built by {}/{} on main.",
        path.display(),
        OWNER,
        REPO
    );
    Ok(())
}

/// Verify the currently running ahma binary.
pub async fn verify_self() -> Result<()> {
    let path = std::env::current_exe()
        .context("Cannot resolve the path to the current ahma binary for self-verification.")?;
    verify_artifact(&path).await
}

/// Entry point for `ahma verify` CLI subcommand.
pub async fn run_cli(args: VerifyArgs) -> Result<()> {
    let path = if let Some(p) = args.path.filter(|_| !args.self_check) {
        p
    } else {
        std::env::current_exe().context(
            "Cannot resolve the current ahma binary path. \
             Pass an explicit path instead: ahma verify <path>",
        )?
    };

    if !path.exists() {
        bail!("Artifact not found: {}", path.display());
    }

    println!("Verifying: {} ...", path.display());
    verify_artifact(&path).await?;
    println!(
        "Verified: {} was built by {}/{} via the official CI pipeline.",
        path.display(),
        OWNER,
        REPO
    );
    Ok(())
}

fn should_skip_verify() -> bool {
    for var in &["AHMA_INSECURE_SKIP_VERIFY", "AHMA_INSECURE_SKIP_SIGNATURE"] {
        if std::env::var(var)
            .ok()
            .is_some_and(|val| matches!(val.trim(), "1" | "true" | "yes" | "on"))
        {
            return true;
        }
    }
    false
}

fn sha256_hex(path: &Path) -> Result<String> {
    let bytes =
        std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex};
    use tempfile::tempdir;

    // Serialize env-var-touching tests so they don't race each other.
    static ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    // SAFETY: all env-var writes are guarded by ENV_MUTEX; nextest runs each
    // test binary in an isolated process, so there is no cross-binary interference.

    #[test]
    fn skip_verify_false_when_unset() {
        let _g = ENV_MUTEX.lock().unwrap();
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_VERIFY") };
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_SIGNATURE") };
        assert!(!should_skip_verify());
    }

    #[test]
    fn skip_verify_truthy_values() {
        let _g = ENV_MUTEX.lock().unwrap();
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_SIGNATURE") };
        for val in ["1", "true", "yes", "on"] {
            unsafe { std::env::set_var("AHMA_INSECURE_SKIP_VERIFY", val) };
            assert!(
                should_skip_verify(),
                "expected skip for AHMA_INSECURE_SKIP_VERIFY={val}"
            );
        }
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_VERIFY") };
    }

    #[test]
    fn skip_verify_falsy_values() {
        let _g = ENV_MUTEX.lock().unwrap();
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_SIGNATURE") };
        for val in ["0", "false", "no", "off"] {
            unsafe { std::env::set_var("AHMA_INSECURE_SKIP_VERIFY", val) };
            assert!(
                !should_skip_verify(),
                "expected no skip for AHMA_INSECURE_SKIP_VERIFY={val}"
            );
        }
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_VERIFY") };
    }

    #[test]
    fn skip_verify_trims_whitespace() {
        let _g = ENV_MUTEX.lock().unwrap();
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_SIGNATURE") };
        unsafe { std::env::set_var("AHMA_INSECURE_SKIP_VERIFY", " 1 ") };
        assert!(should_skip_verify());
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_VERIFY") };
    }

    #[test]
    fn skip_verify_legacy_var_honored() {
        let _g = ENV_MUTEX.lock().unwrap();
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_VERIFY") };
        unsafe { std::env::set_var("AHMA_INSECURE_SKIP_SIGNATURE", "1") };
        assert!(should_skip_verify());
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_SIGNATURE") };
    }

    #[test]
    fn sha256_known_value() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hello.bin");
        std::fs::write(&path, b"hello").unwrap();
        // echo -n "hello" | sha256sum
        assert_eq!(
            sha256_hex(&path).unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn sha256_empty_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.bin");
        std::fs::write(&path, b"").unwrap();
        // sha256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert_eq!(
            sha256_hex(&path).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_missing_file_returns_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.bin");
        let err = sha256_hex(&path).unwrap_err();
        assert!(
            err.to_string().contains("Failed to read"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn verify_artifact_ok_when_skip_env_set() {
        let _g = ENV_MUTEX.lock().unwrap();
        unsafe { std::env::set_var("AHMA_INSECURE_SKIP_VERIFY", "1") };
        // Path need not exist — we return before reading it.
        let result = verify_artifact(std::path::Path::new("/nonexistent/artifact")).await;
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_VERIFY") };
        assert!(
            result.is_ok(),
            "expected Ok when skip env var is set: {result:?}"
        );
    }

    #[tokio::test]
    async fn run_cli_errors_on_missing_artifact() {
        use std::path::PathBuf;
        let _g = ENV_MUTEX.lock().unwrap();
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_VERIFY") };
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_SIGNATURE") };
        let args = VerifyArgs {
            path: Some(PathBuf::from("/nonexistent/path/artifact.bin")),
            self_check: false,
        };
        let err = run_cli(args).await.unwrap_err();
        assert!(
            err.to_string().contains("Artifact not found"),
            "unexpected error: {err}"
        );
    }
}
