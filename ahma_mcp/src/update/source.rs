//! Build and install ahma from a Git ref via `cargo install`.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use super::install::cargo_install_root;

pub const GIT_REPO_URL: &str = "https://github.com/paulirotta/ahma";

/// Build the `cargo install` command for a Git branch/ref.
pub fn build_cargo_install_command(branch: &str, install_dir: &Path) -> Vec<String> {
    let root = cargo_install_root(install_dir);
    // The `ahma` binary lives in the `ahma_bin` crate (moved from `ahma_mcp` in 0.7.0).
    vec![
        "cargo".to_string(),
        "install".to_string(),
        "--git".to_string(),
        GIT_REPO_URL.to_string(),
        "--branch".to_string(),
        branch.to_string(),
        "ahma_bin".to_string(),
        "--bin".to_string(),
        "ahma".to_string(),
        "--root".to_string(),
        root.display().to_string(),
        "--locked".to_string(),
        "--force".to_string(),
    ]
}

/// Compute the RUSTFLAGS value needed to build ahma from source.
///
/// The workspace uses `reqwest` with the `http3` feature, which requires
/// `RUSTFLAGS='--cfg reqwest_unstable'` to compile.  We append to any
/// existing flags the caller already has set so we don't clobber user config.
pub fn required_rustflags() -> String {
    let existing = std::env::var("RUSTFLAGS").unwrap_or_default();
    let flag = "--cfg reqwest_unstable";
    if existing.contains(flag) {
        // Already present — return as-is
        existing
    } else if existing.is_empty() {
        flag.to_string()
    } else {
        format!("{existing} {flag}")
    }
}

/// Install from a Git branch using Cargo.
pub async fn install_from_git_ref(branch: &str, install_dir: &Path, dry_run: bool) -> Result<()> {
    let args = build_cargo_install_command(branch, install_dir);
    let display = args.join(" ");

    let rustflags = required_rustflags();

    if dry_run {
        println!("[dry-run] Would run: RUSTFLAGS='{rustflags}' {display}");
        return Ok(());
    }

    if which_cargo().is_none() {
        bail!(
            "cargo is not installed or not on PATH. \
             Install Rust from https://rustup.rs/ then retry."
        );
    }

    println!("Building ahma from Git branch '{branch}' (this may take several minutes)...");
    println!("  RUSTFLAGS='{rustflags}' {display}");

    let status = tokio::process::Command::new(&args[0])
        .args(&args[1..])
        .env("RUSTFLAGS", &rustflags)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("Failed to spawn cargo install")?;

    if !status.success() {
        bail!("cargo install failed with status {status}");
    }

    let binary = install_dir.join(if cfg!(target_os = "windows") {
        "ahma.exe"
    } else {
        "ahma"
    });

    if !binary.exists() {
        bail!(
            "cargo install completed but binary not found at {}",
            binary.display()
        );
    }

    println!("Installed {}", binary.display());
    Ok(())
}

fn which_cargo() -> Option<PathBuf> {
    which_command("cargo")
}

fn which_command(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(name);
            if candidate.is_file() {
                Some(candidate)
            } else {
                #[cfg(windows)]
                {
                    let with_exe = dir.join(format!("{name}.exe"));
                    if with_exe.is_file() {
                        return Some(with_exe);
                    }
                }
                None
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_cargo_install_command() {
        let cmd = build_cargo_install_command("feature/update", Path::new("/home/u/.local/bin"));
        assert_eq!(cmd[0], "cargo");
        assert!(cmd.contains(&"--branch".to_string()));
        assert!(cmd.contains(&"feature/update".to_string()));
        assert!(cmd.contains(&"ahma_bin".to_string()), "package should be ahma_bin, not ahma_mcp");
        assert!(!cmd.contains(&"ahma_mcp".to_string()), "ahma_mcp no longer has the ahma bin target");
        assert!(cmd.contains(&"--root".to_string()));
        assert!(cmd.contains(&"/home/u/.local".to_string()));
    }

    #[test]
    fn test_required_rustflags_sets_reqwest_unstable() {
        // With no pre-existing RUSTFLAGS the flag should be set.
        // SAFETY: test-only; single-threaded by nextest process isolation.
        unsafe { std::env::remove_var("RUSTFLAGS") };
        let flags = required_rustflags();
        assert!(flags.contains("--cfg reqwest_unstable"), "expected reqwest_unstable in '{flags}'");
    }

    #[test]
    fn test_required_rustflags_appends_to_existing() {
        // SAFETY: test-only; single-threaded by nextest process isolation.
        unsafe { std::env::set_var("RUSTFLAGS", "-C opt-level=2") };
        let flags = required_rustflags();
        assert!(flags.contains("-C opt-level=2"), "existing flag should be preserved");
        assert!(flags.contains("--cfg reqwest_unstable"), "reqwest flag should be appended");
        unsafe { std::env::remove_var("RUSTFLAGS") };
    }

    #[test]
    fn test_required_rustflags_no_duplicate() {
        // SAFETY: test-only; single-threaded by nextest process isolation.
        unsafe { std::env::set_var("RUSTFLAGS", "--cfg reqwest_unstable") };
        let flags = required_rustflags();
        // Should not double-add the flag.
        let count = flags.matches("reqwest_unstable").count();
        assert_eq!(count, 1, "flag should appear exactly once, got: '{flags}'");
        unsafe { std::env::remove_var("RUSTFLAGS") };
    }
}
