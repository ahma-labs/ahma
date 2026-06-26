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
        if !dir.exists()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            tracing::debug!("Could not pre-create package-cache dir {:?}: {}", dir, e);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex};
    use tempfile::TempDir;

    /// Serialize all tests that mutate process-wide environment variables.
    static ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Saved environment that is restored on drop so tests do not leak state
    /// into one another regardless of pass/fail/panic.
    struct EnvRestore {
        cargo_home: Option<String>,
        home: Option<String>,
        userprofile: Option<String>,
    }

    impl EnvRestore {
        fn capture() -> Self {
            Self {
                cargo_home: std::env::var("CARGO_HOME").ok(),
                home: std::env::var("HOME").ok(),
                userprofile: std::env::var("USERPROFILE").ok(),
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            // SAFETY: test-only restoration; tests holding ENV_MUTEX are
            // serialized so no other thread reads/writes these vars concurrently.
            unsafe {
                restore("CARGO_HOME", &self.cargo_home);
                restore("HOME", &self.home);
                restore("USERPROFILE", &self.userprofile);
            }
        }
    }

    /// SAFETY: caller must hold `ENV_MUTEX`.
    unsafe fn restore(key: &str, val: &Option<String>) {
        unsafe {
            match val {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn cargo_home_uses_cargo_home_env_when_set() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _restore = EnvRestore::capture();
        let tmp = TempDir::new().unwrap();
        // SAFETY: test-only; serialized by ENV_MUTEX.
        unsafe { std::env::set_var("CARGO_HOME", tmp.path()) };

        let home = cargo_home();
        assert_eq!(home, tmp.path());
    }

    #[test]
    fn cargo_home_falls_back_to_dot_cargo_under_home() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _restore = EnvRestore::capture();
        let tmp = TempDir::new().unwrap();
        // SAFETY: test-only; serialized by ENV_MUTEX.
        unsafe {
            std::env::remove_var("CARGO_HOME");
            std::env::remove_var("USERPROFILE");
            std::env::set_var("HOME", tmp.path());
        }

        let home = cargo_home();
        assert_eq!(home, tmp.path().join(".cargo"));
    }

    #[test]
    fn cargo_home_falls_back_to_userprofile_when_home_unset() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _restore = EnvRestore::capture();
        let tmp = TempDir::new().unwrap();
        // SAFETY: test-only; serialized by ENV_MUTEX.
        unsafe {
            std::env::remove_var("CARGO_HOME");
            std::env::remove_var("HOME");
            std::env::set_var("USERPROFILE", tmp.path());
        }

        let home = cargo_home();
        assert_eq!(home, tmp.path().join(".cargo"));
    }

    #[test]
    fn cargo_cache_paths_none_when_base_missing() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _restore = EnvRestore::capture();
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        // SAFETY: test-only; serialized by ENV_MUTEX.
        unsafe { std::env::set_var("CARGO_HOME", &missing) };

        assert!(cargo_cache_paths().is_none());
    }

    #[test]
    fn cargo_cache_paths_some_with_derived_fields() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _restore = EnvRestore::capture();
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        // SAFETY: test-only; serialized by ENV_MUTEX.
        unsafe { std::env::set_var("CARGO_HOME", base) };

        let paths = cargo_cache_paths().expect("base exists, should be Some");

        assert_eq!(
            paths.writable_dirs,
            vec![base.join("registry"), base.join("git")]
        );
        assert_eq!(
            paths.writable_files,
            vec![
                base.join(".package-cache"),
                base.join(".package-cache-mutate"),
            ]
        );
    }

    #[test]
    fn all_writable_package_cache_paths_includes_cargo_when_present() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _restore = EnvRestore::capture();
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        // SAFETY: test-only; serialized by ENV_MUTEX.
        unsafe { std::env::set_var("CARGO_HOME", base) };

        let all = all_writable_package_cache_paths();
        assert_eq!(
            all.len(),
            1,
            "exactly the cargo ecosystem should be present"
        );
        assert!(all[0].writable_dirs.contains(&base.join("registry")));
        assert!(all[0].writable_dirs.contains(&base.join("git")));
    }

    #[test]
    fn all_writable_package_cache_paths_empty_when_base_missing() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _restore = EnvRestore::capture();
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope");
        // SAFETY: test-only; serialized by ENV_MUTEX.
        unsafe { std::env::set_var("CARGO_HOME", &missing) };

        assert!(all_writable_package_cache_paths().is_empty());
    }

    #[test]
    fn pre_create_creates_dirs_and_files() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let paths = PackageCachePaths {
            writable_dirs: vec![base.join("registry"), base.join("git")],
            writable_files: vec![
                base.join(".package-cache"),
                base.join("nested").join(".package-cache-mutate"),
            ],
        };

        pre_create_package_cache_paths(&paths);

        for dir in &paths.writable_dirs {
            assert!(dir.is_dir(), "expected dir to be created: {dir:?}");
        }
        for file in &paths.writable_files {
            assert!(file.is_file(), "expected file to be created: {file:?}");
        }
        // Parent of the nested lock file should have been created too.
        assert!(base.join("nested").is_dir());
    }

    #[test]
    fn pre_create_is_idempotent_when_paths_exist() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let dir = base.join("registry");
        let file = base.join(".package-cache");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, b"existing contents").unwrap();

        let paths = PackageCachePaths {
            writable_dirs: vec![dir.clone()],
            writable_files: vec![file.clone()],
        };

        // Second creation must not error or truncate the existing file.
        pre_create_package_cache_paths(&paths);

        assert!(dir.is_dir());
        assert!(file.is_file());
        assert_eq!(
            std::fs::read(&file).unwrap(),
            b"existing contents",
            "existing file must not be truncated/overwritten"
        );
    }
}
