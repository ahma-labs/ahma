use anyhow::{Result, anyhow};
use dunce;
use std::path::{Path, PathBuf};

use super::error::SandboxError;
use super::scopes;
use super::types::{SandboxMode, ScopesGuard};

// ─────────────────────────────────────────────────────────────────────────────
// Livelog symlink resolution helpers
// ─────────────────────────────────────────────────────────────────────────────

pub fn load_exceptions(primary_root: &Path) -> Vec<PathBuf> {
    let path = primary_root.join(".ahma").join("exceptions.json");
    if !path.exists() {
        return vec![];
    }
    let Ok(content) = std::fs::read_to_string(&path) else {
        return vec![];
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) else {
        return vec![];
    };
    let Some(arr) = val.get("approved_log_symlinks").and_then(|v| v.as_array()) else {
        return vec![];
    };
    arr.iter()
        .filter_map(|item| {
            let target = item.get("target_path").and_then(|v| v.as_str())?;
            Some(PathBuf::from(target))
        })
        .collect()
}

pub fn is_target_allowed(target: &Path, scopes: &[PathBuf], exceptions: &[PathBuf]) -> bool {
    if scopes.iter().any(|scope| target.starts_with(scope)) {
        return true;
    }
    if exceptions.iter().any(|exc| target == exc) {
        return true;
    }
    false
}

fn resolve_livelog_scopes(canonicalized: &[PathBuf]) -> Vec<PathBuf> {
    let exceptions = if let Some(primary) = canonicalized.first() {
        load_exceptions(primary)
    } else {
        vec![]
    };

    canonicalized
        .iter()
        .filter_map(|scope| {
            resolve_log_dir_symlinks(&log_dir_for_scope(scope), canonicalized, &exceptions)
        })
        .flatten()
        .collect()
}

fn log_dir_for_scope(scope: &Path) -> PathBuf {
    scope.join("logs")
}

fn resolve_log_dir_symlinks(
    log_dir: &Path,
    scopes: &[PathBuf],
    exceptions: &[PathBuf],
) -> Option<Vec<PathBuf>> {
    let entries = std::fs::read_dir(log_dir).ok()?;
    Some(
        entries
            .flatten()
            .filter_map(|entry| {
                let target = resolve_log_symlink(&entry.path(), log_dir, scopes, exceptions)?;
                tracing::info!(
                    "Adding --livelog read-only scope for symlink target: {}",
                    target.display()
                );
                Some(target)
            })
            .collect(),
    )
}

fn resolve_log_symlink(
    path: &Path,
    log_dir: &Path,
    scopes: &[PathBuf],
    exceptions: &[PathBuf],
) -> Option<PathBuf> {
    if !is_log_symlink(path) {
        return None;
    }

    let target = resolve_log_symlink_target(path, log_dir)?;
    if is_target_allowed(&target, scopes, exceptions) {
        Some(target)
    } else {
        tracing::warn!(
            "Blocked out-of-scope log symlink target: {} (link: {}). Use logs_approve to authorize.",
            target.display(),
            path.display()
        );
        None
    }
}

fn is_log_symlink(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    meta.is_symlink() && path.extension().is_some_and(|ext| ext == "log")
}

fn resolve_log_symlink_target(path: &Path, log_dir: &Path) -> Option<PathBuf> {
    let target = std::fs::read_link(path).ok()?;
    let canonical_target = dunce::canonicalize(log_dir.join(&target)).ok()?;
    canonical_target.is_file().then_some(canonical_target)
}

// ─────────────────────────────────────────────────────────────────────────────
// Security policy helpers
// ─────────────────────────────────────────────────────────────────────────────

fn is_blocked_temp_path(path_str: &str) -> bool {
    const BLOCKED_PREFIXES: &[&str] = &[
        "/tmp",
        "/var/folders",
        "/private/tmp",
        "/private/var/folders",
        "/dev",
    ];
    BLOCKED_PREFIXES
        .iter()
        .any(|prefix| path_str.starts_with(prefix))
}

fn is_in_temp_dir(path: &Path) -> bool {
    dunce::canonicalize(std::env::temp_dir())
        .map(|temp_dir| path.starts_with(&temp_dir))
        .unwrap_or(false)
}

fn canonicalize_with_fallback(full_path: &Path) -> PathBuf {
    if let Some(parent) = full_path.parent()
        && let Ok(parent_canonical) = dunce::canonicalize(parent)
    {
        return full_path
            .file_name()
            .map(|name| parent_canonical.join(name))
            .unwrap_or(parent_canonical);
    }
    scopes::normalize_path_lexically(full_path)
}

/// The security context for the Ahma session.
pub struct Sandbox {
    pub(super) scopes: std::sync::RwLock<Vec<PathBuf>>,
    pub(super) read_scopes: std::sync::RwLock<Vec<PathBuf>>,
    pub(super) mode: SandboxMode,
    pub(super) no_temp_files: bool,
    /// When true, the canonical temp directory is preserved across scope updates.
    pub(super) tmp_access: bool,
    /// When true, the scopes were explicitly provided by the user (via
    /// `--sandbox-scope`, `--working-directories`, or a task vault) and MUST NOT
    /// be widened or replaced via the MCP `roots/list` protocol (SPEC R5.5).
    ///
    /// When false, the scopes were implicitly derived (e.g. from the current
    /// working directory or the `--tmp` temp scope). Implicit scopes are only a
    /// fallback: the server still requests `roots/list` from the client and
    /// prefers the client's workspace roots when provided. This is what lets
    /// shared-process clients like Cursor — whose subprocess CWD is unrelated to
    /// the open workspace — get sandboxed to the correct workspace root.
    pub(super) explicit_scopes: bool,
    pub(super) livelog: bool,
    /// Allow package-manager caches (cargo registry/git) to be written.
    /// Default `true`; disable with `--no-package-cache-write`.
    pub(super) package_cache_write: bool,
    /// When true, cargo commands spawned by ahma use `target/ahma/` instead of
    /// `target/`, isolating ahma's build artefacts from the IDE's background
    /// `cargo check` and preventing cross-process file-lock contention.
    /// Default `false`.
    pub(super) separate_cargo_target: bool,
}

impl Clone for Sandbox {
    fn clone(&self) -> Self {
        Self {
            scopes: std::sync::RwLock::new(self.scopes.read().unwrap().clone()),
            read_scopes: std::sync::RwLock::new(self.read_scopes.read().unwrap().clone()),
            mode: self.mode,
            no_temp_files: self.no_temp_files,
            tmp_access: self.tmp_access,
            explicit_scopes: self.explicit_scopes,
            livelog: self.livelog,
            package_cache_write: self.package_cache_write,
            separate_cargo_target: self.separate_cargo_target,
        }
    }
}

impl std::fmt::Debug for Sandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sandbox")
            .field("scopes", &self.scopes.read().unwrap())
            .field("read_scopes", &self.read_scopes.read().unwrap())
            .field("mode", &self.mode)
            .field("no_temp_files", &self.no_temp_files)
            .field("tmp_access", &self.tmp_access)
            .field("explicit_scopes", &self.explicit_scopes)
            .field("livelog", &self.livelog)
            .field("package_cache_write", &self.package_cache_write)
            .field("separate_cargo_target", &self.separate_cargo_target)
            .finish()
    }
}

impl Sandbox {
    /// Create a new Sandbox with the given scopes.
    pub fn new(
        scopes: Vec<PathBuf>,
        mode: SandboxMode,
        no_temp_files: bool,
        livelog: bool,
        tmp_access: bool,
    ) -> Result<Self> {
        let canonicalized = scopes::canonicalize_scopes(
            scopes,
            mode,
            "Specify explicit directories with --sandbox-scope or --working-directories. \
             Example: --sandbox-scope /home/user/project",
        )?;

        let read_scopes = if livelog && mode != SandboxMode::Test {
            resolve_livelog_scopes(&canonicalized)
        } else {
            Default::default()
        };

        Ok(Self {
            scopes: std::sync::RwLock::new(canonicalized),
            read_scopes: std::sync::RwLock::new(read_scopes),
            mode,
            no_temp_files,
            tmp_access,
            explicit_scopes: false,
            livelog,
            package_cache_write: true,
            separate_cargo_target: false,
        })
    }

    /// Override the package-cache-write flag.
    ///
    /// The default is `true` (on). Pass `false` to disable write access to the
    /// package-manager cache directories (cargo registry/git, etc.).  This is
    /// the builder counterpart of `--no-package-cache-write`.
    #[must_use]
    pub fn with_package_cache_write(mut self, enabled: bool) -> Self {
        self.package_cache_write = enabled;
        self
    }

    /// Use a dedicated `target/ahma/` subdirectory for cargo builds.
    ///
    /// When `true`, ahma sets `CARGO_TARGET_DIR=<working_dir>/target/ahma` for
    /// every cargo command it spawns.  This isolates ahma's build artefacts from
    /// the IDE's background `cargo check`, eliminating cross-process file-lock
    /// contention at the cost of a separate build cache.
    #[must_use]
    pub fn with_separate_cargo_target(mut self, enabled: bool) -> Self {
        self.separate_cargo_target = enabled;
        self
    }

    /// Returns `true` when ahma uses `target/ahma/` for cargo builds.
    pub fn is_separate_cargo_target(&self) -> bool {
        self.separate_cargo_target
    }

    /// Mark whether the initial scopes were explicitly provided by the user.
    ///
    /// Explicit scopes (`--sandbox-scope`, `--working-directories`, task vault)
    /// must not be widened or replaced via `roots/list` (SPEC R5.5). Implicit
    /// scopes (CWD fallback, `--tmp`) are provisional and yield to client roots.
    #[must_use]
    pub fn with_explicit_scopes(mut self, explicit: bool) -> Self {
        self.explicit_scopes = explicit;
        self
    }

    /// Returns `true` when the scopes were explicitly provided by the user and
    /// must not be modified via the MCP `roots/list` protocol (SPEC R5.5).
    pub fn has_explicit_scopes(&self) -> bool {
        self.explicit_scopes
    }

    /// Update the sandbox scopes, preserving the temp directory if `--tmp` was set.
    pub fn update_scopes(&self, scopes: Vec<PathBuf>) -> Result<()> {
        let mut canonicalized = scopes::canonicalize_scopes(
            scopes,
            self.mode,
            "Client must provide valid workspace roots.",
        )?;

        if let Some(canonical_temp) = self.preserved_temp_dir(&canonicalized) {
            tracing::info!(
                "Preserving temp directory in sandbox scopes via --tmp: {:?}",
                canonical_temp
            );
            canonicalized.push(canonical_temp);
        }

        if self.livelog && self.mode != SandboxMode::Test {
            let new_read_scopes = resolve_livelog_scopes(&canonicalized);
            let mut current_read_scopes = self.read_scopes.write().unwrap();
            *current_read_scopes = new_read_scopes;
        }

        let mut current_scopes = self.scopes.write().unwrap();
        *current_scopes = canonicalized;
        Ok(())
    }

    /// Check if the sandbox is in test mode.
    pub fn is_test_mode(&self) -> bool {
        self.mode == SandboxMode::Test
    }

    /// Get the sandbox mode.
    pub fn mode(&self) -> SandboxMode {
        self.mode
    }

    /// Returns true when tool calls can execute against sandboxed roots.
    ///
    /// In test mode, tool calls are always allowed. In normal modes, at least one
    /// rooted scope must be configured (typically via roots/list).
    pub fn is_ready_for_tool_calls(&self) -> bool {
        self.is_test_mode() || !self.scopes().is_empty()
    }

    /// Check if no-temp-files mode is enabled.
    pub fn is_no_temp_files(&self) -> bool {
        self.no_temp_files
    }

    /// Check if tmp_access is enabled.
    pub fn is_tmp_access(&self) -> bool {
        self.tmp_access
    }

    /// Check if package-cache writes are enabled (default `true`).
    pub fn package_cache_write(&self) -> bool {
        self.package_cache_write
    }

    /// Get the allowed scopes.
    pub fn scopes(&self) -> ScopesGuard<'_> {
        ScopesGuard(self.scopes.read().unwrap())
    }

    /// Get the read-only scopes (for --livelog symlink targets).
    pub fn read_scopes(&self) -> Vec<PathBuf> {
        self.read_scopes.read().unwrap().clone()
    }

    /// Check if a path is within any of the sandbox scopes.
    pub fn validate_path(&self, path: &Path) -> Result<PathBuf> {
        let scopes_guard = self.scopes();

        let canonical = self.resolve_path(path, &scopes_guard)?;

        if !self.is_path_allowed(&canonical, &scopes_guard) {
            return Err(SandboxError::PathOutsideSandbox {
                path: path.to_path_buf(),
                scopes: scopes_guard.to_vec(),
            }
            .into());
        }

        self.check_security_policies(path, &canonical)?;
        Ok(canonical)
    }

    fn resolve_path(&self, path: &Path, scopes_guard: &[PathBuf]) -> Result<PathBuf> {
        let first_scope = scopes_guard
            .first()
            .ok_or_else(|| anyhow!("No sandbox scopes configured"))?;

        let full_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            first_scope.join(path)
        };

        Ok(dunce::canonicalize(&full_path)
            .unwrap_or_else(|_| canonicalize_with_fallback(&full_path)))
    }

    fn is_path_allowed(&self, canonical: &Path, scopes_guard: &[PathBuf]) -> bool {
        let canonical_stripped = strip_extended_prefix(canonical);
        scopes_guard
            .iter()
            .any(|scope| canonical_stripped.starts_with(strip_extended_prefix(scope)))
    }

    fn check_security_policies(&self, original_path: &Path, canonical: &Path) -> Result<()> {
        if !self.no_temp_files {
            return Ok(());
        }

        let path_str = canonical.to_string_lossy();
        let is_blocked = is_blocked_temp_path(&path_str) || is_in_temp_dir(canonical);
        if !is_blocked {
            return Ok(());
        }

        Err(SandboxError::HighSecurityViolation {
            path: original_path.to_path_buf(),
        }
        .into())
    }

    fn preserved_temp_dir(&self, canonicalized: &[PathBuf]) -> Option<PathBuf> {
        if !self.tmp_access {
            return None;
        }

        let canonical_temp = dunce::canonicalize(std::env::temp_dir()).ok()?;
        (!canonicalized.contains(&canonical_temp)).then_some(canonical_temp)
    }
}

/// Strip the Windows extended-length path prefix (`\\?\`) if present.
/// Returns an owned `PathBuf`; on non-Windows this is always a clone.
///
/// Windows `std::fs::canonicalize` may add or omit `\\?\` depending on
/// the input form.  Stripping before `starts_with` comparisons lets paths
/// referring to the same location compare equal.
fn strip_extended_prefix(path: &Path) -> PathBuf {
    #[cfg(target_os = "windows")]
    if let Some(stripped) = path.as_os_str().to_string_lossy().strip_prefix(r"\\?\") {
        return PathBuf::from(stripped);
    }
    path.to_path_buf()
}
