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
pub async fn install_from_git_ref(
    branch: &str,
    install_dir: &Path,
    dry_run: bool,
) -> Result<PathBuf> {
    let args = build_cargo_install_command(branch, install_dir);
    let display = args.join(" ");

    let rustflags = required_rustflags();

    if dry_run {
        println!("[dry-run] Would run: RUSTFLAGS='{rustflags}' {display}");
        return Ok(install_dir.join(super::AHMA_BINARY_NAME));
    }

    if which_cargo().is_none() {
        bail!(
            "cargo is not installed or not on PATH. \
             Install Rust from https://rustup.rs/ then retry."
        );
    }

    println!("Building ahma from Git branch '{branch}' (this may take several minutes)...");
    println!("  RUSTFLAGS='{rustflags}' {display}");

    // Owned child (SPEC R-PROC.1). `status()` spawns internally, so a Ctrl-C that
    // drops this future would otherwise leave a multi-minute `cargo build`
    // running with nothing left to stop it.
    let status = tokio::process::Command::new(&args[0])
        .args(&args[1..])
        .env("RUSTFLAGS", &rustflags)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .status()
        .await
        .context("Failed to spawn cargo install")?;

    if !status.success() {
        bail!("cargo install failed with status {status}");
    }

    let binary = install_dir.join(super::AHMA_BINARY_NAME);

    if !binary.exists() {
        bail!(
            "cargo install completed but binary not found at {}",
            binary.display()
        );
    }

    println!("Installed {}", binary.display());
    Ok(binary)
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
    use parking_lot::Mutex;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::LazyLock;
    use tempfile::tempdir;

    static ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[test]
    fn test_build_cargo_install_command() {
        let cmd = build_cargo_install_command("feature/update", Path::new("/home/u/.local/bin"));
        assert_eq!(cmd[0], "cargo");
        assert!(cmd.contains(&"--branch".to_string()));
        assert!(cmd.contains(&"feature/update".to_string()));
        assert!(
            cmd.contains(&"ahma_bin".to_string()),
            "package should be ahma_bin, not ahma_mcp"
        );
        assert!(
            !cmd.contains(&"ahma_mcp".to_string()),
            "ahma_mcp no longer has the ahma bin target"
        );
        assert!(cmd.contains(&"--root".to_string()));
        assert!(cmd.contains(&"/home/u/.local".to_string()));
    }

    #[test]
    fn test_required_rustflags_sets_reqwest_unstable() {
        let _guard = ENV_MUTEX.lock();
        // With no pre-existing RUSTFLAGS the flag should be set.
        // SAFETY: test-only; single-threaded by nextest process isolation.
        unsafe { std::env::remove_var("RUSTFLAGS") };
        let flags = required_rustflags();
        assert!(
            flags.contains("--cfg reqwest_unstable"),
            "expected reqwest_unstable in '{flags}'"
        );
    }

    #[test]
    fn test_required_rustflags_appends_to_existing() {
        let _guard = ENV_MUTEX.lock();
        // SAFETY: test-only; single-threaded by nextest process isolation.
        unsafe { std::env::set_var("RUSTFLAGS", "-C opt-level=2") };
        let flags = required_rustflags();
        assert!(
            flags.contains("-C opt-level=2"),
            "existing flag should be preserved"
        );
        assert!(
            flags.contains("--cfg reqwest_unstable"),
            "reqwest flag should be appended"
        );
        unsafe { std::env::remove_var("RUSTFLAGS") };
    }

    #[test]
    fn test_required_rustflags_no_duplicate() {
        let _guard = ENV_MUTEX.lock();
        // SAFETY: test-only; single-threaded by nextest process isolation.
        unsafe { std::env::set_var("RUSTFLAGS", "--cfg reqwest_unstable") };
        let flags = required_rustflags();
        // Should not double-add the flag.
        let count = flags.matches("reqwest_unstable").count();
        assert_eq!(count, 1, "flag should appear exactly once, got: '{flags}'");
        unsafe { std::env::remove_var("RUSTFLAGS") };
    }

    #[tokio::test]
    async fn test_install_from_git_ref_dry_run_returns_binary_path() {
        let dir = tempdir().unwrap();
        let install_dir = dir.path();
        let result = install_from_git_ref("main", install_dir, true).await;
        assert!(
            result.is_ok(),
            "dry-run should succeed without spawning cargo"
        );
        let path = result.unwrap();
        let expected_name = crate::AHMA_BINARY_NAME;
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some(expected_name),
            "dry-run path should end in the platform binary name, got {}",
            path.display()
        );
        assert!(
            path.starts_with(install_dir),
            "binary path {} should be under install_dir {}",
            path.display(),
            install_dir.display()
        );
        assert_eq!(path, install_dir.join(expected_name));
    }

    #[test]
    fn test_which_command_finds_file_on_path() {
        let _guard = ENV_MUTEX.lock();
        let saved_path = std::env::var_os("PATH");

        let dir = tempdir().unwrap();
        // The code checks `is_file()` on `dir.join(name)`; on Windows it also
        // checks `{name}.exe`. Create both so the lookup is deterministic
        // regardless of platform.
        let bin = dir.path().join("fakebin");
        fs::write(&bin, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            let mut perms = fs::metadata(&bin).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&bin, perms).unwrap();
        }
        #[cfg(windows)]
        {
            fs::write(dir.path().join("fakebin.exe"), b"").unwrap();
        }

        // SAFETY: test-only; ENV_MUTEX serializes env access in this module.
        unsafe { std::env::set_var("PATH", dir.path()) };

        let found = which_command("fakebin");
        assert!(found.is_some(), "fakebin should be found on PATH");

        let missing = which_command("definitely_not_a_real_binary_xyz");
        assert!(missing.is_none(), "nonexistent binary should not be found");

        // Restore PATH for other tests.
        // SAFETY: test-only; ENV_MUTEX held.
        unsafe {
            match saved_path {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    #[test]
    fn test_which_command_none_when_path_empty() {
        let _guard = ENV_MUTEX.lock();
        let saved_path = std::env::var_os("PATH");

        let dir = tempdir().unwrap(); // empty dir, no matching file
        // SAFETY: test-only; ENV_MUTEX held.
        unsafe { std::env::set_var("PATH", dir.path()) };

        assert!(
            which_command("fakebin").is_none(),
            "no binary present in PATH dir, expected None"
        );

        // SAFETY: test-only; ENV_MUTEX held.
        unsafe {
            match saved_path {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    #[test]
    fn test_which_cargo_uses_which_command() {
        let _guard = ENV_MUTEX.lock();
        let saved_path = std::env::var_os("PATH");

        let dir = tempdir().unwrap();
        let cargo_name = if cfg!(target_os = "windows") {
            "cargo.exe"
        } else {
            "cargo"
        };
        let bin = dir.path().join(cargo_name);
        fs::write(&bin, b"").unwrap();
        #[cfg(unix)]
        {
            let mut perms = fs::metadata(&bin).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&bin, perms).unwrap();
        }

        // SAFETY: test-only; ENV_MUTEX held.
        unsafe { std::env::set_var("PATH", dir.path()) };

        let found = which_cargo();
        assert!(found.is_some(), "cargo placed on PATH should be found");
        assert_eq!(
            found.unwrap().file_name().and_then(|n| n.to_str()),
            Some(cargo_name)
        );

        // SAFETY: test-only; ENV_MUTEX held.
        unsafe {
            match saved_path {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    #[test]
    fn test_build_cargo_install_command_has_locked_force_and_bin() {
        let cmd = build_cargo_install_command("main", Path::new("/opt/tools"));
        assert!(cmd.contains(&"--locked".to_string()), "expected --locked");
        assert!(cmd.contains(&"--force".to_string()), "expected --force");
        assert!(cmd.contains(&"--bin".to_string()), "expected --bin");
        assert!(
            cmd.contains(&"ahma".to_string()),
            "expected ahma bin target"
        );
        assert!(cmd.contains(&"--git".to_string()), "expected --git");
        assert!(cmd.contains(&GIT_REPO_URL.to_string()), "expected repo URL");
        // --git must be immediately followed by the repo URL.
        let git_idx = cmd.iter().position(|s| s == "--git").unwrap();
        assert_eq!(cmd[git_idx + 1], GIT_REPO_URL);
    }
}
