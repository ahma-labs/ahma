//! Build and install ahma from a Git ref via `cargo install`.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use super::install::cargo_install_root;

pub const GIT_REPO_URL: &str = "https://github.com/paulirotta/ahma";

/// Build the `cargo install` command for a Git branch/ref.
pub fn build_cargo_install_command(branch: &str, install_dir: &Path) -> Vec<String> {
    let root = cargo_install_root(install_dir);
    vec![
        "cargo".to_string(),
        "install".to_string(),
        "--git".to_string(),
        GIT_REPO_URL.to_string(),
        "--branch".to_string(),
        branch.to_string(),
        "ahma_mcp".to_string(),
        "--bin".to_string(),
        "ahma".to_string(),
        "--root".to_string(),
        root.display().to_string(),
        "--locked".to_string(),
        "--force".to_string(),
    ]
}

/// Install from a Git branch using Cargo.
pub async fn install_from_git_ref(branch: &str, install_dir: &Path, dry_run: bool) -> Result<()> {
    let args = build_cargo_install_command(branch, install_dir);
    let display = args.join(" ");

    if dry_run {
        println!("[dry-run] Would run: {display}");
        return Ok(());
    }

    if which_cargo().is_none() {
        bail!(
            "cargo is not installed or not on PATH. \
             Install Rust from https://rustup.rs/ then retry."
        );
    }

    println!("Building ahma from Git branch '{branch}' (this may take several minutes)...");
    println!("  {display}");

    let status = tokio::process::Command::new(&args[0])
        .args(&args[1..])
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
        assert!(cmd.contains(&"--root".to_string()));
        assert!(cmd.contains(&"/home/u/.local".to_string()));
    }
}
