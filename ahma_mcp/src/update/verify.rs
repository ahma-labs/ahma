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
