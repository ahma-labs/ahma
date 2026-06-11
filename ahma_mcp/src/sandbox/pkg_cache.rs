//! Package-manager cache path resolution for sandbox write rules.
//!
//! When `package_cache_write` is enabled (the default), the sandbox grants write
//! access to the subdirs that package managers need in order to fetch new
//! dependency versions.  Sensitive paths (binaries, config, credentials) are
//! deliberately excluded and remain under the existing read-only rule.
//!
//! # Cargo
//!
//! The writable set is:
//! - `{base}/registry`  — index + cache + src (needed for `cargo add`/`cargo update`)
//! - `{base}/git`       — git checkouts (needed for git dependencies)
//! - `{base}/.package-cache`        — exclusive download-lock file
//! - `{base}/.package-cache-mutate` — exclusive mutate-lock file
//!
//! The following are intentionally **never** writable:
//! - `{base}/bin`            — installed binaries
//! - `{base}/config.toml`    — registry credentials / source overrides
//! - `{base}/credentials.toml` — crates.io token
//!
//! # Extensibility
//!
//! Add new ecosystems (npm, pip, go) by appending entries to
//! [`PackageCachePaths`].  The platform backends (`seatbelt.rs`,
//! `landlock.rs`) iterate the `writable_dirs` and `writable_files` fields
//! from the returned struct.

// Functions are only called from platform-specific modules (seatbelt on macOS,
// landlock on Linux).  On the current platform the functions ARE used; suppress
// the false-positive dead_code warnings from cross-compilation analysis.
#![allow(dead_code)]

use std::path::PathBuf;

/// Writable paths for one package-manager ecosystem.
pub struct PackageCachePaths {
    /// Directories that must be writable (write access applies recursively).
    pub writable_dirs: Vec<PathBuf>,
    /// Individual files that must be writable (lock files, etc.).
    pub writable_files: Vec<PathBuf>,
}

/// Compute the writable cargo cache paths for the current user.
///
/// Returns `None` if the base directory does not exist (nothing to grant).
fn cargo_cache_paths() -> Option<PackageCachePaths> {
    let base = cargo_home();
    if !base.exists() {
        return None;
    }

    let writable_dirs = vec![base.join("registry"), base.join("git")];
    let writable_files = vec![
        base.join(".package-cache"),
        base.join(".package-cache-mutate"),
    ];

    Some(PackageCachePaths {
        writable_dirs,
        writable_files,
    })
}

/// Return the cargo home directory: `$CARGO_HOME` if set, else `~/.cargo`.
pub fn cargo_home() -> PathBuf {
    if let Ok(p) = std::env::var("CARGO_HOME") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".cargo")
}

/// Resolve all writable package-cache paths that should receive write access.
///
/// Currently only cargo is implemented; the structure is ready for npm/pip/go.
/// Returns paths only for ecosystems whose cache base dir exists on disk.
pub fn all_writable_package_cache_paths() -> Vec<PackageCachePaths> {
    let mut result = Vec::new();
    if let Some(cargo) = cargo_cache_paths() {
        result.push(cargo);
    }
    // Future: npm_cache_paths(), pip_cache_paths(), go_cache_paths(), …
    result
}

/// Ensure that writable_dirs and writable_files exist so that platform
/// backends can open file-descriptors to them (Landlock requires the path to
/// exist before calling `PathFd::new`).
///
/// This is a best-effort operation — missing directories are created, missing
/// files are touched.  Errors are logged and silently ignored so that the
/// sandbox still starts even in restricted environments.
pub fn pre_create_package_cache_paths(paths: &PackageCachePaths) {
    for dir in &paths.writable_dirs {
        if !dir.exists() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                tracing::debug!("Could not pre-create package-cache dir {:?}: {}", dir, e);
            }
        }
    }
    for file in &paths.writable_files {
        if !file.exists() {
            if let Some(parent) = file.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(file)
            {
                tracing::debug!("Could not pre-create package-cache file {:?}: {}", file, e);
            }
        }
    }
}
