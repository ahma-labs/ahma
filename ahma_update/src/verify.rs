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
//! OIDC identity (`ahma-labs/ahma` repo, `refs/heads/main`). Every attestation is recorded
//! in Sigstore's Rekor transparency log.
//!
//! ## What is checked
//!
//! Verification runs directly on the [`sigstore`] crate (there is no wrapper crate in
//! between). For an artifact to pass, **one** attestation GitHub holds for its
//! `sha256:` digest must satisfy all of:
//!
//! 1. The signing certificate chains to a Fulcio CA in the public-good Sigstore
//!    trust root, fetched over TUF at verification time.
//! 2. The certificate's embedded Signed Certificate Timestamp verifies against the
//!    trust root's CT-log keys.
//! 3. The certificate matches the identity policy in this module's `policy`
//!    submodule: GitHub Actions' OIDC issuer, workflow repository
//!    `ahma-labs/ahma`, and a signer URI under `https://github.com/ahma-labs/ahma/`.
//! 4. The DSSE signature verifies over the envelope's pre-authentication encoding.
//! 5. The signed in-toto statement lists our artifact's sha256 among its subjects.
//! 6. The bundle's Rekor transparency-log entry describes this very envelope, and
//!    the certificate was still valid when the entry was logged.
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

mod github_attestation;
mod policy;

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use std::path::{Path, PathBuf};

use github_attestation::GITHUB_API_BASE;
use policy::AhmaReleaseIdentity;
use sigstore::bundle::Bundle;
use sigstore::bundle::verify::{VerificationError, Verifier};

const OWNER: &str = "ahma-labs";
const REPO: &str = "ahma";

/// Passed to [`Verifier::verify`] as its `offline` flag.
///
/// `false` is the stricter setting: it makes `sigstore` reject a bundle whose
/// transparency-log entry carries only an inclusion *promise* and no inclusion
/// proof. Bundles GitHub issues today (`…bundle.v0.3+json`) always carry a full
/// inclusion proof with a checkpoint, so this costs nothing and closes the door
/// on the weaker legacy profile.
const SIGSTORE_OFFLINE: bool = false;

/// The exact message `sigstore` renders for `SignatureErrorKind::Transparency`.
///
/// **Why this is tolerated.** `sigstore` 0.14 raises this error from exactly two
/// places in `bundle::verify::Verifier::verify_digest`, and both are reached only
/// *after* the certificate chain, the embedded SCT, the identity policy and the
/// DSSE signature have all passed:
///
/// 1. the in-toto statement's **`subject[0]`** digest does not equal the input
///    digest, and
/// 2. the transparency-log entry is not consistent with the bundle.
///
/// Neither is usable as-is for GitHub Build Provenance:
///
/// * (1) `sigstore` 0.14 only ever inspects `subject[0]`, but ahma's release
///   workflow attests the archive and the raw binary in **one** statement, so
///   `ahma verify --self` — which looks up the binary — legitimately matches
///   `subject[1]`.
/// * (2) the log-entry check re-serialises the parsed DSSE envelope to recompute
///   Rekor's `envelopeHash`. That does not round-trip for GitHub's bundles, so
///   *every* GitHub attestation trips it (confirmed against a published ahma
///   release: chain, SCT, policy and signature all pass, then `tlog dsse
///   envelopeHash mismatch`).
///
/// [`Candidate::verify`] therefore tolerates this one error — and only after
/// establishing, itself and more strictly, everything the error could stand for:
/// the artifact digest is among **all** the statement's subjects, and the
/// transparency-log entry is bound to this envelope, this signature, this
/// certificate and that certificate's validity window (see
/// [`github_attestation::verify_log_entry`]).
///
/// The match is on the variant *and* this message, so if a future `sigstore`
/// reworded it, or fixed the round-trip, the tolerance simply stops applying:
/// either verification succeeds outright or it fails closed. It is never widened
/// to `VerificationError::Signature(_)`, which also covers "signature
/// verification failed".
const SIGSTORE_TRANSPARENCY_ERROR: &str = "signature transparency materials are inconsistent";

/// CLI arguments for `ahma verify`.
#[derive(Args, Debug, Clone)]
#[command(
    about = "Verify an artifact's GitHub Build Provenance Attestation (Sigstore SLSA Level 3)",
    long_about = "Verify that an artifact was produced by the official ahma-labs/ahma CI pipeline.\n\n\
        Uses GitHub's Sigstore-backed Build Provenance Attestations to prove that a binary or \
        archive was built from the ahma-labs/ahma repository on the main branch.\n\n\
        Set AHMA_INSECURE_SKIP_VERIFY=1 to skip verification (offline/air-gapped use only).",
    after_help = "EXAMPLES:
  # Verify a downloaded archive
  ahma verify ahma-release-linux-x86_64.tar.gz

  # Verify the currently installed ahma binary
  ahma verify --self

  # Out-of-band verification with gh CLI
  gh attestation verify ahma-release-linux-x86_64.tar.gz --repo ahma-labs/ahma"
)]
pub struct VerifyArgs {
    /// Path to the artifact to verify. Omit to verify the running ahma binary.
    pub path: Option<PathBuf>,

    /// Verify the currently running ahma binary (conflicts with PATH argument).
    #[arg(long = "self", conflicts_with = "path")]
    pub self_check: bool,
}

/// Verify an artifact was built by the official ahma-labs/ahma workflow on main.
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

    let client = crate::github::client()?;

    verify_artifact_via(&client, GITHUB_API_BASE, path).await
}

/// [`verify_artifact`] with an injectable API base and HTTP client, so the
/// attestation lookup can be exercised against a local mock server.
async fn verify_artifact_via(client: &reqwest::Client, api_base: &str, path: &Path) -> Result<()> {
    let sha256 = artifact_sha256_hex(path).await?;
    tracing::debug!(
        "Verifying attestation for {} (sha256:{})",
        path.display(),
        sha256
    );

    let bundles =
        github_attestation::fetch_bundles(client, api_base, OWNER, REPO, &sha256, None)
            .await
            .with_context(|| {
                format!(
                    "Failed to query GitHub attestation API for {} (sha256:{}).\n\
                     Check your network connection or set AHMA_INSECURE_SKIP_VERIFY=1 for offline use.",
                    path.display(),
                    sha256
                )
            })?;

    if bundles.is_empty() {
        bail!(
            "No GitHub Build Provenance attestation found for {} (sha256:{}).\n\
             This artifact was not produced by the official ahma-labs/ahma CI pipeline,\n\
             or the attestation is not yet available.\n\
             Out-of-band check: gh attestation verify {} --repo ahma-labs/ahma\n\
             Set AHMA_INSECURE_SKIP_VERIFY=1 to bypass (only for offline/air-gapped use).",
            path.display(),
            sha256,
            path.display()
        );
    }

    // Bind each attestation to *this* artifact first. This is offline and cheap,
    // and an attestation that does not cover our digest can never verify it, so
    // there is no reason to pay for a TUF round trip before ruling those out.
    let mut failures = Vec::new();
    let mut candidates = Vec::new();
    for bundle in &bundles {
        match Candidate::bind(bundle, &sha256) {
            Ok(candidate) => candidates.push(candidate),
            Err(e) => failures.push(format!("{e:#}")),
        }
    }

    if !candidates.is_empty() {
        // Fetching the trust root is a TUF round trip, so it happens only once we
        // know there is something to verify.
        let verifier = Verifier::production().await.map_err(|e| anyhow!(e)).context(
            "Failed to load the public-good Sigstore trust root from tuf-repo-cdn.sigstore.dev.\n\
             Check your network connection or set AHMA_INSECURE_SKIP_VERIFY=1 for offline use.",
        )?;
        let identity = AhmaReleaseIdentity::new(OWNER, REPO);

        for candidate in &candidates {
            match candidate.verify(&verifier, &identity, path).await {
                Ok(()) => {
                    tracing::info!(
                        "Attestation verified: {} was built by {}/{} on main.",
                        path.display(),
                        OWNER,
                        REPO
                    );
                    return Ok(());
                }
                Err(e) => {
                    tracing::debug!("Attestation rejected: {e:#}");
                    failures.push(format!("{e:#}"));
                }
            }
        }
    }

    bail!(
        "None of the {} GitHub attestation(s) for {} (sha256:{}) satisfies the\n\
         ahma-labs/ahma build-provenance policy. This artifact was not produced by the\n\
         official CI pipeline, or its attestation is not trustworthy.\n\
         Reasons:\n  - {}\n\
         Out-of-band check: gh attestation verify {} --repo ahma-labs/ahma\n\
         Set AHMA_INSECURE_SKIP_VERIFY=1 to bypass (only for offline/air-gapped use).",
        bundles.len(),
        path.display(),
        sha256,
        failures.join("\n  - "),
        path.display()
    );
}

/// An attestation that has already been bound to the artifact's digest and is
/// therefore worth verifying cryptographically.
#[derive(Debug)]
struct Candidate<'a> {
    bundle_json: &'a serde_json::Value,
    /// The signed in-toto statement bytes.
    payload: Vec<u8>,
    /// Index of the artifact in the statement's subject list.
    position: usize,
}

impl<'a> Candidate<'a> {
    /// Bind `bundle_json` to `sha256`, or explain why it cannot be.
    fn bind(bundle_json: &'a serde_json::Value, sha256: &str) -> Result<Self> {
        // The DSSE payload is the in-toto statement the signature covers;
        // `sigstore` reads the same `dsseEnvelope.payload` out of the same JSON
        // value, so a subject read here is a subject that signature protects.
        let payload = github_attestation::dsse_payload(bundle_json)?;
        let subjects = github_attestation::statement_subjects(&payload)?;

        // The artifact-to-statement binding. `sigstore` 0.14 performs only the
        // `subject[0]` form of this check; ours accepts any subject, which is
        // what lets `ahma verify --self` verify the raw binary out of an
        // attestation that also covers the release archive.
        let position = subjects.position_of(sha256).ok_or_else(|| {
            anyhow!("attestation does not list sha256:{sha256} among its subjects")
        })?;

        Ok(Self {
            bundle_json,
            payload,
            position,
        })
    }

    /// Full cryptographic and policy verification of this attestation.
    async fn verify(
        &self,
        verifier: &Verifier,
        identity: &AhmaReleaseIdentity,
        path: &Path,
    ) -> Result<()> {
        let bundle: Bundle = serde_json::from_value(self.bundle_json.clone())
            .context("attestation is not a well-formed Sigstore bundle")?;

        let artifact = tokio::fs::File::open(path)
            .await
            .with_context(|| format!("Failed to read {}", path.display()))?;

        match verifier
            .verify(artifact, bundle, &identity.as_policy(), SIGSTORE_OFFLINE)
            .await
        {
            Ok(()) => Ok(()),
            Err(e) if is_sigstore_transparency_error(&e) => {
                // See SIGSTORE_TRANSPARENCY_ERROR: reaching this error proves the
                // certificate chain, the SCT, the identity policy and the DSSE
                // signature all passed. Everything the error could stand for is
                // re-established below, independently and more strictly.
                let certificate = self.certificate()?;
                github_attestation::verify_log_entry(self.bundle_json, &self.payload, &certificate)
                    .with_context(|| {
                        format!(
                            "transparency-log entry is inconsistent with the attestation for {}",
                            path.display()
                        )
                    })?;
                tracing::debug!(
                    "Artifact is subject {} of the attestation; subject binding and \
                     transparency-log consistency checked directly.",
                    self.position
                );
                Ok(())
            }
            Err(e) => Err(anyhow!(e).context("Sigstore bundle verification failed")),
        }
    }

    /// The bundle's signing certificate.
    ///
    /// This is the same leaf `sigstore` chained to a Fulcio root and matched
    /// against the identity policy: the DER is read straight out of the bundle
    /// JSON `sigstore` was handed.
    fn certificate(&self) -> Result<x509_cert::Certificate> {
        use x509_cert::der::Decode as _;
        let der = github_attestation::certificate_der(self.bundle_json)?;
        x509_cert::Certificate::from_der(&der)
            .context("attestation signing certificate is not valid DER")
    }
}

/// Whether `err` is `sigstore`'s `SignatureErrorKind::Transparency`.
///
/// Matched on the variant *and* the rendered message: the variant alone also
/// covers "signature verification failed", which must never be tolerated.
fn is_sigstore_transparency_error(err: &VerificationError) -> bool {
    matches!(err, VerificationError::Signature(_)) && err.to_string() == SIGSTORE_TRANSPARENCY_ERROR
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

/// Lowercase hex sha256 of the artifact at `path` — the digest GitHub keys its
/// attestations by (`sha256:{digest}`).
async fn artifact_sha256_hex(path: &Path) -> Result<String> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("Failed to read {}", path.display()))?;
    Ok(super::sha256_hex(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::LazyLock;
    use tempfile::tempdir;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A real GitHub Build Provenance attestation for ahma v0.19.7.
    const BUNDLE: &str = include_str!("../tests/fixtures/ahma_build_provenance_bundle.json");
    /// The public-good Sigstore trusted root — Fulcio CA chains, CT-log keys and
    /// Rekor keys — verbatim from `trust_root/prod/trusted_root.json` in the
    /// `sigstore` crate (Apache-2.0). Test-only: production fetches the live copy
    /// over TUF so it stays current without a code change.
    const TRUSTED_ROOT: &str = include_str!("../tests/fixtures/sigstore_trusted_root.json");

    // Serialize env-var-touching tests so they don't race each other.
    static ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    // SAFETY: all env-var writes are guarded by ENV_MUTEX; nextest runs each
    // test binary in an isolated process, so there is no cross-binary interference.

    #[test]
    fn skip_verify_false_when_unset() {
        let _g = ENV_MUTEX.lock();
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_VERIFY") };
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_SIGNATURE") };
        assert!(!should_skip_verify());
    }

    #[test]
    fn skip_verify_truthy_values() {
        let _g = ENV_MUTEX.lock();
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
        let _g = ENV_MUTEX.lock();
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
        let _g = ENV_MUTEX.lock();
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_SIGNATURE") };
        unsafe { std::env::set_var("AHMA_INSECURE_SKIP_VERIFY", " 1 ") };
        assert!(should_skip_verify());
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_VERIFY") };
    }

    #[test]
    fn skip_verify_legacy_var_honored() {
        let _g = ENV_MUTEX.lock();
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_VERIFY") };
        unsafe { std::env::set_var("AHMA_INSECURE_SKIP_SIGNATURE", "1") };
        assert!(should_skip_verify());
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_SIGNATURE") };
    }

    #[tokio::test]
    async fn sha256_known_value() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hello.bin");
        std::fs::write(&path, b"hello").unwrap();
        // echo -n "hello" | sha256sum
        assert_eq!(
            artifact_sha256_hex(&path).await.unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[tokio::test]
    async fn sha256_empty_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.bin");
        std::fs::write(&path, b"").unwrap();
        // sha256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert_eq!(
            artifact_sha256_hex(&path).await.unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[tokio::test]
    async fn sha256_missing_file_returns_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.bin");
        let err = artifact_sha256_hex(&path).await.unwrap_err();
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
        let _g = ENV_MUTEX.lock();
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
        let _g = ENV_MUTEX.lock();
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

    // Covers `verify_self`: resolves `current_exe()` and delegates to
    // `verify_artifact`. With the skip env var set, `verify_artifact` returns
    // before any network call, so this exercises the resolve+delegate path offline.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn verify_self_ok_when_skip_env_set() {
        let _g = ENV_MUTEX.lock();
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_SIGNATURE") };
        unsafe { std::env::set_var("AHMA_INSECURE_SKIP_VERIFY", "1") };
        let result = verify_self().await;
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_VERIFY") };
        assert!(
            result.is_ok(),
            "expected Ok from verify_self when skip env var is set: {result:?}"
        );
    }

    // Covers `run_cli` happy path with an explicit existing artifact: the
    // `args.path.filter(|_| !args.self_check)` Some-branch, the `path.exists()`
    // true case, and the success println!/verify/println! tail. Offline via the
    // skip env var.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn run_cli_ok_with_existing_artifact_and_skip_env() {
        let _g = ENV_MUTEX.lock();
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
    // `filter(|_| !args.self_check)` drops the provided path, falling through to
    // `current_exe()` resolution. Also confirms an explicit `path` is ignored
    // when `self_check` is true.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn run_cli_self_check_uses_current_exe_and_ignores_path() {
        let _g = ENV_MUTEX.lock();
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
    // into the `current_exe()` fallback.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn run_cli_self_check_with_no_path_uses_current_exe() {
        let _g = ENV_MUTEX.lock();
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

    // Exercises the digest helper on a larger, multi-kilobyte input and asserts
    // the hex formatting invariant: 64 lowercase hex chars, deterministic.
    #[tokio::test]
    async fn sha256_larger_file_is_deterministic_64_hex() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("large.bin");
        // ~256 KiB of a repeating, non-trivial byte pattern.
        let mut data = Vec::with_capacity(256 * 1024);
        for i in 0..(256 * 1024) {
            data.push((i % 251) as u8);
        }
        std::fs::write(&path, &data).unwrap();

        let first = artifact_sha256_hex(&path).await.unwrap();
        let second = artifact_sha256_hex(&path).await.unwrap();
        assert_eq!(first, second, "sha256 must be deterministic");
        assert_eq!(first.len(), 64, "sha256 hex must be 64 chars: {first}");
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "sha256 hex must be lowercase hex: {first}"
        );
    }

    // ---- Attestation lookup, against a local mock GitHub API -----------------

    /// A 404 from the attestation API means "nobody attested this digest", which
    /// must surface as the actionable "not produced by the official pipeline"
    /// message rather than an HTTP error.
    #[tokio::test]
    async fn unattested_artifact_reports_no_attestation_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/repos/ahma-labs/ahma/attestations/.*$"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let artifact = dir.path().join("not-ours.tar.gz");
        std::fs::write(&artifact, b"definitely not an ahma release").unwrap();

        let client = reqwest::Client::new();
        let err = verify_artifact_via(&client, &server.uri(), &artifact)
            .await
            .expect_err("an unattested artifact must not verify");
        assert!(
            err.to_string()
                .contains("No GitHub Build Provenance attestation found"),
            "unexpected error: {err:#}"
        );
    }

    /// A non-404 API failure is a lookup failure, not a "no attestation" verdict:
    /// silently treating a 500 as "unattested" would be the same message a real
    /// forgery produces.
    #[tokio::test]
    async fn api_failure_is_reported_as_a_lookup_failure() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/repos/ahma-labs/ahma/attestations/.*$"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let artifact = dir.path().join("artifact.bin");
        std::fs::write(&artifact, b"bytes").unwrap();

        let client = reqwest::Client::new();
        let err = verify_artifact_via(&client, &server.uri(), &artifact)
            .await
            .expect_err("an API failure must not verify");
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("Failed to query GitHub attestation API"),
            "unexpected error: {rendered}"
        );
        assert!(
            rendered.contains("500"),
            "the HTTP status should be surfaced: {rendered}"
        );
    }

    /// The attestation lookup must be keyed by the artifact's own digest: the
    /// request path carries `sha256:{digest}`, so an attestation for some other
    /// file can never be fetched for this one.
    #[tokio::test]
    async fn attestation_is_requested_by_the_artifact_digest() {
        let server = MockServer::start().await;
        // sha256("hello")
        let expected = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        Mock::given(method("GET"))
            .and(path_regex(format!(
                r"^/repos/ahma-labs/ahma/attestations/sha256:{expected}$"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "attestations": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let artifact = dir.path().join("hello.bin");
        std::fs::write(&artifact, b"hello").unwrap();

        let client = reqwest::Client::new();
        let err = verify_artifact_via(&client, &server.uri(), &artifact)
            .await
            .expect_err("an empty attestation list must not verify");
        assert!(
            err.to_string()
                .contains("No GitHub Build Provenance attestation found"),
            "unexpected error: {err:#}"
        );
        // Mock `.expect(1)` is asserted on drop of the server.
    }

    /// A response whose `attestations[].bundle` is inline is parsed into
    /// `sigstore`'s bundle type; a bundle that does not cover our digest is
    /// rejected without ever reaching the trust root.
    #[tokio::test]
    async fn attestation_for_another_artifact_is_rejected() {
        let server = MockServer::start().await;
        let bundle: serde_json::Value = serde_json::from_str(BUNDLE).unwrap();
        Mock::given(method("GET"))
            .and(path_regex(r"^/repos/ahma-labs/ahma/attestations/.*$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "attestations": [{ "bundle": bundle, "bundle_url": null }]
            })))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let artifact = dir.path().join("impostor.tar.gz");
        std::fs::write(&artifact, b"not a real ahma release archive").unwrap();

        let client = reqwest::Client::new();
        let err = verify_artifact_via(&client, &server.uri(), &artifact)
            .await
            .expect_err("a bundle for a different artifact must not verify");
        let rendered = format!("{err:#}");
        // Rejected purely offline: no trust root is fetched for an attestation
        // that does not cover our digest.
        assert!(
            rendered.contains("among its subjects"),
            "unexpected error: {rendered}"
        );
        assert!(
            !rendered.contains("trust root"),
            "the subject binding must be checked before any TUF fetch: {rendered}"
        );
    }

    /// The other half of the binding: the *same* bundle does cover the digest of
    /// the artifact it was issued for, so binding succeeds and `position` says
    /// which subject matched. `subject[1]` is the raw `ahma` binary, i.e. the
    /// `ahma verify --self` case that `sigstore` 0.14 alone cannot express.
    #[test]
    fn candidate_binds_both_subjects_of_the_real_attestation() {
        let bundle: serde_json::Value = serde_json::from_str(BUNDLE).unwrap();
        let archive = "4bd93abdc592c4fd25428f965e8664fa9de641362f85490006c4b0ff0e5778d2";
        let binary = "837aed25c6f56eaa71a245ee90a1e52971f9bb796ee949fa8824608b665aa7e2";

        assert_eq!(Candidate::bind(&bundle, archive).unwrap().position, 0);
        assert_eq!(Candidate::bind(&bundle, binary).unwrap().position, 1);

        let err = Candidate::bind(&bundle, &"f".repeat(64)).unwrap_err();
        assert!(
            err.to_string().contains("among its subjects"),
            "unexpected error: {err}"
        );
    }

    // ---- Bundle and trust-root fixtures -------------------------------------

    /// The captured GitHub response deserializes into `sigstore`'s own bundle
    /// type. If a `sigstore` upgrade changed the bundle model, this fails before
    /// anything reaches a user.
    #[test]
    fn real_bundle_deserializes_into_the_sigstore_bundle_type() {
        let bundle: Bundle =
            serde_json::from_str(BUNDLE).expect("fixture must be a Sigstore bundle");
        assert_eq!(
            bundle.media_type,
            "application/vnd.dev.sigstore.bundle.v0.3+json"
        );
    }

    /// A `Verifier` can be built from a pinned trust root without any network,
    /// which proves the Fulcio certificate pool and the CT-log keyring the
    /// production path relies on are constructible from real trust material.
    #[test]
    fn verifier_builds_from_a_pinned_trust_root_offline() {
        use sigstore::trust::sigstore::SigstoreTrustRoot;
        let root = SigstoreTrustRoot::from_trusted_root_json_unchecked(TRUSTED_ROOT.as_bytes())
            .expect("the pinned trusted root must parse");
        Verifier::new(Default::default(), root)
            .expect("a verifier must be constructible from the pinned trust root");
    }

    /// The tolerated-error contract in one place: if `sigstore` rewords this
    /// message, [`is_sigstore_transparency_error`] stops matching and
    /// verification fails closed rather than silently accepting.
    #[test]
    fn transparency_error_message_is_the_one_we_tolerate() {
        assert_eq!(
            SIGSTORE_TRANSPARENCY_ERROR,
            "signature transparency materials are inconsistent"
        );
    }

    /// End-to-end against the live GitHub attestation API and Sigstore trust
    /// root. Ignored by default and additionally gated on `AHMA_NETWORK_TESTS=1`
    /// so `--run-ignored all` does not download a ~19 MB release archive.
    #[tokio::test]
    #[ignore = "network: downloads a published release archive and fetches the Sigstore TUF trust root; set AHMA_NETWORK_TESTS=1"]
    async fn verifies_a_published_release_archive_end_to_end() {
        if std::env::var("AHMA_NETWORK_TESTS").as_deref() != Ok("1") {
            eprintln!("skipped: set AHMA_NETWORK_TESTS=1 to run the live verification test");
            return;
        }
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init();
        let url = "https://github.com/ahma-labs/ahma/releases/download/v0.19.7/\
                   ahma-release-linux-x86_64.tar.gz";
        let client = reqwest::Client::builder()
            .user_agent("ahma-updater")
            .build()
            .unwrap();
        let bytes = client.get(url).send().await.unwrap().bytes().await.unwrap();

        let dir = tempdir().unwrap();
        let artifact = dir.path().join("ahma-release-linux-x86_64.tar.gz");
        std::fs::write(&artifact, &bytes).unwrap();

        verify_artifact(&artifact)
            .await
            .expect("a published ahma release archive must verify");

        // The `ahma verify --self` shape: the raw binary inside the archive is
        // `subject[1]` of the same one-statement attestation, which `sigstore`
        // 0.14 on its own cannot match.
        let tar = flate2::read::GzDecoder::new(std::io::Cursor::new(&bytes[..]));
        let mut archive = tar::Archive::new(tar);
        let binary = dir.path().join("ahma");
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            if entry.path().unwrap().as_ref() == std::path::Path::new("ahma") {
                entry.unpack(&binary).unwrap();
                break;
            }
        }
        assert!(binary.exists(), "the archive must contain the ahma binary");

        verify_artifact(&binary)
            .await
            .expect("the raw ahma binary must verify as a later subject of the attestation");
    }
}
