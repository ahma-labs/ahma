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
    Ok(super::sha256_hex(&bytes))
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

    // Holds the std `ENV_MUTEX` guard across the `.await`: the process-global env
    // var must stay set for the whole call, so the lock has to span the await to
    // keep concurrent env-mutating tests out. Safe here — `#[tokio::test]` uses a
    // current-thread runtime (the guard never moves between threads) and nothing
    // under the lock re-acquires `ENV_MUTEX`, so it cannot deadlock.
    #[allow(clippy::await_holding_lock)]
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

    // See `verify_artifact_ok_when_skip_env_set`: the `ENV_MUTEX` guard must span
    // the await so the cleared env vars stay cleared for the whole `run_cli` call.
    // Safe for the same reasons (current-thread test runtime, no re-lock).
    #[allow(clippy::await_holding_lock)]
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

    // Covers `verify_self` (lines 120-124): resolves `current_exe()` and delegates
    // to `verify_artifact`. With the skip env var set, `verify_artifact` returns
    // before any network call, so this exercises the resolve+delegate path offline.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn verify_self_ok_when_skip_env_set() {
        let _g = ENV_MUTEX.lock().unwrap();
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_SIGNATURE") };
        unsafe { std::env::set_var("AHMA_INSECURE_SKIP_VERIFY", "1") };
        let result = verify_self().await;
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_VERIFY") };
        assert!(
            result.is_ok(),
            "expected Ok from verify_self when skip env var is set: {result:?}"
        );
    }

    // Covers `run_cli` happy path with an explicit existing artifact:
    // the `args.path.filter(|_| !args.self_check)` Some-branch (line 128-129),
    // the `path.exists()` true case (skips the bail at 137-138), and the success
    // println!/verify/println! tail (lines 141-149). Offline via skip env var.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn run_cli_ok_with_existing_artifact_and_skip_env() {
        let _g = ENV_MUTEX.lock().unwrap();
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_SIGNATURE") };
        unsafe { std::env::set_var("AHMA_INSECURE_SKIP_VERIFY", "1") };

        let dir = tempdir().unwrap();
        let artifact = dir.path().join("ahma-release.tar.gz");
        std::fs::write(&artifact, b"pretend-archive-bytes").unwrap();

        let args = VerifyArgs {
            path: Some(artifact.clone()),
            self_check: false,
        };
        let result = run_cli(args).await;
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_VERIFY") };
        assert!(
            result.is_ok(),
            "expected Ok from run_cli on existing artifact with skip env set: {result:?}"
        );
    }

    // Covers `run_cli`'s self-check branch: with `self_check == true` the
    // `filter(|_| !args.self_check)` drops the provided path (line 128 false arm),
    // falling through to `current_exe()` resolution (lines 130-135). The running
    // test binary exists, so it proceeds through the success tail with skip set.
    // Also confirms an explicit `path` is ignored when `self_check` is true.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn run_cli_self_check_uses_current_exe_and_ignores_path() {
        let _g = ENV_MUTEX.lock().unwrap();
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_SIGNATURE") };
        unsafe { std::env::set_var("AHMA_INSECURE_SKIP_VERIFY", "1") };

        // Provide a bogus, non-existent path: it must be ignored because self_check
        // is true, so resolution falls back to current_exe (which exists).
        let args = VerifyArgs {
            path: Some(PathBuf::from("/nonexistent/should/be/ignored.bin")),
            self_check: true,
        };
        let result = run_cli(args).await;
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_VERIFY") };
        assert!(
            result.is_ok(),
            "expected Ok from run_cli --self (current_exe) with skip env set: {result:?}"
        );
    }

    // Covers `run_cli` self-check with no explicit path (None) — the other entry
    // into the `current_exe()` fallback. Distinct from the test above which passes
    // Some(..) + self_check; here `args.path` is None outright.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn run_cli_self_check_with_no_path_uses_current_exe() {
        let _g = ENV_MUTEX.lock().unwrap();
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_SIGNATURE") };
        unsafe { std::env::set_var("AHMA_INSECURE_SKIP_VERIFY", "1") };
        let args = VerifyArgs {
            path: None,
            self_check: true,
        };
        let result = run_cli(args).await;
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_VERIFY") };
        assert!(
            result.is_ok(),
            "expected Ok from run_cli --self with None path and skip env set: {result:?}"
        );
    }

    // Exercises `sha256_hex` on a larger, multi-kilobyte input (existing tests only
    // use <=5 byte inputs) and asserts the hex formatting invariant: 64 lowercase
    // hex chars, fully deterministic across repeated calls.
    #[test]
    fn sha256_larger_file_is_deterministic_64_hex() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("large.bin");
        // ~256 KiB of a repeating, non-trivial byte pattern.
        let mut data = Vec::with_capacity(256 * 1024);
        for i in 0..(256 * 1024) {
            data.push((i % 251) as u8);
        }
        std::fs::write(&path, &data).unwrap();

        let first = sha256_hex(&path).unwrap();
        let second = sha256_hex(&path).unwrap();
        assert_eq!(first, second, "sha256_hex must be deterministic");
        assert_eq!(first.len(), 64, "sha256 hex must be 64 chars: {first}");
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "sha256 hex must be lowercase hex: {first}"
        );
    }
}
