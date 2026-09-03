//! `ahma update` orchestration.
//!
//! The release download, Sigstore attestation verification, archive
//! extraction and `cargo install` paths live in the [`ahma_update`] crate
//! (re-exported here wholesale, so `crate::update::…` and
//! `ahma_mcp::update::…` paths are unchanged). What stays in this module is
//! only the part that needs the engine: stopping the running bridge/daemon
//! before the binary is replaced ([`crate::shell::modes::server`]) and the
//! post-install MCP-config drift check ([`crate::setup`]).

pub use ahma_update::*;

use anyhow::{Context, Result};
use std::path::Path;

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
    let platform = platform::detect_platform().ok();

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

async fn maybe_run_setup_wizard(args: &UpdateArgs, binary_path: &Path) -> Result<()> {
    let drifts = crate::setup::detect_mcp_config_drifts();

    if args.dry_run {
        if !drifts.is_empty() {
            println!(
                "[dry-run] Detected {} outdated MCP configuration(s) that would be updated with backups (.bak):",
                drifts.len()
            );
            for drift in &drifts {
                println!(
                    "  - {} ({})",
                    drift.platform_name,
                    drift.config_path.display()
                );
            }
        }
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

    if !drifts.is_empty() && can_prompt_for_setup() {
        println!();
        println!("Detected outdated MCP server configuration(s):");
        for drift in &drifts {
            println!(
                "  - {} ({})",
                drift.platform_name,
                drift.config_path.display()
            );
        }
        if prompt_yes_no(
            "Update outdated MCP configuration(s) and create backups (.bak)? [Y/n]: ",
            true,
        )
        .await?
        {
            for drift in &drifts {
                if let Err(e) = drift.apply_update() {
                    eprintln!(
                        "Warning: failed to update MCP config for {}: {e}",
                        drift.platform_name
                    );
                } else {
                    println!(
                        "✓ Updated and backed up MCP config for {}",
                        drift.platform_name
                    );
                }
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::LazyLock;

    // Serialize env-var-mutating tests so they don't race each other.
    // SAFETY: all env-var writes/removes are performed while holding this lock;
    // nextest runs each test binary in an isolated process, so there is no
    // cross-binary interference.
    static ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

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

    // ENV_MUTEX guard must span the .await so the env var stays set for the whole call.
    // Safe: current-thread tokio test runtime; no re-lock of ENV_MUTEX under the lock.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn test_run_dry_run_without_explicit_install_dir() {
        // args.install_dir = None → resolve_install_dir falls back to the default.
        let _g = ENV_MUTEX.lock();
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
}
