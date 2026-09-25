use anyhow::{Result, anyhow};
use dunce;
use std::path::{Path, PathBuf};

use super::display::{ActiveSandbox, ScopeSource, ScopeView};
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
/// file — the same reasoning as `ahma_core`-style tool-approval grants.
/// Exceptions are keyed by workspace root:
///
/// ```json
/// { "/Users/you/github/ahma": ["/abs/target/one", "/abs/target/two"] }
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
    scope.join(".ahma").join("logs")
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
    // `std::env::temp_dir()` is fixed for the process lifetime, and
    // canonicalizing it is a syscall — cache it once rather than paying that
    // cost on every path check.
    static CANONICAL_TEMP_DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    CANONICAL_TEMP_DIR
        .get_or_init(|| dunce::canonicalize(std::env::temp_dir()).ok())
        .as_deref()
        .is_some_and(|temp_dir| path.starts_with(temp_dir))
}

/// The name of the immediate child of `container` that `requested` lives in, or
/// `None` when `requested` is the container itself or lies outside it.
///
/// A *name* rather than a full path because the caller has to apply it to every
/// spelling of the container that is in scope, not just the canonical one.
///
/// The container itself deliberately selects nothing: a caller naming the
/// container has not chosen a project, which is exactly the case R5.2.8 makes
/// the command surfaces refuse outright.
fn container_child_name(container: &Path, requested: &Path) -> Option<std::ffi::OsString> {
    let container = scopes::resolve_for_comparison(container);
    let requested = scopes::resolve_for_comparison(requested);

    let relative = requested.strip_prefix(&container).ok()?;
    Some(relative.components().next()?.as_os_str().to_os_string())
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

/// Append every path in `additions` that `target` does not already contain,
/// preserving both the existing order and the order of `additions`.
///
/// The de-duplication is exact-equality on the already-canonicalized paths, the
/// same test the call sites used inline: scope sets are small, and a widening
/// prefix test here would be a policy change, not a refactor.
fn append_missing_scopes(target: &mut Vec<PathBuf>, additions: &[PathBuf]) {
    for path in additions {
        if !target.contains(path) {
            target.push(path.clone());
        }
    }
}

/// The security context for the Ahma session.
pub struct Sandbox {
    pub(super) scopes: parking_lot::RwLock<Vec<PathBuf>>,
    pub(super) read_scopes: parking_lot::RwLock<Vec<PathBuf>>,
    pub(super) mode: SandboxMode,
    pub(super) no_temp_files: bool,
    /// When true, the canonical temp directory is preserved across scope updates.
    pub(super) tmp_access: bool,
    /// When set, this user-configured scratch directory is preserved across every
    /// `update_scopes` call so that `roots/list` replacements produce
    /// `roots ∪ {sandbox_dir}` rather than discarding the secondary scope.
    pub(super) scratch_dir: Option<PathBuf>,
    /// User-granted external directories (writable) that survive `roots/list`
    /// replacement, just like [`Self::scratch_dir`]. These come from
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
    pub(super) egress_proxy_addr: parking_lot::RwLock<Option<std::net::SocketAddr>>,
    /// The scope commit latch and roots-received flag, modeled as an explicit
    /// state machine (SPEC R23). Owns the one-shot lock semantics and the memory
    /// ordering that the commit decision must not be reordered past the scopes
    /// write (SPEC R5.1 / R5.1.1 / R5.2.2).
    pub(super) scope_lock: super::scope_lock::ScopeLock,
    /// The user's `[sandbox] container_root`, when it is what this session's
    /// scope was derived from (SPEC R5.2.3). `None` for every other scope
    /// source — an explicit scope and a client-reported root are already the
    /// project, so there is nothing to narrow.
    ///
    /// Its presence is what arms [`narrow_container_to`](Self::narrow_container_to).
    pub(super) container_root: Option<PathBuf>,
    /// `Some` once the container has been narrowed, holding the child that won.
    /// Narrowing happens at most once per session: the second call is a no-op,
    /// so a later tool call naming a *different* project cannot re-point the
    /// writable scope (R5.1.1 — one commit, never re-derived).
    pub(super) narrowed_to: parking_lot::RwLock<Option<PathBuf>>,
    /// `Some(host)` when this process is itself inside a macOS Seatbelt profile
    /// that refuses to nest ahma's own (SPEC R7.6). Commands then spawn bare —
    /// still inside the outer kernel boundary — and every scope surface reports
    /// `DeferredToHost`. Decided once, at construction, from the kernel's own
    /// answer plus a refused nesting probe; never from environment markers
    /// alone. Always `None` off macOS: Landlock and Job Objects nest fine.
    pub(super) outer_sandbox: Option<super::host_detect::HostSandbox>,
}

/// Outcome of a scope commit ([`Sandbox::commit_scopes`] /
/// [`Sandbox::commit_existing_scopes`]).
///
/// `AlreadyCommitted` is a **tolerated no-op**, not an error (SPEC R10.5): real
/// clients re-emit `roots/list_changed` routinely, and the immutability of the
/// commit — not session teardown — is what prevents scope widening.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum ScopeCommit {
    /// This call won the one-shot latch; the scopes are now locked.
    Applied,
    /// The scope was already committed; nothing was changed.
    AlreadyCommitted,
}

/// What [`Sandbox::narrow_container_to`] did, when it did something.
///
/// Returned rather than logged because narrowing is a scope decision, and SPEC
/// R5.4 forbids communicating one only through an internal log line — the caller
/// is expected to put this in front of the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerNarrowing {
    /// The container that was granted, now read-only.
    pub container: PathBuf,
    /// The single immediate child that is now the writable scope.
    pub child: PathBuf,
}

impl ContainerNarrowing {
    /// The disclosure to append to the tool result that triggered the narrowing.
    pub fn notice(&self) -> String {
        format!(
            "\n\nNote: this session's writable sandbox scope has been narrowed to `{child}` \
             for the rest of the session. It started as your configured container root \
             `{container}`, which spans every project under it; the first tool call naming a \
             path selected the project. The rest of the container stays readable but is no \
             longer writable, and this choice cannot be changed without restarting — a later \
             call naming a different project will be denied, not re-scoped.",
            child = self.child.display(),
            container = self.container.display(),
        )
    }
}

impl Clone for Sandbox {
    fn clone(&self) -> Self {
        Self {
            scopes: parking_lot::RwLock::new(self.scopes.read().clone()),
            read_scopes: parking_lot::RwLock::new(self.read_scopes.read().clone()),
            mode: self.mode,
            no_temp_files: self.no_temp_files,
            tmp_access: self.tmp_access,
            scratch_dir: self.scratch_dir.clone(),
            persistent_write_scopes: self.persistent_write_scopes.clone(),
            persistent_read_scopes: self.persistent_read_scopes.clone(),
            explicit_scopes: self.explicit_scopes,
            livelog: self.livelog,
            package_cache_write: self.package_cache_write,
            egress_proxy_addr: parking_lot::RwLock::new(*self.egress_proxy_addr.read()),
            scope_lock: self.scope_lock.clone(),
            container_root: self.container_root.clone(),
            narrowed_to: parking_lot::RwLock::new(self.narrowed_to.read().clone()),
            outer_sandbox: self.outer_sandbox,
        }
    }
}

impl std::fmt::Debug for Sandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sandbox")
            .field("scopes", &self.scopes.read())
            .field("read_scopes", &self.read_scopes.read())
            .field("mode", &self.mode)
            .field("no_temp_files", &self.no_temp_files)
            .field("tmp_access", &self.tmp_access)
            .field("scratch_dir", &self.scratch_dir)
            .field("persistent_write_scopes", &self.persistent_write_scopes)
            .field("persistent_read_scopes", &self.persistent_read_scopes)
            .field("explicit_scopes", &self.explicit_scopes)
            .field("livelog", &self.livelog)
            .field("package_cache_write", &self.package_cache_write)
            .field("scope_lock", &self.scope_lock)
            .field("container_root", &self.container_root)
            .field("narrowed_to", &self.narrowed_to.read())
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

        // SPEC R7.6: decide *here*, once, whether this process can apply its own
        // Seatbelt profile at all. Doing it at the first spawn instead would
        // surface as an opaque `sandbox_apply: Operation not permitted` from
        // the child — the failure that hid this for the in-process test suite,
        // which never goes through the server's startup probe.
        let outer_sandbox = super::prerequisites::nested_seatbelt_denial();
        if let Some(host) = outer_sandbox {
            tracing::warn!(
                "{} (SPEC R7.6: macOS refuses to nest a Seatbelt profile inside one that \
                 denies anything; ahma's own scope{} is validated in-process but not \
                 kernel-enforced — the outer sandbox's boundary is.)",
                ActiveSandbox::DeferredToHost(host).disclosure_line(),
                super::error::format_scopes(&canonicalized)
            );
        }

        Ok(Self {
            scopes: parking_lot::RwLock::new(canonicalized),
            read_scopes: parking_lot::RwLock::new(read_scopes),
            mode,
            no_temp_files,
            tmp_access,
            scratch_dir: None,
            persistent_write_scopes: Vec::new(),
            persistent_read_scopes: Vec::new(),
            explicit_scopes: false,
            livelog,
            package_cache_write: true,
            egress_proxy_addr: parking_lot::RwLock::new(None),
            // A fresh sandbox has received nothing from any client, so
            // `scope_source()` never reports `roots/list` for a scope that
            // never saw roots.
            scope_lock: super::scope_lock::ScopeLock::new(),
            container_root: None,
            narrowed_to: parking_lot::RwLock::new(None),
            outer_sandbox,
        })
    }

    /// The outer sandbox this instance defers to because the kernel refuses to
    /// nest ahma's own inside it (SPEC R7.6), or `None` when ahma enforces.
    pub fn deferred_to_outer_sandbox(&self) -> Option<super::host_detect::HostSandbox> {
        self.outer_sandbox
    }

    /// Which sandbox is actually protecting this instance's spawns (SPEC R5.4,
    /// R7): the deferral verdict when there is one, else the host-detection /
    /// confinement probes. Every surface that discloses the sandbox state must
    /// derive it from here so logs, clients, and the TUI agree.
    pub fn active_sandbox(&self) -> ActiveSandbox {
        match self.outer_sandbox {
            Some(host) => ActiveSandbox::DeferredToHost(host),
            None => ActiveSandbox::observe(self.is_enforced()),
        }
    }

    /// Install the guarded egress-proxy address for R-NET enforcement (macOS
    /// Seatbelt). Called once at server startup after the proxy binds. `None`
    /// disables the network-deny rule (restriction off).
    pub fn set_egress_proxy_addr(&self, addr: Option<std::net::SocketAddr>) {
        let mut guard = self.egress_proxy_addr.write();
        *guard = addr;
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

    /// Set a persistent secondary scratch scope (`[sandbox] scratch_directory`) that is
    /// re-appended after every `update_scopes` call so it survives `roots/list`
    /// replacements.  The path must already be canonicalized by the caller.
    #[must_use]
    pub fn with_scratch_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.scratch_dir = dir;
        self
    }

    /// Return the persistent secondary scratch directory, if one was configured.
    pub fn scratch_dir(&self) -> Option<&PathBuf> {
        self.scratch_dir.as_ref()
    }

    /// Register user-granted persistent scopes (from `[sandbox].persistent_scopes`).
    ///
    /// `write` paths join the writable scope set; `read` paths join the read-only
    /// set. Both are folded into the live scopes immediately — so the very first
    /// platform enforcement (Landlock at startup / Seatbelt per-command) already
    /// includes them — and stored so [`Self::commit_scopes`]
    /// re-appends them after every `roots/list` replacement. Paths must already be
    /// canonicalized by the caller.
    #[must_use]
    pub fn with_persistent_scopes(mut self, write: Vec<PathBuf>, read: Vec<PathBuf>) -> Self {
        if !write.is_empty() {
            append_missing_scopes(&mut self.scopes.write(), &write);
        }
        if !read.is_empty() {
            append_missing_scopes(&mut self.read_scopes.write(), &read);
        }
        self.persistent_write_scopes = write;
        self.persistent_read_scopes = read;
        self
    }

    /// Add a live scope grant immediately to the active session.
    ///
    /// Called when a human or elicitation confirms a `sandbox_grant`.
    /// Updates the in-memory scopes so subsequent tool executions in this session
    /// take effect immediately without requiring a full server restart.
    pub fn add_live_grant(&self, path: &Path, access: ahma_common::config::ScopeAccess) {
        let canon = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if access.is_write() {
            append_missing_scopes(&mut self.scopes.write(), &[canon]);
        } else {
            append_missing_scopes(&mut self.read_scopes.write(), &[canon]);
        }
    }

    /// Commit the sandbox scope by **replacing** the provisional scopes with
    /// `scopes` — the single door through which a scope becomes locked (SPEC
    /// R5.1.1).
    ///
    /// The one-shot commit latch and the scope mutation are one operation, so
    /// scope immutability holds *by construction*: there is no public way to
    /// replace the scopes of an already-committed sandbox. A call that loses the
    /// latch returns [`ScopeCommit::AlreadyCommitted`] without touching the
    /// locked scope — the tolerated no-op R10.5 requires.
    ///
    /// Fail-closed on error: if canonicalization/validation of `scopes` fails
    /// *after* the latch was claimed, the latch stays consumed and the sandbox
    /// keeps its previous (narrower, possibly empty) scopes. A retry cannot
    /// widen a scope whose commit already failed; the session surfaces the
    /// failure instead (`notifications/sandbox/failed`).
    pub fn commit_scopes(&self, scopes: Vec<PathBuf>) -> Result<ScopeCommit> {
        if !self.scope_lock.try_commit() {
            return Ok(ScopeCommit::AlreadyCommitted);
        }
        self.apply_scopes(scopes)?;
        Ok(ScopeCommit::Applied)
    }

    /// Commit the sandbox scope **as it currently stands** (pre-configured /
    /// explicit scopes), without replacing anything.
    ///
    /// This is the commit path for scopes that were already seeded at
    /// construction — an explicit `--sandbox-scope`, a task vault, or the user's
    /// container root — where there is nothing to replace, only a latch to
    /// claim. Returns [`ScopeCommit::AlreadyCommitted`] when the latch was
    /// already claimed.
    pub fn commit_existing_scopes(&self) -> ScopeCommit {
        if self.scope_lock.try_commit() {
            ScopeCommit::Applied
        } else {
            ScopeCommit::AlreadyCommitted
        }
    }

    /// Replace the sandbox scopes, preserving the temp directory if `--tmp` was
    /// set, the scratch dir if `--scratch` was set, and any user-granted
    /// persistent scopes (`[sandbox].persistent_scopes`).
    ///
    /// Private on purpose: every wholesale scope replacement must go through
    /// [`commit_scopes`](Self::commit_scopes), which claims the one-shot commit
    /// latch first. The only post-commit mutation is
    /// [`narrow_container_to`](Self::narrow_container_to), which can only shrink.
    fn apply_scopes(&self, scopes: Vec<PathBuf>) -> Result<()> {
        let mut canonicalized = scopes::canonicalize_scopes(
            scopes,
            self.mode,
            "Client must provide valid workspace roots.",
        )?;

        // Re-append the scratch dir so roots/list replacements don't discard it.
        if let Some(ref dir) = self.scratch_dir
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
            let mut current_read_scopes = self.read_scopes.write();
            // The livelog read set is recomputed from the new write scopes, so it
            // replaces the prior value wholesale — re-add granted read-only scopes
            // afterwards so they too survive the roots/list update.
            if self.livelog && self.mode != SandboxMode::Test {
                *current_read_scopes = resolve_livelog_scopes(&canonicalized);
            }
            append_missing_scopes(&mut current_read_scopes, &self.persistent_read_scopes);
        }

        let mut current_scopes = self.scopes.write();
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

    /// Record that this session's scope came from the user's container root
    /// (SPEC R5.2.3), arming auto-narrowing (R5.2.6).
    ///
    /// `root` must already be canonicalized and must be one of the live scopes;
    /// callers get it from the same resolution that seeded the scope.
    #[must_use]
    pub fn with_container_root(mut self, root: Option<PathBuf>) -> Self {
        self.container_root = root;
        self
    }

    /// The container root this session's scope was derived from, if any.
    pub fn container_root(&self) -> Option<&PathBuf> {
        self.container_root.as_ref()
    }

    /// The child the container was narrowed to, once it has been.
    pub fn narrowed_to(&self) -> Option<PathBuf> {
        self.narrowed_to.read().clone()
    }

    /// Narrow a container-root scope to the one project subtree `requested`
    /// lives in (SPEC R5.2.6). Returns `Some` only on the call that actually
    /// narrows, so the caller can disclose it exactly once.
    ///
    /// **Why a derived path is allowed to decide this.** `requested` comes from
    /// tool-call input and is therefore attacker-influenceable, which R5.2.1
    /// otherwise forbids as a scope signal. It is safe here because it can only
    /// ever *narrow*: the container is a directory the user already authorized,
    /// and every candidate this function accepts is a subtree of it. A derived
    /// signal still may not establish or widen a scope — only choose within one
    /// already granted.
    ///
    /// **Why narrowing matters at all.** A container that matches how people
    /// actually work (`~/github`) spans every repository they own. Left whole, an
    /// injected prompt could drop a `.git/hooks/post-checkout` into an unrelated
    /// project — persistence, not merely data loss. Narrowing bounds that to one
    /// project while leaving the rest of the container readable, which is what
    /// keeps cross-project reference lookups working.
    ///
    /// Returns `None` — deliberately, not an error — when there is nothing to do:
    /// no container root, already narrowed, or `requested` is outside the
    /// container. The last case is not this function's to reject; it is an
    /// ordinary out-of-scope path and [`validate_path`](Self::validate_path)
    /// gives it the error message it deserves.
    pub fn narrow_container_to(&self, requested: &Path) -> Option<ContainerNarrowing> {
        let container = self.container_root.clone()?;
        // Narrowing is armed only while the container is genuinely the scope
        // source. When the client later reported usable roots (or the scope is
        // explicit), the live scope is already a project the user/client chose —
        // there is no container in the writable set to narrow, and rewriting a
        // roots-derived scope entry with a joined child name would corrupt it.
        if self.scope_source() != ScopeSource::Container {
            return None;
        }
        // Cheap pre-check outside the write lock; re-checked under it below.
        if self.narrowed_to.read().is_some() {
            return None;
        }

        let child_name = container_child_name(&container, requested)?;
        let child = container.join(&child_name);

        let mut narrowed = self.narrowed_to.write();
        // Two concurrent first tool calls both pass the pre-check; the one that
        // gets here second must not re-point the scope (R5.1.1).
        if narrowed.is_some() {
            return None;
        }

        {
            let mut scopes = self.scopes.write();
            // `canonicalize_scopes` deliberately keeps both the canonical path
            // and its pre-symlink alias (`/private/var/...` and `/var/...` on
            // macOS) so either spelling validates. Narrowing has to replace
            // *every* spelling of the container with the same spelling of the
            // child, or the alias would silently keep the whole container
            // writable. The match is on **the container itself** (any spelling),
            // not "anything under it": a persistent grant that happens to live
            // inside the container is a user-chosen scope of its own and must
            // survive untouched, not be rewritten with a joined child name.
            let (container_aliases, mut kept): (Vec<PathBuf>, Vec<PathBuf>) =
                scopes.drain(..).partition(|s| {
                    scopes::resolve_for_comparison(s) == scopes::resolve_for_comparison(&container)
                });
            let mut narrowed_scopes: Vec<PathBuf> = container_aliases
                .into_iter()
                .map(|alias| alias.join(&child_name))
                .collect();
            narrowed_scopes.append(&mut kept);
            *scopes = narrowed_scopes;
        }
        {
            let mut reads = self.read_scopes.write();
            if !reads.contains(&container) {
                reads.push(container.clone());
            }
        }
        *narrowed = Some(child.clone());

        tracing::info!(
            container = %container.display(),
            child = %child.display(),
            "Narrowed the container-root scope to the project subtree in use (SPEC R5.2.6); \
             the remainder of the container is now read-only"
        );
        Some(ContainerNarrowing { container, child })
    }

    /// Provenance of this session's scope (SPEC R5.2 precedence, rendered per
    /// R5.4).
    ///
    /// One derivation, on the type that owns the flags, so no two surfaces can
    /// disagree about *why* the scope is what it is. It used to be open-coded in
    /// both the `notifications/sandbox/configured` emitter and the shell handler
    /// that refuses to substitute a default scope — a model comparing the
    /// notification against an error body would have seen the drift first.
    ///
    /// `roots_received()` is set only when *usable* client roots were parsed and
    /// applied, so a client that answers `roots/list` with an empty array
    /// (Antigravity, Cursor with no folder open — SPEC R5.2.7) does **not**
    /// masquerade as `roots/list` provenance: the scope it actually runs under
    /// (the container root) is what gets reported, which is also what arms the
    /// R5.2.8 working-directory refusal.
    ///
    /// `Unestablished` is the honest answer for a scope that has no provenance
    /// yet — nothing explicit, no usable roots, no container. `Elicited` and
    /// `PendingTui` are not derivable from these flags; a caller holding richer
    /// provenance should render those instead.
    pub fn scope_source(&self) -> ScopeSource {
        if self.has_explicit_scopes() {
            ScopeSource::Explicit
        } else if self.roots_received() {
            ScopeSource::RootsList
        } else if self.container_root.is_some() {
            ScopeSource::Container
        } else {
            ScopeSource::Unestablished
        }
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
    /// In test mode, tool calls are always allowed. In normal modes the scope
    /// must be **committed** (SPEC R5.1.2.1) — a non-empty *provisional* scope is
    /// not enough. The old `!scopes().is_empty()` check let a call through while
    /// negotiation was still running, on the theory that the provisional scope is
    /// a subset of the committed one. That is false for the one provisional
    /// source that matters: a container root spans every project the user owns
    /// and only *narrows* after commit, so running against it early was running
    /// against a *wider* scope than the one about to be locked.
    pub fn is_ready_for_tool_calls(&self) -> bool {
        self.is_test_mode() || self.is_committed()
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
        ScopesGuard(self.scopes.read())
    }

    /// Get the read-only scopes (for --livelog symlink targets).
    pub fn read_scopes(&self) -> Vec<PathBuf> {
        self.read_scopes.read().clone()
    }

    /// Whether kernel enforcement is active. `--no-sandbox` maps to
    /// [`SandboxMode::Test`], in which scope is resolved but never enforced.
    pub fn is_enforced(&self) -> bool {
        !self.is_test_mode() && self.outer_sandbox.is_none()
    }

    /// Canonical human-readable scope summary with provenance (SPEC R5.4).
    /// Every surface that shows scope renders through this one path.
    pub fn scope_text(&self, source: ScopeSource) -> String {
        let writes = self.scopes.read().clone();
        let reads = self.read_scopes.read().clone();
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
        let writes = self.scopes.read().clone();
        let reads = self.read_scopes.read().clone();
        let mut v = ScopeView {
            write_scopes: &writes,
            read_scopes: &reads,
            tmp_access: self.tmp_access,
            enforced: self.is_enforced(),
            source,
        }
        .to_json();

        // SPEC R5.4: carry the active-sandbox state (including whether ahma is
        // nested inside a host sandbox) so clients like the TUI can render the
        // effective posture and its remediation without a separate query.
        let active = self.active_sandbox();
        if let serde_json::Value::Object(map) = &mut v {
            map.insert("active".into(), serde_json::json!(active.token()));
            map.insert(
                "active_disclosure".into(),
                serde_json::json!(active.disclosure_line()),
            );
            if let Some(host) = active.host_label() {
                map.insert("host".into(), serde_json::json!(host));
            }
        }
        v
    }

    /// Check if a path is within any of the sandbox scopes.
    ///
    /// In `SandboxMode::Test` (`--no-sandbox`) scope enforcement is disabled; the
    /// path is still resolved to canonical form but never rejected.
    pub fn validate_path(&self, path: &Path) -> Result<PathBuf> {
        if self.is_test_mode() {
            return Ok(self.resolve_path_unchecked(path));
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

    /// Resolve `path` to canonical form **without any scope check** — the
    /// `SandboxMode::Test` (`--no-sandbox`) half of [`Self::validate_path`],
    /// where scope is still resolved but never enforced.
    ///
    /// A relative path is joined onto the first scope, or onto the current
    /// directory when there is no scope at all; unlike [`Self::resolve_path`]
    /// (the enforcing path) a missing scope is a fallback here, not an error.
    /// Never call this from an enforcing code path: it cannot reject anything.
    fn resolve_path_unchecked(&self, path: &Path) -> PathBuf {
        let full_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            let base = self
                .scopes()
                .first()
                .cloned()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            base.join(path)
        };

        dunce::canonicalize(&full_path).unwrap_or_else(|_| canonicalize_with_fallback(&full_path))
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
        path_within_scopes(canonical, scopes_guard)
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
/// Whether an **already-canonical** path sits inside any of `scopes`.
///
/// The single spelling of scope membership, shared by [`Sandbox::validate_path`]
/// / [`Sandbox::is_path_in_scope`] and by
/// [`super::exec_config::grantable_git_dirs`], which needs the same answer
/// without holding a `Sandbox`. Callers that start from a user-supplied path
/// must canonicalize first — this does no I/O.
pub(super) fn path_within_scopes(canonical: &Path, scopes: &[PathBuf]) -> bool {
    let canonical_stripped = strip_extended_prefix(canonical);
    scopes
        .iter()
        .any(|scope| canonical_stripped.starts_with(strip_extended_prefix(scope)))
}

fn strip_extended_prefix(path: &Path) -> std::borrow::Cow<'_, Path> {
    #[cfg(target_os = "windows")]
    if let Some(stripped) = path.as_os_str().to_string_lossy().strip_prefix(r"\\?\") {
        return std::borrow::Cow::Owned(PathBuf::from(stripped));
    }
    std::borrow::Cow::Borrowed(path)
}

#[cfg(test)]
mod persistent_scope_tests {
    use super::*;
    use tempfile::tempdir;

    /// The core guarantee behind `ahma sandbox grant`: a granted external
    /// directory survives a client `roots/list` that otherwise replaces the
    /// workspace scope wholesale. Without the re-append in `commit_scopes`, the
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
        // `commit_scopes` is the only door: replacement and the one-shot commit
        // latch are a single operation (SPEC R5.1.1).
        assert_eq!(
            sb.commit_scopes(vec![client_root.path().to_path_buf()])
                .unwrap(),
            ScopeCommit::Applied
        );

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
        // R5.4: the active-sandbox posture is carried for clients (the TUI).
        // Not enforcing + (typically) no host detected in the test env => disabled.
        assert!(
            json.get("active").and_then(|v| v.as_str()).is_some(),
            "scope_json must carry an `active` token: {json}"
        );
        assert!(
            json.get("active_disclosure")
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty()),
            "scope_json must carry a non-empty `active_disclosure`: {json}"
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
        let text = sb.scope_text(ScopeSource::Container);
        assert!(
            text.contains("source: container"),
            "missing source line:\n{text}"
        );
        assert!(text.contains("Sandbox:"), "missing header:\n{text}");
    }

    #[test]
    fn sandbox_tmp_access_getter() {
        let dir = tempdir().unwrap();
        let sb = Sandbox::new(
            vec![dir.path().to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            true,
        )
        .unwrap();
        assert!(sb.is_tmp_access());
    }
}
