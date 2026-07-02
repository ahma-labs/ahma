use anyhow::{Result, anyhow};
use dunce;
use std::path::{Path, PathBuf};

use super::display::{ScopeSource, ScopeView};
use super::error::SandboxError;
use super::scopes;
use super::types::{SandboxMode, ScopesGuard};

// ─────────────────────────────────────────────────────────────────────────────
// Livelog symlink resolution helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Path to the out-of-sandbox log-symlink exceptions file
/// (`~/.config/ahma/log_exceptions.json`), or `None` if no config dir exists.
///
/// These approvals are stored *outside* any workspace scope so a sandboxed
/// agent cannot grant itself access to out-of-scope log targets by writing the
/// file — the same reasoning as [`ahma_core`-style] tool-approval grants.
/// Exceptions are keyed by workspace root:
///
/// ```json
/// { "/Users/you/sandbox/ahma": ["/abs/target/one", "/abs/target/two"] }
/// ```
fn log_exceptions_path() -> Option<PathBuf> {
    // Honors `AHMA_CONFIG_DIR` (tests / relocation), else the platform config dir.
    std::env::var_os("AHMA_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
        .map(|d| d.join("ahma").join("log_exceptions.json"))
}

/// Normalise a workspace root into the map key (canonical where possible).
fn workspace_key(primary_root: &Path) -> String {
    dunce::canonicalize(primary_root)
        .unwrap_or_else(|_| primary_root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn read_exceptions_map(path: &Path) -> std::collections::BTreeMap<String, Vec<String>> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

pub fn load_exceptions(primary_root: &Path) -> Vec<PathBuf> {
    let Some(path) = log_exceptions_path() else {
        return vec![];
    };
    let map = read_exceptions_map(&path);
    map.get(&workspace_key(primary_root))
        .map(|targets| targets.iter().map(PathBuf::from).collect())
        .unwrap_or_default()
}

/// Persist an approved out-of-scope symlink `target` for `primary_root` to the
/// out-of-sandbox exceptions file. Idempotent.
pub fn add_log_exception(primary_root: &Path, target: &Path) -> std::io::Result<()> {
    let Some(path) = log_exceptions_path() else {
        return Ok(()); // no config dir — nothing we can do, fail soft
    };

    let mut map = read_exceptions_map(&path);
    let key = workspace_key(primary_root);
    let target_str = target.to_string_lossy().into_owned();
    let entry = map.entry(key).or_default();
    if !entry.iter().any(|t| t == &target_str) {
        entry.push(target_str);
        entry.sort();
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let serialized = serde_json::to_string_pretty(&map)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, serialized)
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
    // Only use the parent-canonicalize shortcut when the last component is a
    // real name (not `..`).  If `file_name()` returns `None` the path ends in
    // a `ParentDir` component; on Windows `dunce::canonicalize` can resolve the
    // parent (which has one fewer `..`) to a path *inside* the sandbox scope
    // even though the full path with one more `..` would escape it.  Falling
    // through to `normalize_path_lexically` handles `..` components correctly
    // on every platform without filesystem access.
    if let Some(parent) = full_path.parent()
        && let Ok(parent_canonical) = dunce::canonicalize(parent)
        && let Some(name) = full_path.file_name()
    {
        return parent_canonical.join(name);
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
    /// When set, this directory (typically `~/sandbox`) is preserved across every
    /// `update_scopes` call so that `roots/list` replacements produce
    /// `roots ∪ {sandbox_dir}` rather than discarding the secondary scope.
    pub(super) sandbox_dir: Option<PathBuf>,
    /// User-granted external directories (writable) that survive `roots/list`
    /// replacement, just like [`sandbox_dir`](Self::sandbox_dir). These come from
    /// `[sandbox].persistent_scopes` with `access = "rw"` (e.g. an sccache cache
    /// outside the workspace) and are re-appended on every `update_scopes` call.
    pub(super) persistent_write_scopes: Vec<PathBuf>,
    /// User-granted external directories (read-only) that survive `roots/list`
    /// replacement. These come from `[sandbox].persistent_scopes` with
    /// `access = "ro"` and are merged into `read_scopes` on every update.
    pub(super) persistent_read_scopes: Vec<PathBuf>,
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
    /// When `--restrict-network` is on, the address of the local guarded egress
    /// proxy. On macOS the Seatbelt profile then denies all outbound IP egress
    /// *except* this address, so the subprocess can only reach the network through
    /// the allow-listed, SSRF-guarded proxy (R-NET enforcement). `None` leaves the
    /// blanket `(allow network*)` rule (advisory tier / restriction off). Set after
    /// the proxy binds its port; behind a lock because it is installed post-construction.
    pub(super) egress_proxy_addr: std::sync::RwLock<Option<std::net::SocketAddr>>,
    /// The scope commit latch and roots-received flag, modeled as an explicit
    /// state machine (SPEC R23). Owns the one-shot lock semantics and the memory
    /// ordering that the commit decision must not be reordered past the scopes
    /// write (SPEC R5.1 / R5.1.1 / R5.2.2).
    pub(super) scope_lock: super::scope_lock::ScopeLock,
}

impl Clone for Sandbox {
    fn clone(&self) -> Self {
        Self {
            scopes: std::sync::RwLock::new(self.scopes.read().unwrap().clone()),
            read_scopes: std::sync::RwLock::new(self.read_scopes.read().unwrap().clone()),
            mode: self.mode,
            no_temp_files: self.no_temp_files,
            tmp_access: self.tmp_access,
            sandbox_dir: self.sandbox_dir.clone(),
            persistent_write_scopes: self.persistent_write_scopes.clone(),
            persistent_read_scopes: self.persistent_read_scopes.clone(),
            explicit_scopes: self.explicit_scopes,
            livelog: self.livelog,
            package_cache_write: self.package_cache_write,
            egress_proxy_addr: std::sync::RwLock::new(*self.egress_proxy_addr.read().unwrap()),
            scope_lock: self.scope_lock.clone(),
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
            .field("sandbox_dir", &self.sandbox_dir)
            .field("persistent_write_scopes", &self.persistent_write_scopes)
            .field("persistent_read_scopes", &self.persistent_read_scopes)
            .field("explicit_scopes", &self.explicit_scopes)
            .field("livelog", &self.livelog)
            .field("package_cache_write", &self.package_cache_write)
            .field("scope_lock", &self.scope_lock)
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
            sandbox_dir: None,
            persistent_write_scopes: Vec::new(),
            persistent_read_scopes: Vec::new(),
            explicit_scopes: false,
            livelog,
            package_cache_write: true,
            egress_proxy_addr: std::sync::RwLock::new(None),
            scope_lock: super::scope_lock::ScopeLock::new(true),
        })
    }

    /// Install the guarded egress-proxy address for R-NET enforcement (macOS
    /// Seatbelt). Called once at server startup after the proxy binds. `None`
    /// disables the network-deny rule (restriction off).
    pub fn set_egress_proxy_addr(&self, addr: Option<std::net::SocketAddr>) {
        if let Ok(mut guard) = self.egress_proxy_addr.write() {
            *guard = addr;
        }
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

    /// Set a persistent secondary scope directory (typically `~/sandbox`) that is
    /// re-appended after every `update_scopes` call so it survives `roots/list`
    /// replacements.  The path must already be canonicalized by the caller.
    #[must_use]
    pub fn with_sandbox_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.sandbox_dir = dir;
        self
    }

    /// Return the persistent secondary scope directory, if one was configured.
    pub fn sandbox_dir(&self) -> Option<&PathBuf> {
        self.sandbox_dir.as_ref()
    }

    /// Register user-granted persistent scopes (from `[sandbox].persistent_scopes`).
    ///
    /// `write` paths join the writable scope set; `read` paths join the read-only
    /// set. Both are folded into the live scopes immediately — so the very first
    /// platform enforcement (Landlock at startup / Seatbelt per-command) already
    /// includes them — and stored so [`update_scopes`](Self::update_scopes)
    /// re-appends them after every `roots/list` replacement. Paths must already be
    /// canonicalized by the caller.
    #[must_use]
    pub fn with_persistent_scopes(mut self, write: Vec<PathBuf>, read: Vec<PathBuf>) -> Self {
        if !write.is_empty() {
            let mut scopes = self.scopes.write().unwrap();
            for p in &write {
                if !scopes.contains(p) {
                    scopes.push(p.clone());
                }
            }
        }
        if !read.is_empty() {
            let mut reads = self.read_scopes.write().unwrap();
            for p in &read {
                if !reads.contains(p) {
                    reads.push(p.clone());
                }
            }
        }
        self.persistent_write_scopes = write;
        self.persistent_read_scopes = read;
        self
    }

    /// Update the sandbox scopes, preserving the temp directory if `--tmp` was set,
    /// the sandbox_dir if `--sandbox` was set, and any user-granted persistent
    /// scopes (`[sandbox].persistent_scopes`).
    pub fn update_scopes(&self, scopes: Vec<PathBuf>) -> Result<()> {
        let mut canonicalized = scopes::canonicalize_scopes(
            scopes,
            self.mode,
            "Client must provide valid workspace roots.",
        )?;

        // Re-append ~/sandbox so roots/list replacements don't discard it.
        if let Some(ref dir) = self.sandbox_dir
            && !canonicalized.contains(dir)
        {
            tracing::info!(
                "Preserving sandbox directory in scopes after roots/list: {:?}",
                dir
            );
            canonicalized.push(dir.clone());
        }

        // Re-append user-granted writable persistent scopes so a client's
        // roots/list (e.g. Cursor sending its workspace root) does not silently
        // drop them. This is the durable half of the `ahma sandbox grant` flow.
        for dir in &self.persistent_write_scopes {
            if !canonicalized.contains(dir) {
                tracing::info!(
                    "Preserving granted persistent scope after roots/list: {:?}",
                    dir
                );
                canonicalized.push(dir.clone());
            }
        }

        if let Some(canonical_temp) = self.preserved_temp_dir(&canonicalized) {
            tracing::info!(
                "Preserving temp directory in sandbox scopes via --tmp: {:?}",
                canonical_temp
            );
            canonicalized.push(canonical_temp);
        }

        {
            let mut current_read_scopes = self.read_scopes.write().unwrap();
            // The livelog read set is recomputed from the new write scopes, so it
            // replaces the prior value wholesale — re-add granted read-only scopes
            // afterwards so they too survive the roots/list update.
            if self.livelog && self.mode != SandboxMode::Test {
                *current_read_scopes = resolve_livelog_scopes(&canonicalized);
            }
            for dir in &self.persistent_read_scopes {
                if !current_read_scopes.contains(dir) {
                    current_read_scopes.push(dir.clone());
                }
            }
        }

        let mut current_scopes = self.scopes.write().unwrap();
        *current_scopes = canonicalized;
        Ok(())
    }

    /// Set whether roots have been received.
    pub fn set_roots_received(&self, received: bool) {
        self.scope_lock.set_roots_received(received);
    }

    /// Return true if roots have been received.
    pub fn roots_received(&self) -> bool {
        self.scope_lock.roots_received()
    }

    /// The current observable state of the sandbox scope lock (SPEC R23).
    pub fn lock_state(&self) -> super::scope_lock::ScopeLockState {
        self.scope_lock.state()
    }

    /// Returns true once the sandbox scope has been committed (locked). After
    /// this, the scope is immutable and must not be re-derived from a later
    /// `roots/list` / `roots/list_changed` (SPEC R5.1 / R5.2.2).
    pub fn is_committed(&self) -> bool {
        self.scope_lock.is_committed()
    }

    /// Atomically claim the one-shot scope commit. Returns `true` for the single
    /// caller that wins the latch (and may proceed to apply/enforce scopes) and
    /// `false` for every subsequent call, which must treat the configuration as
    /// a tolerated no-op rather than widening the locked sandbox (SPEC R5.1.1).
    #[must_use]
    pub fn try_commit(&self) -> bool {
        self.scope_lock.try_commit()
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

    /// Whether kernel enforcement is active. `--no-sandbox` maps to
    /// [`SandboxMode::Test`], in which scope is resolved but never enforced.
    pub fn is_enforced(&self) -> bool {
        !self.is_test_mode()
    }

    /// Canonical human-readable scope summary with provenance (SPEC R5.4).
    /// Every surface that shows scope renders through this one path.
    pub fn scope_text(&self, source: ScopeSource) -> String {
        let writes = self.scopes.read().unwrap().clone();
        let reads = self.read_scopes.read().unwrap().clone();
        ScopeView {
            write_scopes: &writes,
            read_scopes: &reads,
            tmp_access: self.tmp_access,
            enforced: self.is_enforced(),
            source,
        }
        .render_text()
    }

    /// Structured scope summary for the `notifications/sandbox/configured`
    /// payload and any machine-readable surface (SPEC R5.4).
    pub fn scope_json(&self, source: ScopeSource) -> serde_json::Value {
        let writes = self.scopes.read().unwrap().clone();
        let reads = self.read_scopes.read().unwrap().clone();
        ScopeView {
            write_scopes: &writes,
            read_scopes: &reads,
            tmp_access: self.tmp_access,
            enforced: self.is_enforced(),
            source,
        }
        .to_json()
    }

    /// Check if a path is within any of the sandbox scopes.
    ///
    /// In `SandboxMode::Test` (`--no-sandbox`) scope enforcement is disabled; the
    /// path is still resolved to canonical form but never rejected.
    pub fn validate_path(&self, path: &Path) -> Result<PathBuf> {
        if self.is_test_mode() {
            let canonical = if path.is_absolute() {
                dunce::canonicalize(path).unwrap_or_else(|_| canonicalize_with_fallback(path))
            } else {
                let scopes_guard = self.scopes();
                if let Some(first_scope) = scopes_guard.first() {
                    let full = first_scope.join(path);
                    dunce::canonicalize(&full).unwrap_or_else(|_| canonicalize_with_fallback(&full))
                } else {
                    let full = std::env::current_dir().unwrap_or_default().join(path);
                    dunce::canonicalize(&full).unwrap_or_else(|_| canonicalize_with_fallback(&full))
                }
            };
            return Ok(canonical);
        }

        let scopes_guard = self.scopes();

        let canonical = self.resolve_path(path, &scopes_guard)?;

        if self.is_path_allowed(&canonical, &scopes_guard) {
            self.check_security_policies(path, &canonical)?;
            return Ok(canonical);
        }

        drop(scopes_guard);

        // SPEC R5.1 / R5.2.1: scope is locked once and is NEVER widened by
        // inference at runtime. The previous behaviour walked up from an
        // out-of-scope path looking for a project-marker ancestor and silently
        // added it as a writable scope — both spoofable marker inference and a
        // silent post-lock downgrade. An out-of-scope path is now simply
        // rejected; widening requires an explicit user decision (R5.3).

        // If we get here, it's not allowed, so return PathOutsideSandbox error
        let final_scopes_guard = self.scopes();
        Err(SandboxError::PathOutsideSandbox {
            path: path.to_path_buf(),
            scopes: final_scopes_guard.to_vec(),
        }
        .into())
    }

    /// Whether `path` resolves to a location inside the current (locked) scopes.
    ///
    /// Unlike [`Self::validate_path`], this is **not** relaxed in test mode — it
    /// answers the real scope-membership question in every mode. Read-only: it
    /// never mutates scopes. Used by the scope-grant detector to skip denials for
    /// paths that are already in scope (so it does not offer to "grant" them).
    pub fn is_path_in_scope(&self, path: &Path) -> bool {
        let scopes_guard = self.scopes();
        match self.resolve_path(path, &scopes_guard) {
            Ok(canonical) => self.is_path_allowed(&canonical, &scopes_guard),
            Err(_) => false,
        }
    }

    fn resolve_path(&self, path: &Path, scopes_guard: &[PathBuf]) -> Result<PathBuf> {
        let full_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            let first_scope = scopes_guard
                .first()
                .ok_or_else(|| anyhow!("No sandbox scopes configured"))?;
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

#[cfg(test)]
mod persistent_scope_tests {
    use super::*;
    use tempfile::tempdir;

    /// The core guarantee behind `ahma sandbox grant`: a granted external
    /// directory survives a client `roots/list` that otherwise replaces the
    /// workspace scope wholesale. Without the re-append in `update_scopes`, the
    /// grant would silently vanish the moment Cursor sent its workspace root.
    #[test]
    fn granted_scopes_survive_roots_list_replacement() {
        let workspace = tempdir().unwrap();
        let cache = tempdir().unwrap(); // rw grant (e.g. sccache)
        let toolchains = tempdir().unwrap(); // ro grant
        let client_root = tempdir().unwrap(); // arrives later via roots/list

        let sb = Sandbox::new(
            vec![workspace.path().to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap()
        .with_persistent_scopes(
            vec![cache.path().to_path_buf()],
            vec![toolchains.path().to_path_buf()],
        );

        // Builder folds grants into the live scopes immediately so the first
        // enforcement pass already covers them.
        assert!(sb.scopes().iter().any(|p| p == cache.path()));
        assert!(sb.read_scopes().iter().any(|p| p == toolchains.path()));

        // Simulate the client sending workspace roots, replacing the scope set.
        sb.update_scopes(vec![client_root.path().to_path_buf()])
            .unwrap();

        let scopes = sb.scopes();
        let client_canon = dunce::canonicalize(client_root.path()).unwrap();
        assert!(
            scopes.contains(&client_canon),
            "client root applied: {scopes:?}"
        );
        assert!(
            scopes.iter().any(|p| p == cache.path()),
            "rw grant survived roots/list: {scopes:?}"
        );
        let ws_canon = dunce::canonicalize(workspace.path()).unwrap();
        assert!(
            !scopes.contains(&ws_canon),
            "old workspace scope was replaced: {scopes:?}"
        );
        assert!(
            sb.read_scopes().iter().any(|p| p == toolchains.path()),
            "ro grant survived roots/list: {:?}",
            sb.read_scopes()
        );
    }
}

#[cfg(test)]
mod scope_view_tests {
    use super::*;
    use crate::sandbox::display::ScopeSource;
    use tempfile::tempdir;

    #[test]
    fn scope_json_reports_scopes_source_and_enforcement() {
        let dir = tempdir().unwrap();
        // Test mode avoids filesystem-root rejection and means "not enforced".
        let sb = Sandbox::new(
            vec![dir.path().to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap();

        let json = sb.scope_json(ScopeSource::RootsList);
        assert_eq!(json["source"], serde_json::json!("roots/list"));
        assert_eq!(json["tmp"], serde_json::json!(false));
        // Test mode == --no-sandbox == not enforced.
        assert_eq!(json["enforced"], serde_json::json!(false));
        let writes = json["write"].as_array().unwrap();
        assert!(
            !writes.is_empty(),
            "expected at least one write scope: {json}"
        );
    }

    #[test]
    fn out_of_scope_path_is_rejected_and_never_widens_scope() {
        // SPEC R5.1 / R5.2.1: an out-of-scope path is rejected; the locked scope
        // is never widened by inference at runtime (the removed find_auto_scope
        // behaviour). Guards against re-introducing silent marker-based widening.
        let allowed = tempdir().unwrap();
        let outside = tempdir().unwrap();
        // A project marker in the out-of-scope dir must NOT cause it to be added.
        std::fs::create_dir(outside.path().join(".git")).unwrap();
        let outside_file = outside.path().join("escape.txt");
        std::fs::write(&outside_file, b"x").unwrap();

        let sb = Sandbox::new(
            vec![allowed.path().to_path_buf()],
            SandboxMode::Strict,
            false,
            false,
            false,
        )
        .unwrap();
        // Simulate "no roots/list client" — the condition the old auto-add keyed on.
        sb.set_roots_received(false);

        let before = sb.scopes().to_vec();
        let result = sb.validate_path(&outside_file);
        assert!(result.is_err(), "out-of-scope path must be rejected");
        let after = sb.scopes().to_vec();
        assert_eq!(before, after, "scope must not be widened by validation");
    }

    #[test]
    fn scope_text_includes_source_attribution() {
        let dir = tempdir().unwrap();
        let sb = Sandbox::new(
            vec![dir.path().to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap();
        let text = sb.scope_text(ScopeSource::Default);
        assert!(
            text.contains("source: default"),
            "missing source line:\n{text}"
        );
        assert!(text.contains("Sandbox:"), "missing header:\n{text}");
    }
}
