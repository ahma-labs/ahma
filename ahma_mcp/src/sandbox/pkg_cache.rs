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
///
/// When ahma runs **nested inside Cursor's agent sandbox**, Cursor injects ~25
/// build-cache environment variables (`CARGO_TARGET_DIR`, `GOCACHE`,
/// `NPM_CONFIG_CACHE`, …) pointing *outside* the workspace into its own
/// `cursor-sandbox-cache/` tree. ahma's sandbox would otherwise deny every build
/// that writes there. [`editor_injected_cache_paths`] auto-grants those specific
/// directories so builds "just work" with no per-session grant prompt — see that
/// function for the safety gating.
pub fn all_writable_package_cache_paths() -> Vec<PackageCachePaths> {
    let mut result = Vec::new();
    if let Some(cargo) = cargo_cache_paths() {
        result.push(cargo);
    }
    // Future: npm_cache_paths(), pip_cache_paths(), go_cache_paths(), …
    if let Some(editor) = editor_injected_cache_paths() {
        result.push(editor);
    }
    result
}

/// Environment variables a host editor/agent sandbox may set to redirect a build
/// tool's cache/output directory. Honored only under the strict gating in
/// [`editor_injected_cache_paths`] (nested-in-Cursor + path inside the sandbox
/// cache tree), so listing a superset here is safe: an unset or out-of-tree var
/// is simply ignored.
const EDITOR_CACHE_ENV_VARS: &[&str] = &[
    "CARGO_TARGET_DIR",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "SCCACHE_DIR",
    "CCACHE_DIR",
    "GOCACHE",
    "GOMODCACHE",
    "NPM_CONFIG_CACHE",
    "npm_config_devdir",
    "PNPM_STORE_PATH",
    "YARN_CACHE_FOLDER",
    "BUN_INSTALL_CACHE_DIR",
    "PIP_CACHE_DIR",
    "UV_CACHE_DIR",
    "POETRY_CACHE_DIR",
    "CONDA_PKGS_DIRS",
    "GRADLE_USER_HOME",
    "GEM_SPEC_CACHE",
    "BUNDLE_PATH",
    "NUGET_PACKAGES",
    "COMPOSER_HOME",
    "HOMEBREW_CACHE",
    "CP_HOME_DIR",
    "PLAYWRIGHT_BROWSERS_PATH",
    "PUPPETEER_CACHE_DIR",
    "CYPRESS_CACHE_FOLDER",
    "NX_CACHE_DIRECTORY",
    "TURBO_CACHE_DIR",
];

/// The path segment Cursor uses for its per-session build-cache tree. A candidate
/// directory is only auto-granted when it lives under such a segment — this is the
/// security gate that prevents a hostile env var (e.g. `CARGO_TARGET_DIR=~/.ssh`)
/// from widening the sandbox: that path lacks the marker and is ignored.
const CURSOR_CACHE_MARKER: &str = "cursor-sandbox-cache";

/// True when ahma is running nested inside Cursor's agent sandbox and the user has
/// not opted out of auto-granting the injected build caches.
///
/// Opt-out: set `AHMA_NO_EDITOR_CACHE_WRITE=1` (or any non-empty/`true`/`1`
/// value). The umbrella `--no-package-cache-write` also disables this, since the
/// backends skip [`all_writable_package_cache_paths`] entirely when it is off.
fn editor_cache_write_enabled() -> bool {
    if std::env::var_os("CURSOR_SANDBOX").is_none() {
        return false;
    }
    // Opt-out: any truthy AHMA_NO_EDITOR_CACHE_WRITE disables the auto-grant.
    if let Ok(v) = std::env::var("AHMA_NO_EDITOR_CACHE_WRITE") {
        let v = v.trim().to_ascii_lowercase();
        let opted_out = !(v.is_empty() || v == "0" || v == "false" || v == "no");
        if opted_out {
            return false;
        }
    }
    true
}

/// Collect the host-editor-injected build-cache directories that should be
/// writable, or `None` when not applicable.
///
/// Gating (all required):
///  1. [`editor_cache_write_enabled`] — we are nested in Cursor and not opted out.
///  2. Each candidate comes from the [`EDITOR_CACHE_ENV_VARS`] allowlist.
///  3. The candidate is an absolute path **under a [`CURSOR_CACHE_MARKER`]
///     segment** — this confines auto-grants to Cursor's own cache tree and
///     rejects any var repointed at a sensitive location.
fn editor_injected_cache_paths() -> Option<PackageCachePaths> {
    if !editor_cache_write_enabled() {
        return None;
    }

    let mut writable_dirs: Vec<PathBuf> = Vec::new();
    for var in EDITOR_CACHE_ENV_VARS {
        let Some(raw) = std::env::var_os(var) else {
            continue;
        };
        let path = PathBuf::from(raw);
        if !path.is_absolute() {
            continue;
        }
        if !path
            .components()
            .any(|c| c.as_os_str() == CURSOR_CACHE_MARKER)
        {
            continue;
        }
        if !writable_dirs.contains(&path) {
            writable_dirs.push(path);
        }
    }

    if writable_dirs.is_empty() {
        return None;
    }

    tracing::info!(
        "Detected Cursor agent sandbox (CURSOR_SANDBOX set); auto-granting write to {} \
         editor-injected build-cache dir(s) so builds are not denied: {:?}. \
         Disable with AHMA_NO_EDITOR_CACHE_WRITE=1.",
        writable_dirs.len(),
        writable_dirs
    );

    Some(PackageCachePaths {
        writable_dirs,
        writable_files: Vec::new(),
    })
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
        // Neutralize any Cursor editor-cache vars so this asserts on cargo alone
        // even when the test process itself runs inside Cursor.
        let _editor = capture_and_clear_editor_env();
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
        // Neutralize Cursor editor-cache vars so "empty" holds even when the test
        // process runs inside Cursor.
        let _editor = capture_and_clear_editor_env();
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

    // ── editor-injected cache auto-grant (Cursor nesting) ─────────────────────

    /// Save/restore an arbitrary set of env vars on drop so editor-cache tests do
    /// not leak state. Tests hold `ENV_MUTEX` while a guard is alive.
    struct VarsRestore {
        saved: Vec<(String, Option<String>)>,
    }

    impl VarsRestore {
        fn capture(names: &[&str]) -> Self {
            Self {
                saved: names
                    .iter()
                    .map(|n| (n.to_string(), std::env::var(n).ok()))
                    .collect(),
            }
        }
    }

    impl Drop for VarsRestore {
        fn drop(&mut self) {
            // SAFETY: tests holding ENV_MUTEX are serialized.
            unsafe {
                for (n, v) in &self.saved {
                    match v {
                        Some(val) => std::env::set_var(n, val),
                        None => std::env::remove_var(n),
                    }
                }
            }
        }
    }

    /// Capture every var the editor-cache logic reads, then clear them so each
    /// test starts from a known-empty environment (important because the test
    /// process itself often runs *inside* Cursor with these vars already set).
    fn capture_and_clear_editor_env() -> VarsRestore {
        let mut names: Vec<&str> = EDITOR_CACHE_ENV_VARS.to_vec();
        names.push("CURSOR_SANDBOX");
        names.push("AHMA_NO_EDITOR_CACHE_WRITE");
        let guard = VarsRestore::capture(&names);
        // SAFETY: caller holds ENV_MUTEX.
        unsafe {
            for n in &names {
                std::env::remove_var(n);
            }
        }
        guard
    }

    #[test]
    fn editor_cache_none_without_cursor_sandbox() {
        let _g = ENV_MUTEX.lock().unwrap();
        let _r = capture_and_clear_editor_env();
        // A cache var is set to a real-looking cursor path, but CURSOR_SANDBOX is
        // unset → must NOT auto-grant anything (we only act when nested).
        // SAFETY: serialized by ENV_MUTEX.
        unsafe {
            std::env::set_var(
                "CARGO_TARGET_DIR",
                "/tmp/cursor-sandbox-cache/abc/cargo-target",
            );
        }
        assert!(editor_injected_cache_paths().is_none());
    }

    #[test]
    fn editor_cache_grants_dirs_under_cursor_marker() {
        let _g = ENV_MUTEX.lock().unwrap();
        let _r = capture_and_clear_editor_env();
        // SAFETY: serialized by ENV_MUTEX.
        unsafe {
            std::env::set_var("CURSOR_SANDBOX", "1");
            std::env::set_var(
                "CARGO_TARGET_DIR",
                "/tmp/cursor-sandbox-cache/abc/cargo-target",
            );
            std::env::set_var("GOCACHE", "/tmp/cursor-sandbox-cache/abc/go-build");
        }
        let paths = editor_injected_cache_paths().expect("nested → should grant");
        assert!(
            paths
                .writable_dirs
                .contains(&PathBuf::from("/tmp/cursor-sandbox-cache/abc/cargo-target"))
        );
        assert!(
            paths
                .writable_dirs
                .contains(&PathBuf::from("/tmp/cursor-sandbox-cache/abc/go-build"))
        );
        assert!(paths.writable_files.is_empty());
    }

    #[test]
    fn editor_cache_rejects_paths_outside_cursor_marker() {
        let _g = ENV_MUTEX.lock().unwrap();
        let _r = capture_and_clear_editor_env();
        // A hostile/incidental var repointed at a sensitive dir lacks the
        // cursor-sandbox-cache marker → must be ignored even when nested.
        // SAFETY: serialized by ENV_MUTEX.
        unsafe {
            std::env::set_var("CURSOR_SANDBOX", "1");
            std::env::set_var("CARGO_TARGET_DIR", "/Users/victim/.ssh");
        }
        assert!(
            editor_injected_cache_paths().is_none(),
            "paths outside the cursor cache tree must never be auto-granted"
        );
    }

    #[test]
    fn editor_cache_respects_opt_out() {
        let _g = ENV_MUTEX.lock().unwrap();
        let _r = capture_and_clear_editor_env();
        // SAFETY: serialized by ENV_MUTEX.
        unsafe {
            std::env::set_var("CURSOR_SANDBOX", "1");
            std::env::set_var("AHMA_NO_EDITOR_CACHE_WRITE", "1");
            std::env::set_var(
                "CARGO_TARGET_DIR",
                "/tmp/cursor-sandbox-cache/abc/cargo-target",
            );
        }
        assert!(
            editor_injected_cache_paths().is_none(),
            "AHMA_NO_EDITOR_CACHE_WRITE=1 must disable the auto-grant"
        );
    }

    #[test]
    fn editor_cache_opt_out_falsey_values_keep_it_enabled() {
        let _g = ENV_MUTEX.lock().unwrap();
        let _r = capture_and_clear_editor_env();
        // SAFETY: serialized by ENV_MUTEX.
        unsafe {
            std::env::set_var("CURSOR_SANDBOX", "1");
            std::env::set_var("AHMA_NO_EDITOR_CACHE_WRITE", "0");
            std::env::set_var(
                "CARGO_TARGET_DIR",
                "/tmp/cursor-sandbox-cache/abc/cargo-target",
            );
        }
        assert!(
            editor_injected_cache_paths().is_some(),
            "a falsey opt-out value must NOT disable the auto-grant"
        );
    }

    #[test]
    fn all_writable_includes_editor_cache_when_nested() {
        let _g = ENV_MUTEX.lock().unwrap();
        let _r = capture_and_clear_editor_env();
        // Point CARGO_HOME at a missing dir so the cargo ecosystem contributes
        // nothing; the only entry should be the editor cache.
        let tmp = TempDir::new().unwrap();
        // SAFETY: serialized by ENV_MUTEX.
        unsafe {
            std::env::set_var("CARGO_HOME", tmp.path().join("missing-cargo-home"));
            std::env::set_var("CURSOR_SANDBOX", "1");
            std::env::set_var(
                "CARGO_TARGET_DIR",
                "/tmp/cursor-sandbox-cache/abc/cargo-target",
            );
        }
        let all = all_writable_package_cache_paths();
        assert!(
            all.iter().any(|p| p
                .writable_dirs
                .contains(&PathBuf::from("/tmp/cursor-sandbox-cache/abc/cargo-target"))),
            "all_writable_package_cache_paths must include the editor cache when nested"
        );
    }
}
