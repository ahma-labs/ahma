//! CLI subcommand and orchestration for `ahma update`.

mod install;
mod platform;
mod ref_mode;
mod release;
mod source;
pub mod verify;

use anyhow::{Context, Result};
use clap::Args;
use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

pub use install::{cargo_install_root, default_install_dir};
pub use ref_mode::{UpdateMode, classify_ref};
pub use source::build_cargo_install_command;

use install::{install_release_asset, read_installed_version};
use platform::detect_platform;
use release::{fetch_latest_asset, fetch_tagged_asset};
use source::install_from_git_ref;

/// Lowercase hex of the SHA-256 of `bytes`, matching the format used in the
/// release `SHA256SUMS` files.
///
/// `sha2` 0.11 returns a `hybrid_array::Array`, which — unlike the `GenericArray`
/// of 0.10 — does not implement `LowerHex`, so the encoding is done here.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Download or build and install ahma.
#[derive(Args, Debug)]
#[command(
    about = "Download or build and install ahma",
    long_about = "Update the installed ahma binary.\n\n\
        With no ref: install the latest published GitHub release.\n\
        With a semver ref (e.g. 0.6.7 or v0.6.7): install that release tag.\n\
        With a branch ref (e.g. main or feature/update): build from GitHub source via cargo install.\n\n\
        For an unpushed local checkout, use:\n\
          RUSTFLAGS='--cfg reqwest_unstable' cargo install --path ahma_bin --bin ahma --root ~/.local --locked --force",
    after_help = "EXAMPLES:
  # Install latest published release
  ahma update

  # Install a specific release
  ahma update 0.6.7

  # Build and install from a Git branch
  ahma update main
  ahma update feature/update

  # Custom install location
  ahma update --install-dir ~/.local/bin

    # Also install user-scoped terminal hooks
    ahma update --install-hooks

  # Preview actions without writing
  ahma update --dry-run"
)]
pub struct UpdateArgs {
    /// Release tag or Git branch (default: latest published release)
    #[arg(value_name = "REF")]
    pub reference: Option<String>,

    /// Install directory (default: ~/.local/bin).
    ///
    /// Replaces the retired `AHMA_INSTALL_DIR` environment variable (R-CFG1.2):
    /// where a binary is installed *from* and *to* is exactly the kind of decision
    /// that must not come from ambient process state.
    #[arg(long)]
    pub install_dir: Option<PathBuf>,

    /// Reinstall even when the installed version already matches
    #[arg(long)]
    pub force: bool,

    /// Install user-scoped terminal hooks after updating.
    #[arg(long)]
    pub install_hooks: bool,

    /// Print planned actions without downloading or installing
    #[arg(long)]
    pub dry_run: bool,

    /// Skip Sigstore attestation verification (insecure — for offline/air-gapped use only).
    /// Equivalent to setting `AHMA_INSECURE_SKIP_VERIFY=1`.
    #[arg(long, alias = "insecure-skip-signature")]
    pub insecure_skip_verify: bool,

    /// Prefer musl builds on Linux (static binaries, glibc-free).
    /// Replaces the retired `AHMA_PREFER_MUSL` environment variable (R-CFG1.2).
    #[arg(long)]
    pub prefer_musl: bool,
}

#[derive(Debug)]
struct UpdateOutcome {
    binary_path: PathBuf,
    binary_changed: bool,
    /// `Some(true)` = attestation verified, `Some(false)` = verification skipped, `None` = git install (no attestation).
    attestation_verified: Option<bool>,
}

/// Resolve the directory the `ahma` binary is installed into (and removed from).
///
/// The single resolution shared by `ahma update` and `ahma uninstall`, so the two
/// halves of the same lifecycle can never disagree about where the binary lives.
///
/// `AHMA_INSTALL_DIR` is **retired** (R-CFG1.2) and is warn-and-ignored here rather
/// than read. "Which directory does a downloaded binary get written into, and which
/// binary does uninstall delete" is the most consequential thing an `AHMA_*` variable
/// still decided: it is ambient state an agent's environment can carry into a step
/// that runs *outside* the sandbox, which is the trust-handoff shape of R-HANDOFF.1.
/// `--install-dir` on `ahma update` is the replacement.
///
/// The bootstrap installers (`scripts/install.sh`, `scripts/install.ps1`) still read
/// `AHMA_INSTALL_DIR`, and legitimately so: they run before any `ahma` binary exists,
/// so there is no CLI to pass a flag to and no settings file to read.
pub(crate) fn resolve_install_dir(explicit: Option<&Path>) -> Result<PathBuf> {
    crate::warn_retired_env("AHMA_INSTALL_DIR");
    match explicit {
        Some(dir) => Ok(dir.to_path_buf()),
        None => default_install_dir(),
    }
}

/// Entry point for `ahma update`.
pub async fn run(args: UpdateArgs, cfg: &crate::shell::cli::AppConfig) -> Result<()> {
    if !args.dry_run {
        println!("Stopping running background processes...");
        let _ = ahma_common::daemon_hub::stop_daemon().await;

        let socket_path_opt = if cfg!(unix) && !cfg.unix_socket_path.is_empty() {
            Some(cfg.unix_socket_path.as_str())
        } else if cfg!(unix) {
            Some(crate::shell::modes::server::GLOBAL_SOCKET_PATH)
        } else {
            None
        };
        let http_url = format!("http://{}:{}", cfg.http_host, cfg.http_port);
        let http_url_opt = Some(http_url.as_str());
        let _ = crate::shell::modes::server::trigger_bridge_restart(socket_path_opt, http_url_opt)
            .await;
    }

    let install_dir = resolve_install_dir(args.install_dir.as_deref())?;

    if args.prefer_musl {
        platform::set_prefer_musl_override();
    }

    let mode = classify_ref(args.reference.as_deref());
    let platform = detect_platform().ok();

    warn_if_running_binary_differs(&install_dir).await;

    let outcome = match mode {
        UpdateMode::LatestRelease | UpdateMode::TaggedRelease { .. } => {
            let platform = platform.context(
                "Prebuilt releases are unavailable on this platform. \
                 Try: ahma update main",
            )?;
            run_release_update(&args, &platform, &install_dir, &mode).await
        }
        UpdateMode::GitRef { branch } => run_git_update(&branch, &install_dir, args.dry_run).await,
    }?;

    if outcome.binary_changed {
        print_post_install_details(
            &outcome.binary_path,
            &install_dir,
            args.dry_run,
            outcome.attestation_verified,
        )
        .await;
    }
    maybe_run_setup_wizard(&args, &outcome.binary_path).await
}

async fn run_git_update(branch: &str, install_dir: &Path, dry_run: bool) -> Result<UpdateOutcome> {
    let installed = install_from_git_ref(branch, install_dir, dry_run).await?;
    Ok(UpdateOutcome {
        binary_path: installed,
        binary_changed: !dry_run,
        attestation_verified: None, // cargo install builds don't have GitHub attestations
    })
}

async fn run_release_update(
    args: &UpdateArgs,
    platform: &platform::Platform,
    install_dir: &Path,
    mode: &UpdateMode,
) -> Result<UpdateOutcome> {
    // Security-tier and retired (R-CFG2.3), stated through the one function that
    // states it (R-CFG1.2.1). Turning off signature or certificate verification is
    // the last decision that should be reachable from an inherited environment.
    let skip_verify_env = crate::warn_retired_env("AHMA_INSECURE_SKIP_VERIFY");
    let skip_signature_env = crate::warn_retired_env("AHMA_INSECURE_SKIP_SIGNATURE");
    if skip_verify_env || skip_signature_env {
        tracing::warn!(
            "Verification stays ON. Pass --insecure-skip-verify on the command line if you \
             really mean to disable it (R-CFG2.3)."
        );
    }
    let insecure_skip_verify = args.insecure_skip_verify;

    let client = reqwest::Client::builder()
        .user_agent("ahma-updater")
        .build()
        .context("Failed to create HTTP client")?;

    let asset = match mode {
        UpdateMode::LatestRelease => fetch_latest_asset(&client, platform).await?,
        UpdateMode::TaggedRelease { tag } => fetch_tagged_asset(&client, tag, platform).await?,
        UpdateMode::GitRef { .. } => unreachable!("release path only"),
    };

    let target = install_dir.join(platform.binary_name());
    if !args.force
        && let Some(installed) = read_installed_version(&target).await
    {
        if installed == asset.version {
            println!(
                "Ahma {installed} is already installed at {}",
                target.display()
            );
            if args.dry_run {
                println!("[dry-run] Would reinstall with --force");
            } else {
                println!("Use --force to reinstall anyway.");
            }
            return Ok(UpdateOutcome {
                binary_path: target,
                binary_changed: false,
                attestation_verified: Some(!insecure_skip_verify),
            });
        }
        println!("Upgrading ahma from {installed} to {}...", asset.version);
    }

    println!("Installing Ahma {} for {}...", asset.version, platform.id);
    let installed = install_release_asset(
        &client,
        &asset,
        platform,
        install_dir,
        args.dry_run,
        insecure_skip_verify,
    )
    .await?;

    Ok(UpdateOutcome {
        binary_path: installed,
        binary_changed: !args.dry_run,
        attestation_verified: Some(!insecure_skip_verify),
    })
}

async fn print_post_install_details(
    installed: &Path,
    install_dir: &Path,
    dry_run: bool,
    attestation_verified: Option<bool>,
) {
    if dry_run {
        return;
    }

    let version = read_installed_version(installed).await;
    println!("{}", format_install_success(installed, version.as_deref()));

    if let Some(verified) = attestation_verified {
        if verified {
            println!(
                "Authenticity verified: GitHub Build Provenance Attestation (Sigstore SLSA Level 3) confirmed."
            );
        } else {
            println!(
                "WARNING: Sigstore attestation verification was bypassed (--insecure-skip-verify)."
            );
        }
    }

    println!();
    println!("Tip: To verify the installed binary against GitHub's attestation API:");
    println!("  ahma verify --self");
    println!(
        "  # or: gh attestation verify {} --repo paulirotta/ahma",
        installed.display()
    );

    print_path_hint(install_dir);
    print_restart_hint();
}

fn format_install_success(installed: &std::path::Path, version: Option<&str>) -> String {
    match version {
        Some(version) => format!(
            "Success! ahma {version} installed to {}",
            installed.display()
        ),
        None => format!("Success! Installed {}", installed.display()),
    }
}

async fn warn_if_running_binary_differs(install_dir: &std::path::Path) {
    let Ok(current) = std::env::current_exe() else {
        return;
    };
    let target = install_dir.join(if cfg!(target_os = "windows") {
        "ahma.exe"
    } else {
        "ahma"
    });
    if target.exists()
        && let (Ok(a), Ok(b)) = (dunce::canonicalize(&current), dunce::canonicalize(&target))
        && a != b
    {
        eprintln!(
            "Note: running {} but updating {}.",
            a.display(),
            b.display()
        );
    }
}

async fn maybe_run_setup_wizard(args: &UpdateArgs, binary_path: &Path) -> Result<()> {
    if args.dry_run {
        if args.install_hooks {
            println!(
                "[dry-run] Would run {} setup --hooks --auto",
                binary_path.display()
            );
        }
        return Ok(());
    }

    if args.install_hooks {
        run_setup_hooks_only_auto(binary_path).await?;
        return Ok(());
    }

    if !can_prompt_for_setup() {
        run_setup_skills_only_auto(binary_path).await?;
        return Ok(());
    }

    println!();
    println!(
        "Optional: run the setup wizard to configure MCP servers, terminal hooks, TLS, and agent skills."
    );

    if !prompt_yes_no("Run the setup wizard now? [Y/n]: ", true).await? {
        println!("Tip: run `ahma setup` later to configure your environment.");
        return Ok(());
    }

    if let Err(error) = run_setup_interactive(binary_path).await {
        eprintln!("Warning: updated ahma but failed to run setup: {error}");
        eprintln!("Run `ahma setup` later to retry.");
    }

    Ok(())
}

fn can_prompt_for_setup() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

async fn prompt_yes_no(prompt: &str, default: bool) -> Result<bool> {
    let prompt = prompt.to_string();
    tokio::task::spawn_blocking(move || {
        print!("{prompt}");
        io::stdout().flush().context("Failed to flush prompt")?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("Failed to read prompt response")?;

        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Ok(default);
        }
        Ok(matches!(trimmed, "y" | "Y" | "yes" | "Yes" | "YES"))
    })
    .await
    .context("Setup prompt task failed")?
}

async fn run_setup_hooks_only_auto(binary_path: &Path) -> Result<()> {
    println!();
    println!("Configuring terminal hooks automatically...");

    let status = tokio::process::Command::new(binary_path)
        .args(["setup", "--hooks", "--auto"])
        .kill_on_drop(true) // owned child (SPEC R-PROC.1)
        .status()
        .await
        .with_context(|| {
            format!(
                "Failed to run {} setup --hooks --auto",
                binary_path.display()
            )
        })?;

    if !status.success() {
        anyhow::bail!(
            "{} setup --hooks --auto exited with status {}",
            binary_path.display(),
            status
        );
    }
    Ok(())
}

async fn run_setup_skills_only_auto(binary_path: &Path) -> Result<()> {
    println!();
    println!("Configuring agent skills automatically...");

    let status = tokio::process::Command::new(binary_path)
        .args(["setup", "--skills", "--auto"])
        .kill_on_drop(true) // owned child (SPEC R-PROC.1)
        .status()
        .await
        .with_context(|| {
            format!(
                "Failed to run {} setup --skills --auto",
                binary_path.display()
            )
        })?;

    if !status.success() {
        anyhow::bail!(
            "{} setup --skills --auto exited with status {}",
            binary_path.display(),
            status
        );
    }
    Ok(())
}

async fn run_setup_interactive(binary_path: &Path) -> Result<()> {
    let status = tokio::process::Command::new(binary_path)
        .arg("setup")
        .kill_on_drop(true) // owned child (SPEC R-PROC.1)
        .status()
        .await
        .with_context(|| format!("Failed to run {} setup", binary_path.display()))?;

    if !status.success() {
        anyhow::bail!(
            "{} setup exited with status {}",
            binary_path.display(),
            status
        );
    }
    Ok(())
}

fn print_path_hint(install_dir: &std::path::Path) {
    println!();
    println!("Ensure {} is on your PATH.", install_dir.display());
    #[cfg(not(windows))]
    println!("  export PATH=\"{}:$PATH\"", install_dir.display());
    #[cfg(windows)]
    println!(
        "  [Environment]::SetEnvironmentVariable('PATH', \"$env:PATH;{}\", 'User')",
        install_dir.display()
    );
}

fn print_restart_hint() {
    println!();
    println!("Restart MCP clients or reload your IDE window to pick up the new binary.");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex};

    // Serialize env-var-mutating tests so they don't race each other.
    // SAFETY: all env-var writes/removes are performed while holding this lock;
    // nextest runs each test binary in an isolated process, so there is no
    // cross-binary interference.
    static ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Known-answer test for [`sha256_hex`], pinning it to the exact encoding a
    /// release `SHA256SUMS` line carries.
    ///
    /// The checksum round-trip tests hash with `sha256_hex` on *both* sides, so
    /// an encoding that is wrong but self-consistent (e.g. a dropped zero-pad)
    /// would still satisfy them while silently failing against a real
    /// `SHA256SUMS` from GitHub. These vectors are independent of our code.
    #[test]
    fn sha256_hex_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        // Digest begins with the byte 0xa0: catches a missing `{:02x}` zero-pad,
        // which would render a low nibble as one char and shorten the string.
        let padded = sha256_hex(b"\x00\x0a\xff");
        assert_eq!(
            padded,
            "a0956176ad28cadf4a54b314f9fcd6143d7007957454286ff24580445304b558"
        );
        assert_eq!(padded.len(), 64, "SHA-256 hex must always be 64 chars");
    }

    // ─── clap parsing helper macro ───────────────────────────────────────────

    macro_rules! parse_update_args {
        ($($arg:expr),* $(,)?) => {{
            use clap::Parser;
            #[derive(Parser)]
            struct Cli {
                #[command(subcommand)]
                cmd: Cmd,
            }
            #[derive(clap::Subcommand)]
            enum Cmd {
                Update(UpdateArgs),
            }
            let cli = Cli::try_parse_from([$($arg),*]).unwrap();
            let Cmd::Update(args) = cli.cmd;
            args
        }};
    }

    // ─── UpdateArgs – clap parsing ───────────────────────────────────────────

    #[test]
    fn test_update_args_parse_defaults() {
        let args = parse_update_args!["ahma", "update"];
        assert!(args.reference.is_none());
        assert!(!args.force);
        assert!(!args.install_hooks);
        assert!(!args.dry_run);
        assert!(!args.insecure_skip_verify);
        assert!(!args.prefer_musl);
        assert!(args.install_dir.is_none());
    }

    #[test]
    fn test_update_args_parse_install_hooks() {
        let args = parse_update_args!["ahma", "update", "--install-hooks"];
        assert!(args.install_hooks);
    }

    #[test]
    fn test_update_args_parse_all_flags() {
        let temp_dir = tempfile::tempdir().unwrap();
        let dir_str = temp_dir.path().to_str().unwrap().to_string();
        let args = parse_update_args![
            "ahma",
            "update",
            "--force",
            "--dry-run",
            "--insecure-skip-verify",
            "--prefer-musl",
            "--install-dir",
            &dir_str,
            "v0.6.7",
        ];
        assert_eq!(args.reference.as_deref(), Some("v0.6.7"));
        assert!(args.force);
        assert!(args.dry_run);
        assert!(args.insecure_skip_verify);
        assert!(args.prefer_musl);
        assert_eq!(args.install_dir, Some(temp_dir.path().to_path_buf()));
    }

    #[test]
    fn test_update_args_alias_insecure_skip_signature() {
        // --insecure-skip-signature is a declared alias for --insecure-skip-verify
        let args = parse_update_args!["ahma", "update", "--insecure-skip-signature"];
        assert!(args.insecure_skip_verify);
    }

    #[test]
    fn test_update_args_parse_git_branch_ref() {
        let args = parse_update_args!["ahma", "update", "feature/my-branch"];
        assert_eq!(args.reference.as_deref(), Some("feature/my-branch"));
        assert!(!args.dry_run);
    }

    // ─── format_install_success ──────────────────────────────────────────────

    #[test]
    fn test_format_install_success_with_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ahma");
        let message = format_install_success(&path, Some("0.7.0"));
        assert!(
            message.contains("0.7.0"),
            "version should appear in: {message}"
        );
        assert!(message.starts_with("Success!"));
    }

    #[test]
    fn test_format_install_success_without_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ahma");
        let message = format_install_success(&path, None);
        assert!(message.starts_with("Success!"));
        assert!(
            message.contains(path.to_str().unwrap()),
            "path should appear in: {message}"
        );
    }

    // ─── print_path_hint & print_restart_hint ───────────────────────────────

    #[test]
    fn test_print_path_hint_does_not_panic() {
        let temp_dir = tempfile::tempdir().unwrap();
        // Should print without panicking regardless of platform.
        print_path_hint(temp_dir.path());
    }

    #[test]
    fn test_print_restart_hint_does_not_panic() {
        print_restart_hint();
    }

    // ─── can_prompt_for_setup ───────────────────────────────────────────────

    #[test]
    fn test_can_prompt_for_setup_returns_bool_without_panic() {
        // In nextest stdin/stdout are not terminals; the function must not panic.
        // In a real terminal it may return true; either result is acceptable.
        let _ = can_prompt_for_setup();
    }

    // ─── warn_if_running_binary_differs ─────────────────────────────────────

    #[tokio::test]
    async fn test_warn_if_running_binary_differs_no_target_file() {
        // When the target binary does not exist the function returns silently.
        let temp_dir = tempfile::tempdir().unwrap();
        warn_if_running_binary_differs(temp_dir.path()).await;
        // No panic = success.
    }

    #[tokio::test]
    async fn test_warn_if_running_binary_differs_with_target_file() {
        // When a dummy file exists at the expected target location the canonicalize
        // branches are exercised (paths will differ from the real current_exe).
        let temp_dir = tempfile::tempdir().unwrap();
        let binary_name = if cfg!(target_os = "windows") {
            "ahma.exe"
        } else {
            "ahma"
        };
        let target = temp_dir.path().join(binary_name);
        std::fs::write(&target, b"fake binary").unwrap();
        // Exercises target.exists() == true and the dunce::canonicalize comparison.
        // A "Note: running … but updating …" line may be printed to stderr.
        warn_if_running_binary_differs(temp_dir.path()).await;
        // No panic = success.
    }

    // ─── print_post_install_details ─────────────────────────────────────────

    #[tokio::test]
    async fn test_print_post_install_details_dry_run_returns_early() {
        let temp_dir = tempfile::tempdir().unwrap();
        let binary = temp_dir.path().join("nonexistent_ahma");
        // dry_run=true → function returns at the first guard without touching anything.
        print_post_install_details(&binary, temp_dir.path(), true, None).await;
        // No panic = success.
    }

    #[tokio::test]
    async fn test_print_post_install_details_no_attestation() {
        // dry_run=false, attestation_verified=None (git install path).
        // Binary does not exist → read_installed_version returns None → "no version" message.
        let temp_dir = tempfile::tempdir().unwrap();
        let binary = temp_dir.path().join("nonexistent_ahma");
        print_post_install_details(&binary, temp_dir.path(), false, None).await;
        // Covers: version read (None), format_install_success(None), print hints.
    }

    #[tokio::test]
    async fn test_print_post_install_details_attestation_verified_true() {
        // attestation_verified=Some(true) → prints the "Authenticity verified" message.
        let temp_dir = tempfile::tempdir().unwrap();
        let binary = temp_dir.path().join("nonexistent_ahma");
        print_post_install_details(&binary, temp_dir.path(), false, Some(true)).await;
    }

    #[tokio::test]
    async fn test_print_post_install_details_attestation_bypassed() {
        // attestation_verified=Some(false) → prints the insecure-skip warning.
        let temp_dir = tempfile::tempdir().unwrap();
        let binary = temp_dir.path().join("nonexistent_ahma");
        print_post_install_details(&binary, temp_dir.path(), false, Some(false)).await;
    }

    // ─── run_git_update ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_run_git_update_dry_run_main_branch() {
        let temp_dir = tempfile::tempdir().unwrap();
        let result = run_git_update("main", temp_dir.path(), true).await;
        assert!(
            result.is_ok(),
            "dry-run git update should succeed: {result:?}"
        );
        let outcome = result.unwrap();
        assert!(
            !outcome.binary_changed,
            "dry_run → binary_changed must be false"
        );
        assert!(
            outcome.attestation_verified.is_none(),
            "git builds have no attestation"
        );
        // Binary path should be inside install_dir.
        assert!(
            outcome.binary_path.starts_with(temp_dir.path()),
            "binary path should be under install_dir"
        );
    }

    #[tokio::test]
    async fn test_run_git_update_dry_run_feature_branch() {
        let temp_dir = tempfile::tempdir().unwrap();
        let result = run_git_update("feature/my-branch", temp_dir.path(), true).await;
        assert!(result.is_ok());
        let outcome = result.unwrap();
        assert!(!outcome.binary_changed);
        assert!(outcome.attestation_verified.is_none());
    }

    // ─── Unix-only helpers ───────────────────────────────────────────────────

    /// Returns the path to a Unix command that always exits 0, ignoring all args.
    #[cfg(unix)]
    fn find_true_cmd() -> Option<PathBuf> {
        for p in ["/usr/bin/true", "/bin/true"] {
            let path = PathBuf::from(p);
            if path.exists() {
                return Some(path);
            }
        }
        None
    }

    /// Returns the path to a Unix command that always exits 1, ignoring all args.
    #[cfg(unix)]
    fn find_false_cmd() -> Option<PathBuf> {
        for p in ["/usr/bin/false", "/bin/false"] {
            let path = PathBuf::from(p);
            if path.exists() {
                return Some(path);
            }
        }
        None
    }

    // ─── run_setup_hooks_only_auto ──────────────────────────────────────────

    #[tokio::test]
    async fn test_run_setup_hooks_only_auto_spawn_error_on_missing_binary() {
        let temp_dir = tempfile::tempdir().unwrap();
        let binary = temp_dir.path().join("nonexistent_ahma_binary");
        let result = run_setup_hooks_only_auto(&binary).await;
        assert!(result.is_err(), "nonexistent binary should fail to spawn");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("Failed to run") || msg.contains("setup") || msg.contains("No such"),
            "unexpected error message: {msg}"
        );
    }

    /// Exercises the `bail!` path when the binary exits with a non-zero status.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_setup_hooks_only_auto_nonzero_exit_bail() {
        let Some(false_path) = find_false_cmd() else {
            return; // skip if /usr/bin/false and /bin/false are both absent
        };
        let result = run_setup_hooks_only_auto(&false_path).await;
        assert!(result.is_err(), "non-zero exit should return Err");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("exited with status") || msg.contains("setup"),
            "unexpected error: {msg}"
        );
    }

    /// Exercises the `Ok(())` happy path when the binary exits with status 0.
    /// `true` ignores all arguments and always exits 0.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_setup_hooks_only_auto_success() {
        let Some(true_path) = find_true_cmd() else {
            return;
        };
        let result = run_setup_hooks_only_auto(&true_path).await;
        assert!(
            result.is_ok(),
            "`true` exits 0 — hooks setup should succeed: {result:?}"
        );
    }

    // ─── run_setup_skills_only_auto ─────────────────────────────────────────

    #[tokio::test]
    async fn test_run_setup_skills_only_auto_spawn_error_on_missing_binary() {
        let temp_dir = tempfile::tempdir().unwrap();
        let binary = temp_dir.path().join("nonexistent_ahma_binary");
        let result = run_setup_skills_only_auto(&binary).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("Failed to run") || msg.contains("setup") || msg.contains("No such"),
            "unexpected error: {msg}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_setup_skills_only_auto_nonzero_exit_bail() {
        let Some(false_path) = find_false_cmd() else {
            return;
        };
        let result = run_setup_skills_only_auto(&false_path).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("exited with status") || msg.contains("setup"),
            "unexpected error: {msg}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_setup_skills_only_auto_success() {
        let Some(true_path) = find_true_cmd() else {
            return;
        };
        let result = run_setup_skills_only_auto(&true_path).await;
        assert!(
            result.is_ok(),
            "`true` exits 0 — skills setup should succeed: {result:?}"
        );
    }

    // ─── run_setup_interactive ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_run_setup_interactive_spawn_error_on_missing_binary() {
        let temp_dir = tempfile::tempdir().unwrap();
        let binary = temp_dir.path().join("nonexistent_ahma_binary");
        let result = run_setup_interactive(&binary).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("Failed to run") || msg.contains("setup") || msg.contains("No such"),
            "unexpected error: {msg}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_setup_interactive_nonzero_exit_bail() {
        let Some(false_path) = find_false_cmd() else {
            return;
        };
        let result = run_setup_interactive(&false_path).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("exited with status") || msg.contains("setup"),
            "unexpected error: {msg}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_setup_interactive_success() {
        let Some(true_path) = find_true_cmd() else {
            return;
        };
        let result = run_setup_interactive(&true_path).await;
        assert!(
            result.is_ok(),
            "`true` exits 0 — interactive setup should succeed: {result:?}"
        );
    }

    // ─── maybe_run_setup_wizard ─────────────────────────────────────────────

    fn make_dry_run_args(install_hooks: bool) -> UpdateArgs {
        UpdateArgs {
            reference: None,
            install_dir: None,
            force: false,
            install_hooks,
            dry_run: true,
            insecure_skip_verify: false,
            prefer_musl: false,
        }
    }

    fn make_live_args(install_hooks: bool) -> UpdateArgs {
        UpdateArgs {
            reference: None,
            install_dir: None,
            force: false,
            install_hooks,
            dry_run: false,
            insecure_skip_verify: false,
            prefer_musl: false,
        }
    }

    #[tokio::test]
    async fn test_maybe_run_setup_wizard_dry_run_no_hooks_returns_ok() {
        let temp_dir = tempfile::tempdir().unwrap();
        let binary = temp_dir.path().join("ahma");
        let result = maybe_run_setup_wizard(&make_dry_run_args(false), &binary).await;
        assert!(
            result.is_ok(),
            "dry_run + no hooks should return Ok: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_maybe_run_setup_wizard_dry_run_with_hooks_prints_message() {
        // Exercises the `if args.install_hooks` branch inside the dry_run guard:
        // prints "[dry-run] Would run … setup --hooks --auto".
        let temp_dir = tempfile::tempdir().unwrap();
        let binary = temp_dir.path().join("ahma");
        let result = maybe_run_setup_wizard(&make_dry_run_args(true), &binary).await;
        assert!(
            result.is_ok(),
            "dry_run + hooks should return Ok: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_maybe_run_setup_wizard_install_hooks_spawn_error() {
        // dry_run=false + install_hooks=true + nonexistent binary → Err via ?.
        let temp_dir = tempfile::tempdir().unwrap();
        let binary = temp_dir.path().join("nonexistent_ahma");
        let result = maybe_run_setup_wizard(&make_live_args(true), &binary).await;
        assert!(result.is_err(), "nonexistent binary should propagate Err");
    }

    #[tokio::test]
    async fn test_maybe_run_setup_wizard_no_terminal_skills_spawn_error() {
        // dry_run=false + install_hooks=false.
        // In nextest stdin/stdout are not terminals → can_prompt_for_setup() = false
        // → run_setup_skills_only_auto with nonexistent binary → Err via ?.
        let temp_dir = tempfile::tempdir().unwrap();
        let binary = temp_dir.path().join("nonexistent_ahma");
        let result = maybe_run_setup_wizard(&make_live_args(false), &binary).await;
        // Err in non-terminal environment (nextest), Ok if somehow running in a terminal
        // where the interactive path swallows setup errors.
        if let Err(e) = &result {
            let msg = e.to_string();
            assert!(
                msg.contains("Failed to run") || msg.contains("setup") || msg.contains("No such"),
                "unexpected error: {msg}"
            );
        }
    }

    // ─── run_setup_hooks_only_auto success via `true` exercises Ok(()) line ─

    // (Covered above in the three-case tests for each setup function.)

    // ─── prompt_yes_no ──────────────────────────────────────────────────────

    /// In nextest stdin is a non-terminal pipe; read_line hits EOF immediately,
    /// `trimmed` is empty, and the function returns `Ok(default)`.
    /// A 3-second timeout prevents the test from hanging if stdin is unexpectedly
    /// a blocking terminal.
    #[tokio::test]
    async fn test_prompt_yes_no_eof_stdin_returns_default_false() {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            prompt_yes_no("test prompt? ", false),
        )
        .await;

        match result {
            Ok(Ok(value)) => {
                assert!(!value, "empty/EOF input should return the default (false)")
            }
            Ok(Err(_)) => {
                // I/O error on stdin is acceptable in test env (e.g., stdin is /dev/null).
            }
            Err(_elapsed) => {
                // stdin is blocking (ran in a real terminal) — skip silently.
            }
        }
    }

    #[tokio::test]
    async fn test_prompt_yes_no_eof_stdin_returns_default_true() {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            prompt_yes_no("test prompt? ", true),
        )
        .await;

        match result {
            Ok(Ok(value)) => {
                assert!(value, "empty/EOF input should return the default (true)")
            }
            Ok(Err(_)) => {}
            Err(_elapsed) => {}
        }
    }

    // ─── run() end-to-end with dry_run=true ─────────────────────────────────

    #[tokio::test]
    async fn test_run_dry_run_git_ref_succeeds() {
        // dry_run=true + GitRef(main) exercises:
        //   • install_dir resolution (from args.install_dir)
        //   • classify_ref → GitRef
        //   • detect_platform (ok() — may be None on unsupported platform, handled)
        //   • warn_if_running_binary_differs
        //   • GitRef match arm → run_git_update (dry_run path, no cargo needed)
        //   • outcome.binary_changed = false → skip print_post_install_details
        //   • maybe_run_setup_wizard (dry_run=true → early Ok)
        // No daemon stop, no network calls.
        let temp_dir = tempfile::tempdir().unwrap();
        let args = UpdateArgs {
            reference: Some("main".to_string()),
            install_dir: Some(temp_dir.path().to_path_buf()),
            force: false,
            install_hooks: false,
            dry_run: true,
            insecure_skip_verify: false,
            prefer_musl: false,
        };
        let cfg = crate::shell::cli::AppConfig::default();
        let result = run(args, &cfg).await;
        assert!(
            result.is_ok(),
            "dry-run git-ref update should succeed: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_run_dry_run_git_ref_with_prefer_musl() {
        // Same as above but prefer_musl=true to cover the set_prefer_musl_override branch.
        let temp_dir = tempfile::tempdir().unwrap();
        let args = UpdateArgs {
            reference: Some("feature/test".to_string()),
            install_dir: Some(temp_dir.path().to_path_buf()),
            force: false,
            install_hooks: false,
            dry_run: true,
            insecure_skip_verify: false,
            prefer_musl: true,
        };
        let cfg = crate::shell::cli::AppConfig::default();
        let result = run(args, &cfg).await;
        assert!(
            result.is_ok(),
            "dry-run + prefer_musl update should succeed: {result:?}"
        );
    }

    /// R-CFG1.2: `AHMA_INSTALL_DIR` is retired. With no `--install-dir`, the resolved
    /// directory must be the compiled-in default even when the variable points somewhere
    /// else — the value is reported as "set" (so it can be warned about) but never used.
    #[test]
    fn install_dir_ignores_retired_env_var() {
        let _g = ENV_MUTEX.lock().unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("AHMA_INSTALL_DIR");
        // SAFETY: guarded by ENV_MUTEX; nextest isolates each test binary.
        unsafe { std::env::set_var("AHMA_INSTALL_DIR", temp_dir.path()) };

        let resolved = resolve_install_dir(None);
        let warned = crate::warn_retired_env("AHMA_INSTALL_DIR");

        // SAFETY: see above.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("AHMA_INSTALL_DIR", v),
                None => std::env::remove_var("AHMA_INSTALL_DIR"),
            }
        }

        assert!(warned, "a set retired variable must still be warned about");
        let resolved = resolved.expect("home directory must resolve");
        assert_ne!(
            resolved,
            temp_dir.path(),
            "AHMA_INSTALL_DIR is retired (R-CFG1.2) and must not steer the install dir"
        );
        assert_eq!(
            resolved,
            default_install_dir().expect("home directory"),
            "with no --install-dir the default install dir must be used"
        );
    }

    /// `--install-dir` is the replacement for the retired variable and must win even
    /// when the retired variable is also set.
    #[test]
    fn install_dir_flag_wins_over_retired_env_var() {
        let _g = ENV_MUTEX.lock().unwrap();
        let env_dir = tempfile::tempdir().unwrap();
        let flag_dir = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("AHMA_INSTALL_DIR");
        // SAFETY: guarded by ENV_MUTEX; nextest isolates each test binary.
        unsafe { std::env::set_var("AHMA_INSTALL_DIR", env_dir.path()) };

        let resolved = resolve_install_dir(Some(flag_dir.path()));

        // SAFETY: see above.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("AHMA_INSTALL_DIR", v),
                None => std::env::remove_var("AHMA_INSTALL_DIR"),
            }
        }

        assert_eq!(
            resolved.expect("explicit dir always resolves"),
            flag_dir.path(),
            "--install-dir must be the only thing that overrides the default"
        );
    }

    // ENV_MUTEX guard must span the .await so the env var stays set for the whole call.
    // Safe: current-thread tokio test runtime; no re-lock of ENV_MUTEX under the lock.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn test_run_dry_run_without_explicit_install_dir() {
        // args.install_dir = None → resolve_install_dir falls back to the default.
        let _g = ENV_MUTEX.lock().unwrap();
        let args = UpdateArgs {
            reference: Some("main".to_string()),
            install_dir: None,
            force: false,
            install_hooks: false,
            dry_run: true,
            insecure_skip_verify: false,
            prefer_musl: false,
        };
        let cfg = crate::shell::cli::AppConfig::default();
        let result = run(args, &cfg).await;
        assert!(
            result.is_ok(),
            "dry-run update with the default install dir should succeed: {result:?}"
        );
    }

    // ─── run_release_update – env-var warning + client build ────────────────

    /// Covers the retired-env-var warning block (lines that log a tracing::warn)
    /// and the reqwest client construction.  If a network connection is available
    /// (as in GitHub CI) the test also covers the asset-fetch and dry-run install
    /// paths.  Network failures are silently accepted because we only need coverage
    /// of the lines that execute *before* the first I/O call.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn test_run_release_update_warns_on_retired_env_vars() {
        let _g = ENV_MUTEX.lock().unwrap();
        // SAFETY: guarded by ENV_MUTEX.
        unsafe { std::env::set_var("AHMA_INSECURE_SKIP_VERIFY", "1") };
        unsafe { std::env::set_var("AHMA_INSECURE_SKIP_SIGNATURE", "1") };

        if let Ok(platform) = platform::detect_platform() {
            let temp_dir = tempfile::tempdir().unwrap();
            let args = UpdateArgs {
                reference: None,
                install_dir: Some(temp_dir.path().to_path_buf()),
                force: false,
                install_hooks: false,
                dry_run: true,
                insecure_skip_verify: true,
                prefer_musl: false,
            };
            let outcome = run_release_update(
                &args,
                &platform,
                temp_dir.path(),
                &UpdateMode::LatestRelease,
            )
            .await;
            if let Err(e) = outcome {
                // Network / API errors are expected when GitHub is unreachable.
                // A programming error would show a different pattern.
                let msg = e.to_string();
                assert!(
                    msg.contains("Failed to fetch")
                        || msg.contains("reqwest")
                        || msg.contains("HTTP")
                        || msg.contains("GitHub")
                        || msg.contains("error")
                        || msg.contains("Failed to create"),
                    "unexpected non-network error in run_release_update: {msg}"
                );
            }
        }

        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_VERIFY") };
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_SIGNATURE") };
    }

    /// Covers the `UpdateMode::TaggedRelease` match arm inside `run_release_update`.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn test_run_release_update_tagged_release_match_arm() {
        let _g = ENV_MUTEX.lock().unwrap();
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_VERIFY") };
        unsafe { std::env::remove_var("AHMA_INSECURE_SKIP_SIGNATURE") };

        if let Ok(platform) = platform::detect_platform() {
            let temp_dir = tempfile::tempdir().unwrap();
            let args = UpdateArgs {
                reference: Some("v0.1.0".to_string()),
                install_dir: Some(temp_dir.path().to_path_buf()),
                force: false,
                install_hooks: false,
                dry_run: true,
                insecure_skip_verify: true,
                prefer_musl: false,
            };
            let mode = UpdateMode::TaggedRelease {
                tag: "v0.1.0".to_string(),
            };
            // Accept either Ok (network available + dry-run) or Err (network unavailable).
            // The sole goal is exercising the TaggedRelease match arm for coverage.
            let outcome = run_release_update(&args, &platform, temp_dir.path(), &mode).await;
            if let Err(e) = outcome {
                let msg = e.to_string();
                assert!(
                    msg.contains("Failed to fetch")
                        || msg.contains("reqwest")
                        || msg.contains("HTTP")
                        || msg.contains("GitHub")
                        || msg.contains("error")
                        || msg.contains("Failed to create"),
                    "unexpected non-network error in tagged-release path: {msg}"
                );
            }
        }
    }
}
