//! # Ahma Server CLI
//!
//! This module contains the command-line interface definition and main entry point.
//!
//! ## CLI Design
//!
//! `ahma` uses a subcommand model (git/docker style):
//!
//! ```text
//! ahma serve stdio [--tools rust,python,git]
//! ahma serve http  [--port 3000] [--host 127.0.0.1] [--disable-quic] [--disable-http1-1]
//! ahma tool run <TOOL> [-- <TOOL_ARGS>...]
//! ahma tool validate [TARGET]
//! ahma tool list [--server NAME] [--http URL] [--format json|text] [--mcp-config PATH]
//! ahma tool info [--tools rust,git] [--format json|text] [TOOL]
//! ahma hooks install [--platform claude,codex] [--scope user|project]
//! ahma update [REF] [--force] [--dry-run] [--install-dir PATH]
//! ahma verify [PATH] [--self]
//! ```
//!
//! Niche options that rarely need changing are controlled via environment variables.
//! See `docs/environment-variables.md` for the full reference.

mod commands;

use super::{list_tools, modes, resolution};

use crate::{
    sandbox,
    utils::logging::{detect_log_role_from_startup, init_logging_with_observability, set_log_role},
};
use ahma_common::config::MutexGroupConfig;
use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use dunce;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

// ─────────────────────────────────────────────────────────────────────────────
// Retirement warning macros (R-CFG1.2 / R-CFG2.3)
// ─────────────────────────────────────────────────────────────────────────────

/// Emit a startup `WARN` when any `AHMA_*` env var is set.
/// The value is **NOT** read or honored — all `AHMA_*` vars are retired.
/// Users should migrate to CLI flags or `~/.ahma/settings.toml`.
macro_rules! warn_retired_env {
    ($name:expr) => {
        if std::env::var_os($name).is_some() {
            tracing::warn!(concat!(
                "AHMA env var ",
                $name,
                " is set but IGNORED (retired per R-CFG1.2). ",
                "Use the equivalent CLI flag or ~/.ahma/settings.toml instead."
            ));
        }
    };
}

// Alias kept for call sites that previously used the security-specific name.
macro_rules! warn_retired_security_env {
    ($name:expr) => {
        warn_retired_env!($name);
    };
}

// ─────────────────────────────────────────────────────────────────────────────
// AppConfig — single immutable application configuration
//
// Built once from CLI args + env vars, then passed as a shared reference.
// Never mutated after startup.
// ─────────────────────────────────────────────────────────────────────────────

/// Unified, immutable application configuration.
///
/// Constructed once at startup from CLI flags and environment variables.
/// All subsystems receive `Arc<AppConfig>` or `&AppConfig`; nothing reads
/// the CLI or env vars again after this point.
#[derive(Debug, Clone)]
pub struct AppConfig {
    // ── Tool loading ────────────────────────────────────────────────────────
    /// Path to the `.ahma/` tools directory (auto-detected or from AHMA_TOOLS_DIR).
    pub tools_dir: Option<PathBuf>,
    /// Whether `tools_dir` was explicitly set (vs auto-detected).
    pub explicit_tools_dir: bool,
    /// Tool bundles to activate (e.g. ["rust", "python"]).
    pub tool_bundles: Vec<String>,

    // ── Execution ───────────────────────────────────────────────────────────
    /// Default command timeout in seconds (default 600). Override with the
    /// `--timeout` CLI flag or `tools.timeout_secs` in settings.toml; individual
    /// tools can override via `timeout_seconds` in their JSON definition.
    pub timeout_secs: u64,
    /// Run all tools synchronously (AHMA_SYNC=1).
    pub force_sync: bool,
    /// Reload tools from disk when `.ahma/` changes (AHMA_HOT_RELOAD=1).
    pub hot_reload_tools: bool,
    /// Skip tool availability probes at startup (AHMA_SKIP_PROBES=1).
    pub skip_availability_probes: bool,
    /// Enable output compression and token minimization (AHMA_MINIMIZE_TOKENS=1).
    pub minimize_tokens: bool,
    /// Enable small-model harness adaptations (AHMA_SMALL_MODEL_HARNESS=1).
    pub small_model_harness: bool,
    /// Command serialisation mutex groups (from settings.toml `[tools].mutex_groups`).
    /// Each group gates commands whose first token matches one of `prefixes`,
    /// serialising them per working directory.
    pub mutex_groups: Vec<MutexGroupConfig>,
    /// Use `target/ahma/` instead of `target/` for ahma-spawned cargo builds.
    /// Eliminates cross-process contention with the IDE's background `cargo check`.
    pub separate_cargo_target: bool,

    // ── Sandbox ─────────────────────────────────────────────────────────────
    /// Disable the kernel sandbox entirely (AHMA_DISABLE_SANDBOX=1).
    pub no_sandbox: bool,
    /// Explicit sandbox scope directories (from --sandbox-scope).
    pub sandbox_scopes: Vec<PathBuf>,
    /// Defer sandbox lock until client provides roots/list (AHMA_SANDBOX_DEFER=1).
    pub defer_sandbox: bool,
    /// Working directories seeded when defer mode lacks client roots.
    pub working_dirs: Vec<PathBuf>,
    /// Default scratch directory for sandbox scope fallback.
    /// Auto-created if it does not exist.  Used when no explicit scopes are
    /// provided and the cwd is a filesystem root.
    pub sandbox_directory: Option<PathBuf>,
    /// Add the sandbox_directory (~/sandbox by default) as a persistent secondary
    /// scope that survives roots/list updates.  Set by --sandbox.
    pub use_sandbox_dir: bool,
    /// Add system temp dir to sandbox scopes (AHMA_TMP_ACCESS=1).
    pub tmp_access: bool,
    /// Block writes to temp directories (AHMA_DISABLE_TEMP=1).
    pub no_temp_files: bool,
    /// Enable live-log monitoring mode (AHMA_LOG_MONITOR=1).
    pub log_monitor: bool,
    /// Minimum seconds between log-monitor alerts (AHMA_MONITOR_RATE_LIMIT, default 60).
    pub monitor_rate_limit_secs: u64,
    /// Allow package-manager caches (cargo registry/git) to be written inside
    /// the sandbox (default `true`; disable with `AHMA_NO_PACKAGE_CACHE_WRITE=1`).
    pub package_cache_write: bool,
    /// User-granted external directories (from `[sandbox].persistent_scopes`) that
    /// survive `roots/list` replacement — the persistence behind `ahma sandbox
    /// grant`. Resolved into the sandbox's writable/read-only scope sets.
    pub persistent_scopes: Vec<ahma_common::config::PersistentScope>,
    /// Opt in to auto-granting detected external build caches (sccache/ccache)
    /// to the sandbox scope before lock (from `[sandbox].trust_build_caches`).
    /// When off (default), such caches are only detected and logged, never
    /// auto-granted. See [`sandbox::build_cache`].
    pub trust_build_caches: bool,

    // ── HTTP serve mode ─────────────────────────────────────────────────────
    /// Bind host for HTTP mode (default 127.0.0.1).
    pub http_host: String,
    /// Bind port for HTTP mode (default 3000).
    pub http_port: u16,
    /// Disable HTTP/3 QUIC (AHMA_DISABLE_QUIC=1).
    pub no_quic: bool,
    /// Require HTTP/2+ only (AHMA_DISABLE_HTTP1_1=1).
    pub disable_http1_1: bool,
    /// Handshake timeout for HTTP mode in seconds (AHMA_HANDSHAKE_TIMEOUT, default 45).
    pub handshake_timeout_secs: u64,
    /// Unix domain socket path for `serve unix` mode (AHMA_UNIX_SOCKET).
    /// Empty string means unix socket mode is not active.
    pub unix_socket_path: String,

    // ── Observability ────────────────────────────────────────────────────────
    /// Resolved observability / OTEL configuration (CLI + OTEL_* env vars).
    pub observability: ahma_common::observability::ObservabilityConfig,

    // ── tool list subcommand ─────────────────────────────────────────────────
    /// Server name from mcp.json (for `tool list`).
    pub list_server: Option<String>,
    /// Path to mcp.json (for `tool list`).
    pub mcp_config: PathBuf,
    /// HTTP URL to query (for `tool list`).
    pub list_http: Option<String>,
    /// Output format (for `tool list`).
    pub list_format: list_tools::OutputFormat,

    // ── run subcommand ───────────────────────────────────────────────────────
    /// Tool name for `run` subcommand (also used for positional args in that context).
    pub run_tool: Option<String>,
    /// Arguments forwarded to the tool after `--`.
    pub run_tool_args: Vec<String>,

    // ── task vault ───────────────────────────────────────────────────────────
    /// Task vault root to use as sandbox scope (--task-vault <path>).
    /// When set, the sandbox scope is set to <vault>/workdir/ and an audit
    /// log is initialized at <vault>/audit.jsonl.
    pub task_vault: Option<PathBuf>,

    // ── HTTP authentication / rate limiting / daemon ────────────────────────
    /// Required token for HTTP access (AHMA_REQUIRE_TOKEN).
    pub require_token: Option<String>,
    /// Path to a file containing the required token (AHMA_REQUIRE_TOKEN_PATH).
    pub require_token_path: Option<PathBuf>,
    /// Rate limit requests per second (AHMA_RATE_LIMIT_RPS).
    pub rate_limit_rps: u64,
    /// Rate limit burst allowance (AHMA_RATE_LIMIT_BURST).
    pub rate_limit_burst: u32,
    /// Daemon instance label (AHMA_INSTANCE_LABEL).
    pub instance_label: String,
    /// Idle timeout in seconds before the background bridge shuts down (default 10).
    pub idle_timeout_secs: Option<u64>,
    /// Maximum concurrent HTTP server sessions.
    pub max_sessions: usize,
    /// True when this process was launched as a child server subprocess (--server-child flag or AHMA_SERVER_CHILD env var).
    /// Prevents the child from itself trying to spawn a background bridge and become a proxy client.
    pub is_server_child: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            tools_dir: None,
            explicit_tools_dir: false,
            tool_bundles: vec![],
            timeout_secs: 600,
            force_sync: false,
            hot_reload_tools: false,
            skip_availability_probes: false,
            minimize_tokens: false,
            small_model_harness: false,
            mutex_groups: ahma_common::config::default_mutex_groups(),
            separate_cargo_target: false,

            no_sandbox: false,
            sandbox_scopes: vec![],
            defer_sandbox: false,
            working_dirs: vec![],
            sandbox_directory: Some(PathBuf::from("~/sandbox")),
            use_sandbox_dir: false,
            tmp_access: false,
            no_temp_files: false,
            log_monitor: false,
            monitor_rate_limit_secs: 60,
            package_cache_write: true,
            persistent_scopes: vec![],
            trust_build_caches: false,
            http_host: "127.0.0.1".to_string(),
            http_port: 3000,
            no_quic: false,
            disable_http1_1: false,
            handshake_timeout_secs: 45,
            unix_socket_path: String::new(),
            observability: ahma_common::observability::ObservabilityConfig::default(),
            list_server: None,
            mcp_config: PathBuf::from("mcp.json"),
            list_http: None,
            list_format: list_tools::OutputFormat::Text,
            run_tool: None,
            run_tool_args: vec![],
            task_vault: None,
            require_token: None,
            require_token_path: None,
            rate_limit_rps: 0,
            rate_limit_burst: 10,
            instance_label: "ahma".to_string(),
            idle_timeout_secs: None,
            max_sessions: 10,
            is_server_child: false,
        }
    }
}

impl AppConfig {
    /// Read a boolean env var ("1","true","yes","on" → true; anything else → false).
    pub fn env_flag(name: &str) -> bool {
        std::env::var(name)
            .map(|v| {
                let t = v.trim().to_ascii_lowercase();
                matches!(t.as_str(), "1" | "true" | "yes" | "on")
            })
            .unwrap_or(false)
    }
}

fn expand_tilde(path: PathBuf) -> PathBuf {
    let path_str = path.to_string_lossy();
    if path_str == "~"
        && let Some(home) = dirs::home_dir()
    {
        return home;
    } else if (path_str.starts_with("~/") || path_str.starts_with("~\\"))
        && let Some(home) = dirs::home_dir()
    {
        let mut expanded = home;
        expanded.push(&path_str[2..]);
        return expanded;
    }
    path
}

// ─────────────────────────────────────────────────────────────────────────────
// Sandbox policy helpers
// ─────────────────────────────────────────────────────────────────────────────

struct SandboxPolicy {
    no_sandbox: bool,
    tmp_access: bool,
    mode: sandbox::SandboxMode,
}

fn resolve_sandbox_policy(cfg: &AppConfig) -> SandboxPolicy {
    let no_sandbox = cfg.no_sandbox;
    let tmp_access = cfg.tmp_access;

    let mode = if no_sandbox {
        tracing::warn!("Ahma sandbox disabled via --no-sandbox flag");
        #[cfg(target_os = "linux")]
        if let Err(error) = sandbox::check_sandbox_prerequisites() {
            tracing::warn!(
                "Continuing without Ahma sandbox because Linux sandbox prerequisites are unavailable: {}. \
                 Update Linux kernel to 5.13+ to enable Landlock.",
                error
            );
        }
        sandbox::SandboxMode::Test
    } else {
        sandbox::SandboxMode::Strict
    };

    SandboxPolicy {
        no_sandbox,
        tmp_access,
        mode,
    }
}

fn check_sandbox_availability(no_sandbox: bool) -> Result<()> {
    if no_sandbox {
        return Ok(());
    }

    if let Err(e) = sandbox::check_sandbox_prerequisites() {
        sandbox::exit_with_sandbox_error(&e);
    }

    #[cfg(target_os = "macos")]
    {
        if let Err(e) = sandbox::test_sandbox_exec_available() {
            sandbox::exit_with_sandbox_error(&e);
        }
    }

    Ok(())
}

fn canonicalize_paths(paths: &[PathBuf], context: &str) -> Result<Vec<PathBuf>> {
    paths
        .iter()
        .map(|p| {
            dunce::canonicalize(p)
                .with_context(|| format!("Failed to canonicalize {}: {:?}", context, p))
        })
        .collect()
}

fn ensure_task_vault_layout(task_vault_root: &Path) -> Result<PathBuf> {
    let workdir = task_vault_root.join("workdir");
    let inputs = task_vault_root.join("inputs");
    let outputs = task_vault_root.join("outputs");
    let trash = task_vault_root.join("trash");
    let audit_log = task_vault_root.join("audit.jsonl");

    std::fs::create_dir_all(&inputs).with_context(|| {
        format!(
            "Failed to create task vault inputs dir: {}",
            inputs.display()
        )
    })?;
    std::fs::create_dir_all(&workdir)
        .with_context(|| format!("Failed to create task vault workdir: {}", workdir.display()))?;
    std::fs::create_dir_all(&outputs).with_context(|| {
        format!(
            "Failed to create task vault outputs dir: {}",
            outputs.display()
        )
    })?;
    std::fs::create_dir_all(&trash)
        .with_context(|| format!("Failed to create task vault trash dir: {}", trash.display()))?;
    if !audit_log.exists() {
        std::fs::write(&audit_log, b"").with_context(|| {
            format!(
                "Failed to initialize task vault audit log: {}",
                audit_log.display()
            )
        })?;
    }

    dunce::canonicalize(&workdir).with_context(|| {
        format!(
            "Failed to canonicalize task vault workdir: {}",
            workdir.display()
        )
    })
}

fn resolve_sandbox_scopes(cfg: &AppConfig) -> Result<Option<Vec<PathBuf>>> {
    if let Some(task_vault_root) = &cfg.task_vault {
        let workdir = ensure_task_vault_layout(task_vault_root)?;
        tracing::info!(
            "Task vault mode active: root={}, sandbox_scope={}",
            task_vault_root.display(),
            workdir.display()
        );
        let trash = task_vault_root.join("trash");
        let audit = task_vault_root.join("audit.jsonl");
        let trash_canonical = dunce::canonicalize(&trash).unwrap_or(trash);
        let audit_canonical = dunce::canonicalize(&audit).unwrap_or(audit);
        return Ok(Some(vec![workdir, trash_canonical, audit_canonical]));
    }

    if cfg.defer_sandbox {
        return resolve_deferred_scopes(cfg);
    }

    if !cfg.sandbox_scopes.is_empty() {
        let mut scopes = Vec::with_capacity(cfg.sandbox_scopes.len());
        for scope in &cfg.sandbox_scopes {
            let canonical = ahma_common::config::ensure_sandbox_directory(scope)
                .with_context(|| format!("Failed to initialize sandbox scope: {:?}", scope))?;
            if !scopes.contains(&canonical) {
                scopes.push(canonical);
            }
        }
        return Ok(Some(scopes));
    }

    // SPEC R5.2.1: the launch CWD is NEVER inferred as a sandbox scope — not via
    // project-marker files, not as a "provisional" root. A spoofable, ambient
    // signal must not decide what the AI may write to. With no explicit scope
    // (handled above) the scope source is, in order: the client's roots/list, a
    // user elicitation answer, or the declared default `~/sandbox` (R5.2).
    //
    // Here at startup we have not yet talked to the client, so we seed the
    // declared default when one is configured (the common case — `--sandbox` and
    // the built-in `sandbox_directory` default both provide it), otherwise an
    // empty provisional scope so the server awaits roots/list. Either way the
    // result is shown with its provenance (R5.4); nothing is silent.
    if let Some(sandbox_dir) = &cfg.sandbox_directory {
        let canonical = ahma_common::config::ensure_sandbox_directory(sandbox_dir)
            .context("Failed to initialize default sandbox directory")?;
        tracing::info!(
            "No explicit scope; using default sandbox directory (SPEC R5.2.3): {}",
            canonical.display()
        );
        return Ok(Some(vec![canonical]));
    }

    tracing::warn!(
        "No explicit scope and no sandbox_directory configured; awaiting client roots/list. \
         Tool calls return HTTP 409 until roots arrive. For clients that do not send roots \
         (e.g. Antigravity, LM Studio), add --sandbox-scope <path> or configure \
         [sandbox] sandbox_directory in ~/.ahma/settings.toml (default ~/sandbox)."
    );
    Ok(Some(Vec::new()))
}

fn resolve_deferred_scopes(cfg: &AppConfig) -> Result<Option<Vec<PathBuf>>> {
    if !cfg.working_dirs.is_empty() {
        let scopes = canonicalize_paths(&cfg.working_dirs, "working directory")?;
        tracing::info!("Sandbox initialized from AHMA_WORKING_DIRS: {:?}", scopes);
        return Ok(Some(scopes));
    }

    // An explicit `--sandbox-scope` is a valid provisional fallback in defer mode:
    // it lets clients that never send `roots/list` (Claude Desktop, Antigravity,
    // LM Studio) still lock a sandbox instead of failing the handshake. The HTTP
    // bridge forwards its `default_scope` to the subprocess exactly this way
    // (`--sandbox-scope <path> --defer-sandbox`, see ahma_http_bridge peer.rs).
    // Without this, the subprocess starts with zero scopes; when the client
    // returns -32601 to `roots/list`, config_watcher has no pre-configured scope
    // to fall back to and emits `notifications/sandbox/failed`, poisoning the
    // session so every `tools/call` returns HTTP 409 forever. Seeded scopes still
    // yield to client-provided roots (config_watcher prefers parsed roots), so
    // this stays provisional and does not widen any locked scope.
    if !cfg.sandbox_scopes.is_empty() {
        let mut scopes = Vec::with_capacity(cfg.sandbox_scopes.len());
        for scope in &cfg.sandbox_scopes {
            let canonical = ahma_common::config::ensure_sandbox_directory(scope)
                .with_context(|| format!("Failed to initialize sandbox scope: {:?}", scope))?;
            if !scopes.contains(&canonical) {
                scopes.push(canonical);
            }
        }
        tracing::info!(
            "Deferred sandbox seeded from --sandbox-scope fallback (no client roots required): {:?}",
            scopes
        );
        return Ok(Some(scopes));
    }

    tracing::warn!(
        "Sandbox initialization deferred with ZERO scopes — tool calls will return \
         HTTP 409 until the client sends roots/list or roots/list_changed. \
         If your client does not send roots (e.g. Antigravity), add \
         --sandbox-scope <path> or --working-directories <path> to prevent this."
    );
    Ok(Some(Vec::new()))
}

fn add_temp_scope_if_requested(
    scopes: Option<Vec<PathBuf>>,
    tmp_access: bool,
) -> Option<Vec<PathBuf>> {
    if !tmp_access {
        return scopes;
    }

    let mut scopes = scopes?;

    // The temp scope is auxiliary: it must only ever *augment* a real workspace
    // scope, never stand in as the sole sandbox root. If the scope set is empty
    // (deferred sandbox, or awaiting client `roots/list`), do NOT seed it with
    // the temp dir — doing so would make the sandbox appear "already configured"
    // and lock it to the temp directory, rejecting the actual workspace. The
    // temp dir is re-added by `Sandbox::update_scopes` once real roots arrive.
    if scopes.is_empty() {
        tracing::debug!(
            "Skipping temp scope: no workspace scope yet (temp is re-added once \
             client roots/list resolves)"
        );
        return Some(scopes);
    }

    let temp_dir = std::env::temp_dir();
    match dunce::canonicalize(&temp_dir) {
        Ok(canonical_temp) if !scopes.contains(&canonical_temp) => {
            tracing::info!(
                "Adding temp directory to sandbox scopes via AHMA_TMP_ACCESS: {:?}",
                canonical_temp
            );
            scopes.push(canonical_temp);
        }
        Ok(_) => {}
        Err(_) => {
            tracing::warn!(
                "Could not canonicalize temp directory {:?}, skipping AHMA_TMP_ACCESS scope addition",
                temp_dir
            );
        }
    }

    Some(scopes)
}

/// Resolve `[sandbox].persistent_scopes` into canonical (writable, read-only)
/// path lists for [`sandbox::Sandbox::with_persistent_scopes`].
///
/// Writable (`rw`) grants are auto-created if missing (a fresh build cache is
/// expected not to exist yet); read-only (`ro`) grants are never created — a
/// missing one is logged and skipped rather than silently materialised.
fn resolve_persistent_scopes(cfg: &AppConfig) -> (Vec<PathBuf>, Vec<PathBuf>) {
    use ahma_common::config::ScopeAccess;
    let mut write = Vec::new();
    let mut read = Vec::new();
    for scope in &cfg.persistent_scopes {
        let raw = expand_tilde(scope.path.clone());
        match scope.access {
            ScopeAccess::Rw => match ahma_common::config::ensure_sandbox_directory(&scope.path) {
                Ok(canonical) => {
                    if !write.contains(&canonical) {
                        tracing::info!(
                            "Granted persistent scope (read+write): {} [{}]",
                            canonical.display(),
                            scope.granted_by.as_deref().unwrap_or("user")
                        );
                        write.push(canonical);
                    }
                }
                Err(e) => tracing::warn!(
                    "Skipping persistent scope {}: could not create/canonicalize ({e})",
                    raw.display()
                ),
            },
            ScopeAccess::Ro => match dunce::canonicalize(&raw) {
                Ok(canonical) => {
                    if !read.contains(&canonical) {
                        tracing::info!(
                            "Granted persistent scope (read-only): {} [{}]",
                            canonical.display(),
                            scope.granted_by.as_deref().unwrap_or("user")
                        );
                        read.push(canonical);
                    }
                }
                Err(e) => tracing::warn!(
                    "Skipping read-only persistent scope {} (does not exist or unreadable): {e}",
                    raw.display()
                ),
            },
        }
    }
    (write, read)
}

/// P1a pre-lock build-cache consent.
///
/// Detect external build caches active in this environment (sccache/ccache).
/// When the user has opted in via `sandbox.trust_build_caches`, fold each
/// detected cache directory into `write_scopes` (the writable persistent scopes
/// that are applied before the sandbox locks and survive `roots/list`), so a
/// sandboxed `cargo` build can use the cache for the whole session. When **not**
/// opted in, emit an actionable hint per cache and grant nothing — the sandbox is
/// never widened without explicit consent (secure by default).
///
/// Pure aside from logging + directory creation: detection is environment-driven
/// and the merge is exercised directly in tests via the public seam.
fn merge_trusted_build_caches(cfg: &AppConfig, write_scopes: &mut Vec<PathBuf>) {
    let caches = sandbox::build_cache::detect();
    apply_build_cache_consent(cfg.trust_build_caches, &caches, write_scopes);
}

/// Testable core of [`merge_trusted_build_caches`]: apply the opt-in decision to
/// a concrete list of detected caches. When `trust`, each cache directory is
/// created-if-missing, canonicalized, and appended to `write_scopes` (deduped);
/// otherwise each is only logged with the command to allow it.
fn apply_build_cache_consent(
    trust: bool,
    caches: &[sandbox::build_cache::BuildCache],
    write_scopes: &mut Vec<PathBuf>,
) {
    for cache in caches {
        if trust {
            match ahma_common::config::ensure_sandbox_directory(&cache.dir) {
                Ok(canonical) => {
                    if !write_scopes.contains(&canonical) {
                        tracing::info!(
                            "Trusted build cache granted (read+write): {} [{}]",
                            canonical.display(),
                            cache.tool
                        );
                        write_scopes.push(canonical);
                    }
                }
                Err(e) => tracing::warn!(
                    "Could not grant {} build cache {}: {e}",
                    cache.tool,
                    cache.dir.display()
                ),
            }
        } else {
            tracing::warn!(
                "Detected {} cache at {} outside the sandbox scope; sandboxed builds may \
                 fail (denied cache access / provenance contamination). To allow it for \
                 this and future sessions, set `sandbox.trust_build_caches = true` in \
                 ~/.ahma/settings.toml (or run `ahma sandbox grant {}`), then restart.",
                cache.tool,
                cache.dir.display(),
                cache.dir.display()
            );
        }
    }
}

fn create_sandbox_instance(
    scopes: Option<Vec<PathBuf>>,
    policy: &SandboxPolicy,
    cfg: &AppConfig,
) -> Result<Option<Arc<sandbox::Sandbox>>> {
    let Some(scopes) = scopes else {
        return Ok(None);
    };

    // SPEC R5.5: scopes are "explicit" when the user named them directly via
    // --sandbox-scope, --working-directories, or a task vault. Those must never
    // be widened/replaced via roots/list. Scopes derived implicitly (CWD
    // fallback, --tmp) are provisional and yield to client-provided roots.
    let explicit_scopes =
        cfg.task_vault.is_some() || !cfg.sandbox_scopes.is_empty() || !cfg.working_dirs.is_empty();

    // When --sandbox is set, canonicalize ~/sandbox and record it as the
    // persistent secondary scope that survives every roots/list update.
    let sandbox_dir = if cfg.use_sandbox_dir {
        if let Some(dir) = &cfg.sandbox_directory {
            match ahma_common::config::ensure_sandbox_directory(dir) {
                Ok(canonical) => {
                    tracing::info!(
                        "Persistent sandbox directory (--sandbox): {}",
                        canonical.display()
                    );
                    Some(canonical)
                }
                Err(e) => {
                    tracing::warn!("Failed to create sandbox directory {:?}: {}", dir, e);
                    None
                }
            }
        } else {
            tracing::warn!("--sandbox set but no sandbox_directory configured; ignoring");
            None
        }
    } else {
        None
    };

    // User-granted persistent scopes (e.g. an sccache cache outside the workspace).
    // Folded into the sandbox now (so initial enforcement covers them) and
    // re-applied on every roots/list update so a client's workspace root cannot
    // silently drop them.
    let (mut persistent_write_scopes, persistent_read_scopes) = resolve_persistent_scopes(cfg);

    // P1a: pre-lock build-cache consent. Detect external build caches (sccache/
    // ccache) and, only when the user has opted in, fold them into the writable
    // persistent scopes *before* the sandbox locks — so cached builds work all
    // session without a per-failure grant + restart. Without consent this only
    // logs an actionable hint; it never widens the sandbox on its own.
    merge_trusted_build_caches(cfg, &mut persistent_write_scopes);

    let s = sandbox::Sandbox::new(
        scopes.clone(),
        policy.mode,
        cfg.no_temp_files,
        cfg.log_monitor,
        policy.tmp_access,
    )
    .context("Failed to initialize sandbox")?
    .with_explicit_scopes(explicit_scopes)
    .with_sandbox_dir(sandbox_dir)
    .with_persistent_scopes(persistent_write_scopes, persistent_read_scopes)
    .with_package_cache_write(cfg.package_cache_write)
    .with_separate_cargo_target(cfg.separate_cargo_target);

    tracing::info!("Sandbox scopes initialized: {:?}", scopes);

    apply_platform_sandbox_enforcement(&s, policy, cfg)?;

    Ok(Some(Arc::new(s)))
}

fn apply_platform_sandbox_enforcement(
    sandbox: &sandbox::Sandbox,
    policy: &SandboxPolicy,
    cfg: &AppConfig,
) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if policy.mode == sandbox::SandboxMode::Strict
            && !cfg.defer_sandbox
            && let Err(e) = sandbox::enforce_landlock_sandbox(
                &sandbox.scopes(),
                &sandbox.read_scopes(),
                sandbox.is_no_temp_files(),
                sandbox.package_cache_write(),
            )
        {
            tracing::error!("Failed to enforce Landlock sandbox: {}", e);
            return Err(e);
        }
    }

    #[cfg(target_os = "windows")]
    {
        let _ = (policy, cfg);
        if let Err(e) = sandbox::enforce_windows_sandbox(&sandbox.scopes()) {
            tracing::warn!("Windows Job Object enforcement failed: {}", e);
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let _ = (sandbox, policy, cfg);

    Ok(())
}

fn log_sandbox_mode(no_sandbox: bool) {
    if no_sandbox {
        tracing::info!("🔓 Sandbox mode: DISABLED (commands run without Ahma sandboxing)");
        return;
    }

    #[cfg(target_os = "linux")]
    tracing::info!("SECURE Sandbox mode: LANDLOCK (Linux kernel-level file system restrictions)");

    #[cfg(target_os = "macos")]
    tracing::info!("SECURE Sandbox mode: SEATBELT (macOS sandbox-exec per-command restrictions)");

    #[cfg(target_os = "windows")]
    tracing::info!(
        "SECURE Sandbox mode: JOB OBJECT (kill-on-close process tracking); \
         AppContainer spawn isolation pending"
    );

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    tracing::info!(
        "SECURE Sandbox mode: UNSUPPORTED ON THIS OS (startup fails closed in strict mode)"
    );
}

#[cfg(target_os = "windows")]
fn check_powershell_available() {
    let ps_check = std::process::Command::new("powershell")
        .arg("-NoProfile")
        .arg("-Command")
        .arg("$PSVersionTable.PSVersion.ToString()")
        .output();
    match ps_check {
        Ok(out) if out.status.success() => {
            let ver = String::from_utf8_lossy(&out.stdout);
            tracing::info!("PowerShell detected: {}", ver.trim());
        }
        _ => {
            eprintln!(
                "\nFAIL Error: PowerShell was not found.\n\n\
                 ahma_mcp requires PowerShell (built into Windows 10/11) as its runtime shell.\n"
            );
            std::process::exit(1);
        }
    }
}

async fn dispatch_serve(serve_args: ServeArgs, cfg: AppConfig) -> Result<()> {
    match serve_args.transport {
        Some(ServeTransport::Stdio(stdio_args)) => {
            let mut cfg = cfg;
            if let Some(path) = stdio_args.path {
                cfg.sandbox_scopes.push(path);
            }
            let sandbox = initialize_sandbox(&cfg)?;
            let sandbox =
                sandbox.ok_or_else(|| anyhow!("Sandbox failed to initialize for stdio mode"))?;
            check_stdio_not_interactive()?;
            tracing::info!("Running in STDIO server mode");
            modes::run_server_mode(cfg, sandbox).await
        }
        Some(ServeTransport::Http(_)) => {
            tracing::info!("Running in HTTP bridge mode");
            modes::run_http_bridge_mode(cfg).await
        }
        #[cfg(unix)]
        Some(ServeTransport::Unix(u)) => {
            let path = u.socket_path.as_deref().unwrap_or("/tmp/ahma.sock");
            tracing::info!("Running in Unix socket bridge mode on {}", path);
            modes::run_unix_bridge_mode(cfg).await
        }
        None => {
            #[cfg(unix)]
            {
                tracing::info!("Running in Unix socket bridge mode on /tmp/ahma.sock");
                modes::run_unix_bridge_mode(cfg).await
            }
            #[cfg(not(unix))]
            {
                tracing::info!("Running in HTTP bridge mode");
                modes::run_http_bridge_mode(cfg).await
            }
        }
    }
}

async fn dispatch_tool(tool_cmd: ToolArgs, cfg: AppConfig) -> Result<()> {
    match tool_cmd.command {
        ToolCommand::Validate(v) => {
            tracing::info!("Running in validate mode");
            run_validation_mode(&v.target.unwrap_or_else(|| ".ahma".to_string()))
        }
        ToolCommand::List(_) => {
            tracing::info!("Running in list-tools mode");
            modes::run_list_tools_mode(&cfg).await
        }
        ToolCommand::Run(run_args) => {
            let cfg = AppConfig {
                run_tool: Some(run_args.tool),
                run_tool_args: run_args.tool_args,
                ..cfg
            };
            let sandbox = initialize_sandbox(&cfg)?;
            let sandbox = sandbox
                .ok_or_else(|| anyhow!("Sandbox scopes must be initialized for run mode"))?;
            tracing::info!("Running in CLI mode");
            modes::run_cli_mode(cfg, sandbox).await
        }
        ToolCommand::Info(info_args) => {
            tracing::info!("Running in tool-info mode");
            run_tool_info_mode(info_args).await
        }
    }
}

pub async fn dispatch_subcommand(cmd: Subcommands, cfg: AppConfig) -> Result<()> {
    match cmd {
        Subcommands::Serve(serve_args) => dispatch_serve(serve_args, cfg).await,
        Subcommands::Tool(tool_cmd) => dispatch_tool(tool_cmd, cfg).await,
        Subcommands::Vault(_) => {
            anyhow::bail!(
                "vault commands are provided by the ahma_bin crate (includes ahma_vault). \
                 If you are running a custom binary, implement vault dispatch using ahma_vault::TaskVault."
            )
        }
        Subcommands::Tui(_) => {
            anyhow::bail!(
                "tui is provided by the ahma_bin crate (includes ahma_tui). \
                 If you are running a custom binary, implement TUI dispatch using ahma_tui::run_tui."
            )
        }
        Subcommands::Tls(_) => {
            anyhow::bail!(
                "tls commands are provided by the ahma_bin crate (includes ahma_common::local_tls). \
                 If you are running a custom binary, implement TLS dispatch using ahma_common::local_tls."
            )
        }
        Subcommands::Bundle(bundle_args) => dispatch_bundle_command(bundle_args),
        Subcommands::Llm(_) => {
            anyhow::bail!(
                "llm commands are provided by the ahma_bin crate (includes ahma_common). \
                 If you are running a custom binary, implement llm dispatch using ahma_common::config::AhmaConfig."
            )
        }
        Subcommands::Hooks(args) => {
            tracing::info!("Running in hooks mode");
            crate::hooks::run(args, cfg).await
        }
        Subcommands::Cluster(_) => {
            anyhow::bail!(
                "cluster commands are provided by the ahma_bin crate (includes ahma_cluster). \
                 If you are running a custom binary, implement cluster dispatch using ahma_cluster::discovery::WorkerRegistry."
            )
        }
        #[cfg(feature = "simplify")]
        Subcommands::Simplify(args) => {
            tracing::info!("Running in simplify mode");
            crate::simplify::run(args)
        }
        Subcommands::Update(args) => {
            tracing::info!("Running in update mode");
            crate::update::run(args, &cfg).await
        }
        Subcommands::Verify(args) => {
            tracing::info!("Running in verify mode");
            crate::update::verify::run_cli(args).await
        }
        Subcommands::Setup(args) => {
            tracing::info!("Running in setup mode");
            crate::setup::run(args).await
        }
        Subcommands::Uninstall(args) => {
            tracing::info!("Running in uninstall mode");
            crate::uninstall::run(args).await
        }
        Subcommands::Daemon(_) => {
            anyhow::bail!(
                "daemon is provided by the ahma_bin crate. \
                 If you are running a custom binary, implement daemon dispatch \
                 using ahma_common::daemon_hub::run_daemon."
            )
        }
        Subcommands::Settings(args) => {
            tracing::info!("Running in settings mode");
            run_settings_command(args)
        }
        Subcommands::Prompts(args) => {
            tracing::info!("Running in prompts mode");
            run_prompts_command(args)
        }
        Subcommands::Sandbox(args) => {
            tracing::info!("Running in sandbox-scope management mode");
            commands::run_sandbox_command(args)
        }
    }
}

fn run_settings_command(args: SettingsArgs) -> Result<()> {
    commands::run_settings_command(args)
}

fn run_prompts_command(args: PromptsArgs) -> Result<()> {
    commands::run_prompts_command(args)
}

fn dispatch_bundle_command(args: BundleArgs) -> Result<()> {
    commands::dispatch_bundle_command(args)
}

fn check_stdio_not_interactive() -> Result<()> {
    commands::check_stdio_not_interactive()
}

// ─────────────────────────────────────────────────────────────────────────────
// CLI argument types (clap)
// ─────────────────────────────────────────────────────────────────────────────

/// Ahma MCP: A secure, config-driven adapter for CLI tools.
///
/// User-configurable defaults live in `~/.ahma/settings.toml`.
/// Run `ahma settings init` to create it with all defaults commented out.
/// Use `ahma --no-settings` to ignore the settings file for one invocation.
#[derive(Parser, Debug)]
#[command(
    name = "ahma",
    author,
    version,
    about = "Ahma MCP: secure, config-driven adapter for CLI tools"
)]
pub struct Cli {
    /// Emit the full CLI reference as Markdown and exit. Pipe into a file to
    /// regenerate `docs/cli-reference.md` via `ahma --markdown-help > docs/cli-reference.md`.
    #[arg(long, global = true, hide = true)]
    pub markdown_help: bool,

    /// Ignore `~/.ahma/settings.toml` for this invocation. All settings fall
    /// back to compiled-in defaults and any deprecated `AHMA_*` env vars.
    #[arg(long, global = true)]
    pub no_settings: bool,

    /// Path to settings file instead of `~/.ahma/settings.toml`. Useful for
    /// testing or per-project settings. Ignored when `--no-settings` is supplied.
    #[arg(long, global = true, value_name = "PATH")]
    pub settings_path: Option<PathBuf>,

    /// Tool bundles to enable (e.g. --tools rust --tools python,git).
    /// Repeat or comma-separate. Available: rust, python, git, kotlin, fileutils, github, simplify.
    #[arg(
        long = "tools",
        value_name = "NAME",
        value_delimiter = ',',
        global = true
    )]
    pub tool_bundles: Vec<String>,

    /// Path to the tools directory containing JSON tool definitions.
    /// Defaults to the auto-detected .ahma/ in the current directory.
    #[arg(long = "tools-dir", global = true)]
    pub tools_dir: Option<PathBuf>,

    /// Add the system temp directory to the sandbox scope.
    /// Useful for workflows that need scratch space (compilers, build systems).
    #[arg(long = "tmp", global = true)]
    pub tmp: bool,

    /// Add the sandbox_directory (default ~/sandbox) as a persistent secondary scope.
    /// The directory is created if it does not exist and survives roots/list updates,
    /// giving the AI a stable per-user scratch space regardless of which workspace is open.
    /// Use --sandbox-scope to specify an explicit primary scope instead.
    #[arg(long = "sandbox", global = true)]
    pub use_sandbox: bool,

    /// Enable live log monitoring. Ahma tails the configured log stream through
    /// an LLM to detect issues in real time and push alerts as MCP progress
    /// notifications.
    #[arg(long = "log-monitor", global = true)]
    pub log_monitor: bool,

    /// Minimum seconds between successive log-monitor alerts.
    /// Prevents alert storms when a persistent issue triggers repeated matches.
    #[arg(long = "monitor-rate-limit", value_name = "SECS", global = true)]
    pub monitor_rate_limit: Option<u64>,

    /// Idle timeout in seconds for background servers.
    /// If there are no active clients for this duration, the server exits.
    #[arg(long = "idle-timeout", value_name = "SECS", global = true)]
    pub idle_timeout: Option<u64>,

    /// Disable the kernel sandbox entirely.
    /// UNSAFE: the AI can read and write anywhere on the filesystem.
    /// Use only in environments that provide their own containment (Docker, CI containers).
    #[arg(long = "no-sandbox", global = true)]
    pub no_sandbox: bool,

    /// Enable output compression and token minimization.
    #[arg(long = "minimize-tokens", global = true)]
    pub minimize_tokens: bool,

    /// Disable output compression and token minimization
    /// (overrides settings.toml and the deprecated AHMA_MINIMIZE_TOKENS env var).
    #[arg(
        long = "no-minimize-tokens",
        global = true,
        conflicts_with = "minimize_tokens"
    )]
    pub no_minimize_tokens: bool,

    /// Enable small-model harness adaptations: per-turn coaching hints and
    /// tighter context budgets for local models with small context windows.
    #[arg(long = "small-model-harness", global = true)]
    pub small_model_harness: bool,

    /// Disable small-model harness adaptations
    /// (overrides settings.toml and the deprecated AHMA_SMALL_MODEL_HARNESS env var).
    #[arg(
        long = "no-small-model-harness",
        global = true,
        conflicts_with = "small_model_harness"
    )]
    pub no_small_model_harness: bool,

    /// Model context window size in tokens for the TUI's local-LLM chat agent.
    /// Sizes the conversation and tool-result budgets so small models are not
    /// flooded past their window (e.g. --context-length 8192 for an 8k model).
    #[arg(long = "context-length", value_name = "TOKENS", global = true)]
    pub context_length: Option<u32>,

    /// Default tool execution timeout in seconds.
    /// Individual tools can override this via the timeout_seconds field in their JSON definition.
    #[arg(long = "timeout", value_name = "SECS", global = true)]
    pub timeout: Option<u64>,

    /// Force all tools to run synchronously.
    /// By default, tools are async-first: if a result arrives within 5 seconds it is
    /// returned inline; otherwise an operation ID is returned and the result is pushed
    /// as a notification.
    #[arg(long = "sync", global = true)]
    pub sync: bool,

    /// OTLP endpoint for distributed tracing export.
    /// Providing this flag enables tracing.
    #[arg(long = "opentelemetry", value_name = "URL", global = true)]
    pub opentelemetry: Option<String>,

    /// Run this server session inside an existing task vault (a per-task isolated
    /// directory containing inputs, workdir, outputs, trash, and audit logs).
    /// Enforces the "dedicated folder per task" security principle by restricting
    /// the sandbox scope to <vault>/workdir/, initializing an audit log at
    /// <vault>/audit.jsonl, and routing deletions to <vault>/trash/. The vault
    /// must exist (create with `ahma vault create <slug>` first).
    #[arg(long = "task-vault", value_name = "PATH", global = true)]
    pub task_vault: Option<PathBuf>,

    /// Paths allowed for read/write access under the sandbox (e.g. --sandbox-scope /path1 --sandbox-scope /path2).
    /// Repeat or comma-separate.
    #[arg(
        long = "sandbox-scope",
        value_name = "PATH",
        value_delimiter = ',',
        global = true
    )]
    pub sandbox_scopes: Vec<PathBuf>,

    /// Directories containing allowed working directories.
    /// Repeat or comma-separate.
    #[arg(
        long = "working-dir",
        value_name = "PATH",
        value_delimiter = ',',
        global = true
    )]
    pub working_dirs: Vec<PathBuf>,

    /// Defer sandbox lock until the MCP client provides `roots/list`.
    #[arg(long = "defer-sandbox", global = true)]
    pub defer_sandbox: bool,

    /// Block all access to the system temp directory.
    #[arg(long = "disable-temp-files", global = true)]
    pub no_temp_files: bool,

    /// Disable write access to package-manager caches (cargo registry/git, etc.).
    /// By default, ahma grants write access to these subdirs so that agents can
    /// fetch new dependency versions.  Sensitive paths (bin, config.toml,
    /// credentials.toml) are always kept read-only.
    #[arg(long = "no-package-cache-write", global = true)]
    pub no_package_cache_write: bool,

    /// Watch the tools directory for JSON changes and reload tool definitions at runtime.
    #[arg(long = "hot-reload", global = true)]
    pub hot_reload: bool,

    /// Skip tool availability probes at startup.
    #[arg(long = "skip-probes", global = true)]
    pub skip_probes: bool,

    /// MCP handshake timeout in seconds.
    #[arg(long = "handshake-timeout", value_name = "SECS", global = true)]
    pub handshake_timeout: Option<u64>,

    /// Required bearer token for HTTP access.
    #[arg(long = "require-token", value_name = "TOKEN", global = true)]
    pub require_token: Option<String>,

    /// Path to a file containing the required bearer token for HTTP access.
    #[arg(long = "require-token-path", value_name = "PATH", global = true)]
    pub require_token_path: Option<PathBuf>,

    /// Maximum requests per second for HTTP rate limiting.
    #[arg(long = "rate-limit-rps", value_name = "RPS", global = true)]
    pub rate_limit_rps: Option<u64>,

    /// Burst allowance for the rate limiter.
    #[arg(long = "rate-limit-burst", value_name = "BURST", global = true)]
    pub rate_limit_burst: Option<u32>,

    /// Human-readable instance name shown in TUI.
    #[arg(long = "instance-label", value_name = "LABEL", global = true)]
    pub instance_label: Option<String>,

    /// Log to stderr instead of rolling log files.
    #[arg(long = "log-to-stderr", global = true)]
    pub log_to_stderr: bool,

    /// Directory for rolling log files.
    /// Defaults to `<cwd>/logs`, falling back to `~/.ahma/logs`.
    /// Replaces the deprecated AHMA_LOG_DIR environment variable.
    #[arg(long = "log-dir", value_name = "PATH", global = true)]
    pub log_dir: Option<PathBuf>,

    /// Terminal hook behaviour: `on` forces hooks active, `off` disables them,
    /// `auto` (default) activates when an ahma MCP server is configured in an
    /// editor. Takes precedence over the AHMA_HOOKS environment variable
    /// (which remains supported for hook subprocesses).
    #[arg(long = "hooks-mode", value_name = "on|off|auto", global = true)]
    pub hooks_mode: Option<String>,

    /// Directory for local TLS certificates (default: `~/.ahma/tls`).
    /// Replaces the deprecated AHMA_TLS_DIR environment variable.
    #[arg(long = "tls-dir", value_name = "PATH", global = true)]
    pub tls_dir: Option<PathBuf>,

    /// Hub daemon socket path (Unix socket path; `host:port` on Windows).
    /// Replaces the deprecated AHMA_DAEMON_SOCK environment variable.
    #[arg(long = "daemon-socket", value_name = "PATH", global = true)]
    pub daemon_socket: Option<PathBuf>,

    /// Indicate that this process is spawned as a child server subprocess.
    #[arg(long = "server-child", global = true)]
    pub server_child: bool,

    /// Maximum concurrent HTTP server sessions.
    #[arg(long = "max-sessions", value_name = "LIMIT", global = true)]
    pub max_sessions: Option<usize>,

    /// Path to the Unix domain socket.
    #[arg(long = "unix-socket-path", value_name = "PATH", global = true)]
    pub unix_socket_path: Option<String>,

    /// Disable HTTP/3 (QUIC). Serve HTTP/2 over TCP only.
    #[arg(long = "disable-quic", global = true)]
    pub disable_quic: bool,

    /// Require HTTP/2+; reject HTTP/1.1 connections.
    #[arg(long = "disable-http1-1", global = true)]
    pub disable_http1_1: bool,

    #[command(subcommand)]
    pub command: Subcommands,
}

#[derive(Subcommand, Debug)]
pub enum Subcommands {
    /// Start an MCP server (stdio or http).
    Serve(ServeArgs),
    /// Tool management and execution utilities.
    Tool(ToolArgs),
    /// Task vault management: create and inspect per-question working directories.
    Vault(VaultArgs),
    /// Start the TUI control plane (terminal dashboard for active tasks).
    Tui(TuiArgs),
    /// Local TLS certificate management: init, rotate, and check status.
    Tls(TlsArgs),
    /// Bundle management: audit and verify MTDF tool bundles.
    Bundle(BundleArgs),
    /// LLM provider management: add, list, test, and remove named providers.
    Llm(LlmArgs),
    /// Manage terminal hooks for external AI tools.
    Hooks(crate::hooks::HooksArgs),
    /// Cluster peer management: add, list, ping, and inspect worker nodes.
    Cluster(ClusterArgs),
    /// Analyze source code complexity and generate a simplicity report.
    #[cfg(feature = "simplify")]
    Simplify(crate::simplify::SimplifyArgs),
    /// Download or build and install ahma.
    Update(crate::update::UpdateArgs),
    /// Verify an artifact's GitHub Build Provenance Attestation (Sigstore SLSA Level 3).
    Verify(crate::update::verify::VerifyArgs),
    /// Run the interactive or automated setup wizard.
    Setup(SetupArgs),
    /// Remove integrations installed by `ahma setup` (MCP server entries, terminal hooks,
    /// agent skills, and optionally the ahma binary). Mirrors `ahma setup` with the same
    /// "what / which platforms" prompts when no flags are given.
    Uninstall(UninstallArgs),
    /// Start the TUI hub daemon for multi-instance aggregation. The daemon collects
    /// operation events from running ahma instances (including stdio processes spawned
    /// by IDEs) and fans them to TUI subscribers. Starts automatically on first use.
    Daemon(DaemonArgs),
    /// Manage the settings file (`~/.ahma/settings.toml`). The primary place to
    /// configure Ahma behaviour, superseding environment variables and providing a
    /// single, auditable, self-documented source of truth.
    Settings(SettingsArgs),
    /// Manage LLM prompt templates (~/.ahma/prompts.toml).
    Prompts(PromptsArgs),
    /// Grant, list, and revoke persistent sandbox scopes — external directories
    /// (outside the workspace) that a trusted tool needs, e.g. an sccache build
    /// cache. Grants are recorded in `~/.ahma/settings.toml` and survive every
    /// `roots/list` update, so they stay in effect for the whole session.
    Sandbox(SandboxArgs),
}

/// Arguments for `ahma setup`.
#[derive(clap::Args, Debug, Clone)]
pub struct SetupArgs {
    /// Skip prompts and set up automatically with default options.
    #[arg(short = 'y', long = "auto")]
    pub auto: bool,

    /// Only configure MCP servers.
    #[arg(long = "mcp")]
    pub mcp: bool,

    /// Only configure terminal hooks.
    #[arg(long = "hooks")]
    pub hooks: bool,

    /// Only configure agent skills.
    #[arg(long = "skills")]
    pub skills: bool,

    /// Only initialize TLS certificate.
    #[arg(long = "tls")]
    pub tls: bool,
}

/// Arguments for `ahma uninstall`.
#[derive(clap::Args, Debug, Clone)]
#[command(
    about = "Remove integrations installed by ahma setup",
    long_about = "Remove MCP server entries, terminal hooks, agent skills, and/or the ahma \
binary that were installed by `ahma setup` or `scripts/install.sh`.\n\n\
Without flags, runs an interactive wizard (same question flow as `ahma setup`).\n\
With `--auto`, removes everything from all platforms without prompting.",
    after_help = "EXAMPLES:
  # Interactive: pick what to remove and from which platforms
  ahma uninstall

  # Non-interactive: remove everything (MCP, hooks, skills, binary)
  ahma uninstall --auto

  # Remove only MCP server entries from Cursor and Claude Code
  ahma uninstall --mcp --platform cursor,claude

  # Preview changes without writing files
  ahma uninstall --auto --dry-run

  # Full cleanup including ~/.ahma data directory
  ahma uninstall --auto --purge"
)]
pub struct UninstallArgs {
    /// Skip prompts and remove everything automatically.
    #[arg(short = 'y', long = "auto")]
    pub auto: bool,

    /// Remove only MCP server entries.
    #[arg(long = "mcp")]
    pub mcp: bool,

    /// Remove only terminal hooks.
    #[arg(long = "hooks")]
    pub hooks: bool,

    /// Remove only agent skills (and Claude Code plugin).
    #[arg(long = "skills")]
    pub skills: bool,

    /// Remove the ahma binary from the install directory.
    #[arg(long = "binary")]
    pub binary: bool,

    /// Platform(s) to target (comma-separated). Defaults to all supported platforms.
    #[arg(long = "platform", value_delimiter = ',')]
    pub platforms: Vec<String>,

    /// Also purge the `~/.ahma` data directory (settings, logs, TLS, prompts).
    /// Implies removal of the Antigravity `~/sandbox` directory if it was created by setup.
    #[arg(long = "purge")]
    pub purge: bool,

    /// Show planned changes without writing files.
    #[arg(long = "dry-run")]
    pub dry_run: bool,
}

/// Arguments for `ahma daemon`.
///
/// The daemon is configured via `~/.ahma/settings.toml` or the `AHMA_DAEMON_SOCK`
/// environment variable (socket path override only).
#[derive(clap::Args, Debug, Clone)]
pub struct DaemonArgs {}

// ── settings ─────────────────────────────────────────────────────────────────

/// Arguments for `ahma settings`.
#[derive(clap::Args, Debug, Clone)]
pub struct SettingsArgs {
    #[command(subcommand)]
    pub command: SettingsCommand,
}

/// Subcommands for `ahma settings`.
#[derive(Subcommand, Debug, Clone)]
pub enum SettingsCommand {
    /// Write the settings file with all defaults commented out.
    ///
    /// Creates `~/.ahma/settings.toml` with every available option shown as a
    /// comment, making it easy to discover and override defaults.
    ///
    /// Use `--force` to overwrite an existing file.
    /// Use `--path` to write to a custom location.
    Init {
        /// Overwrite the file if it already exists.
        #[arg(long)]
        force: bool,
        /// Write to this path instead of `~/.ahma/settings.toml`.
        #[arg(long, value_name = "PATH")]
        path: Option<PathBuf>,
    },
    /// Print the effective settings resolved from all sources.
    ///
    /// Shows the current value of each setting and where it came from:
    /// settings file, deprecated environment variable, or compiled-in default.
    Show,
}

// ── sandbox scope grants ─────────────────────────────────────────────────────

/// Arguments for `ahma sandbox`.
#[derive(clap::Args, Debug, Clone)]
pub struct SandboxArgs {
    #[command(subcommand)]
    pub command: SandboxCommand,
}

/// Subcommands for `ahma sandbox` — manage persistent sandbox scope grants.
///
/// A *persistent scope* is an external directory (outside the workspace) added
/// to the kernel sandbox that **survives `roots/list` replacement**, so it stays
/// available no matter which workspace the client opens. Grants are stored in
/// `[sandbox].persistent_scopes` in `~/.ahma/settings.toml` — a file that lives
/// outside every sandbox scope and so cannot be edited by a sandboxed tool call.
/// Changes take effect the next time an ahma server starts.
#[derive(Subcommand, Debug, Clone)]
pub enum SandboxCommand {
    /// Grant a directory persistent access in the sandbox.
    ///
    /// Defaults to read+write (most external tool dirs are caches the tool both
    /// reads and writes); pass `--read-only` for read access alone. A writable
    /// directory is created if it does not yet exist.
    ///
    /// Example (allow an sccache cache outside the workspace):
    ///   ahma sandbox grant ~/Library/Caches/Mozilla.sccache --by sccache \
    ///     --note "compiler cache"
    Grant {
        /// Directory to grant. `~` is expanded to your home directory.
        #[arg(value_name = "PATH")]
        path: PathBuf,
        /// Grant read-only access instead of the default read+write.
        #[arg(long = "read-only")]
        read_only: bool,
        /// Record what asked for this scope (e.g. a tool name), for auditing.
        #[arg(long = "by", value_name = "WHO")]
        by: Option<String>,
        /// Free-form note explaining why this scope exists.
        #[arg(long = "note", value_name = "TEXT")]
        note: Option<String>,
    },
    /// List the persistent scopes currently granted, and the file they live in.
    List,
    /// Revoke a previously granted persistent scope.
    Revoke {
        /// Directory to revoke (matched after `~` expansion).
        #[arg(value_name = "PATH")]
        path: PathBuf,
    },
}

// ── prompts ──────────────────────────────────────────────────────────────────

/// Arguments for `ahma prompts`.
#[derive(clap::Args, Debug, Clone)]
pub struct PromptsArgs {
    #[command(subcommand)]
    pub command: PromptsCommand,
}

/// Subcommands for `ahma prompts`.
#[derive(Subcommand, Debug, Clone)]
pub enum PromptsCommand {
    /// Write the prompts file with all defaults commented out.
    ///
    /// Creates `~/.ahma/prompts.toml` (or `./.ahma/prompts.toml` if `--project` is passed)
    /// with every prompt template shown as a comment.
    Init {
        /// Overwrite the file if it already exists.
        #[arg(long)]
        force: bool,
        /// Create a project-local prompts file in `./.ahma/prompts.toml` instead of global.
        #[arg(long)]
        project: bool,
        /// Write to this path instead of the default location.
        #[arg(long, value_name = "PATH")]
        path: Option<PathBuf>,
    },
    /// Print the effective prompts resolved from all sources.
    Show,
    /// Validate prompt templates, verifying required placeholders exist.
    Validate,
    /// Force update the global defaults file (~/.ahma/prompts.toml) with the latest defaults, backing up first.
    Update,
}

// ── serve ────────────────────────────────────────────────────────────────────

/// Start the ahma MCP server.
///
/// Choose a transport that fits your integration:
///
/// * **stdio** — spawned as a subprocess by an MCP client (Cursor, VS Code,
///   Claude Desktop).  The client manages the process lifetime; no network
///   port is opened.  This is the most common mode.
///
/// * **http** — a persistent, multi-session bridge that listens on a TCP
///   port and supports several MCP clients concurrently.  Useful for CI,
///   shared developer machines, or any situation where clients connect over
///   a network rather than spawning a process.
///
/// Tools are loaded from the directory specified by `--tools-dir`, the
/// `AHMA_TOOLS_DIR` environment variable, or the `.ahma/` folder detected
/// in the current working directory (in that order of precedence).
#[derive(Parser, Debug)]
#[command(after_help = "EXAMPLES:
  # Serve over stdio (typical mcp.json entry for Cursor / VS Code)
  ahma serve stdio

  # Serve over stdio and enable the rust + git tool bundles
  ahma serve stdio --tools rust,git

  # Add temp directory to sandbox scope (for compilers / build tools)
  ahma serve stdio --tools rust --tmp

  # Enable live log monitoring with a custom alert rate limit
  ahma serve stdio --log-monitor --monitor-rate-limit 30

  # Extend the default tool timeout to 10 minutes
  ahma serve stdio --timeout 600

  # Force all tools to run synchronously
  ahma serve stdio --sync

  # Disable the kernel sandbox (only in isolated containers)
  ahma serve stdio --no-sandbox

  # Serve over HTTP on the default address (127.0.0.1:3000)
  ahma serve http

  # Serve over HTTP on a custom port with HTTP/3 disabled
  ahma serve http --port 8080 --disable-quic")]
pub struct ServeArgs {
    #[command(subcommand)]
    pub transport: Option<ServeTransport>,
}

#[derive(Subcommand, Debug)]
pub enum ServeTransport {
    /// Serve over stdio — standard transport for MCP clients. Spawns ahma as a
    /// child process communicating over stdin/stdout. No network port is opened;
    /// sandboxing is applied per-session.
    #[command(after_help = "EXAMPLES:
  # Minimal stdio server
  ahma serve stdio

  # Enable specific tool bundles
  ahma serve stdio --tools rust --tools python,git

  # Use a custom tools directory
  ahma serve stdio --tools-dir /path/to/.ahma

  # Allow compilers / build tools access to the temp directory
  ahma serve stdio --tools rust --tmp

  # Enable live log monitoring with reduced alert rate
  ahma serve stdio --log-monitor --monitor-rate-limit 30

  # Extend the default timeout to 10 minutes
  ahma serve stdio --timeout 600

  # Disable sandbox in a Docker container with its own isolation
  ahma serve stdio --no-sandbox")]
    Stdio(StdioArgs),
    /// Serve over HTTP — a persistent bridge. Listens on a TCP port and routes
    /// MCP sessions over HTTP/2 and HTTP/3/QUIC. Multiple clients can connect
    /// concurrently; suitable for CI runners or remote integrations.
    Http(HttpArgs),
    /// Serve over a Unix domain socket (UDS) for local IPC and sidecar proxies.
    /// Listens on a UDS path and routes MCP Streamable HTTP traffic. Filesystem socket
    /// files are removed automatically on graceful shutdown. Not available on Windows.
    #[cfg(unix)]
    #[command(after_help = "EXAMPLES:
  # Filesystem socket (default path)
  ahma serve unix

  # Custom path
  ahma serve unix --socket-path /run/ahma/mcp.sock

  # Linux abstract socket (@ prefix)
  ahma serve unix --socket-path @ahma

  # Or set via environment variable
  AHMA_UNIX_SOCKET=/tmp/ahma.sock ahma serve unix")]
    Unix(UnixArgs),
}

/// Start a persistent HTTP-based MCP bridge.
///
/// Binds a TCP listener and serves the MCP Streamable HTTP transport
/// (2025-03-26 spec).  Each connecting client gets an isolated session
/// with its own sandbox scope.
///
/// HTTP/3 over QUIC is enabled by default when the platform supports it
/// (requires a valid TLS certificate).  Use `--disable-quic` to fall back
/// to HTTP/2 over TCP only.  HTTP/1.1 is accepted by default; use
/// `--disable-http1-1` to require HTTP/2 or better.
///
/// Security: the server binds to `127.0.0.1` by default.  Bind to
/// `0.0.0.0` only in trusted network environments and consider placing
/// a reverse proxy in front for production use.
#[derive(Parser, Debug)]
#[command(after_help = "EXAMPLES:
  # Default: 127.0.0.1:3000, HTTP/2 + HTTP/3
  ahma serve http

  # Custom port, localhost only
  ahma serve http --port 8080

  # Bind on all interfaces (use with care)
  ahma serve http --host 0.0.0.0 --port 3000

  # HTTP/2 over TCP only (disable QUIC/HTTP3)
  ahma serve http --disable-quic

  # Require at least HTTP/2 — reject HTTP/1.1 clients
  ahma serve http --disable-http1-1

  # Extended timeout, temp access, and log monitoring
  ahma serve http --timeout 600 --tmp --log-monitor")]
pub struct HttpArgs {
    /// Host to bind the HTTP server on.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Port to bind the HTTP server on.
    #[arg(long, default_value_t = 3000)]
    pub port: u16,
}

/// Arguments for `ahma serve stdio`.
#[derive(Parser, Debug, Clone)]
pub struct StdioArgs {
    /// Optional path to set as the sandbox scope (defaults to client roots/list if not specified).
    #[arg(index = 1, value_name = "PATH")]
    pub path: Option<PathBuf>,
}

/// Arguments for `ahma serve unix`.
#[cfg(unix)]
#[derive(Parser, Debug)]
pub struct UnixArgs {
    /// Path to the Unix domain socket to create.
    ///
    /// Supports filesystem paths (`/tmp/ahma.sock`) and Linux abstract sockets
    /// using the `@` prefix (`@ahma`).
    ///
    /// Defaults to the value of `AHMA_UNIX_SOCKET`, or `/tmp/ahma.sock`
    /// if neither the flag nor the env var is set.
    #[arg(long = "socket-path")]
    pub socket_path: Option<String>,
}

// ── run ──────────────────────────────────────────────────────────────────────

/// Arguments for `ahma run <TOOL> [-- <TOOL_ARGS>...]`.
#[derive(Parser, Debug)]
pub struct RunArgs {
    /// Name of the tool to execute.
    #[arg(value_name = "TOOL")]
    pub tool: String,

    /// Arguments forwarded to the tool (after --).
    #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
    pub tool_args: Vec<String>,
}

// ── tool ─────────────────────────────────────────────────────────────────────

/// Arguments for `ahma tool`.
#[derive(Parser, Debug)]
pub struct ToolArgs {
    #[command(subcommand)]
    pub command: ToolCommand,
}

#[derive(Subcommand, Debug)]
pub enum ToolCommand {
    /// Validate tool JSON configurations against the MTDF schema.
    Validate(ValidateArgs),
    /// List all tools available from an MCP server.
    List(ListArgs),
    /// Execute a single tool command and print the result. Loads tool definitions,
    /// applies sandboxing, executes the command, and prints stdout/stderr. Useful
    /// for scripting, CI pipelines, and debugging outside MCP.
    #[command(after_help = "EXAMPLES:
  # Run a cargo build in release mode
  ahma tool run cargo_build -- --release

  # Run git status
  ahma tool run git_status

  # Run with a custom tools directory
  AHMA_TOOLS_DIR=/path/to/.ahma ahma tool run my_tool -- --flag value")]
    Run(RunArgs),
    /// Show locally configured tools, built-in bundles, descriptions, and parameters.
    /// Loads definitions from the tools directory and active bundles, then prints a
    /// summary of each tool.
    #[command(after_help = "EXAMPLES:
  # Show all tools from the local .ahma/ directory
  ahma tool info

  # Include built-in bundles
  ahma tool info --tools rust,git

  # JSON output for scripting
  ahma tool info --tools rust --format json

  # Show details for a specific tool
  ahma tool info cargo")]
    Info(InfoArgs),
}

/// Arguments for `ahma tool validate [TARGET]`.
#[derive(Parser, Debug)]
pub struct ValidateArgs {
    /// File, directory, or comma-separated list of paths to validate.
    /// Defaults to `.ahma` in the current directory.
    #[arg(value_name = "TARGET")]
    pub target: Option<String>,
}

/// Arguments for `ahma tool list`.
#[derive(Parser, Debug)]
pub struct ListArgs {
    /// Name of the server in mcp.json to connect to.
    #[arg(long)]
    pub server: Option<String>,

    /// Path to mcp.json configuration file.
    #[arg(long, default_value = "mcp.json")]
    pub mcp_config: PathBuf,

    /// HTTP URL for the MCP server (e.g. http://localhost:3000).
    #[arg(long)]
    pub http: Option<String>,

    /// Output format.
    #[arg(long, value_enum, default_value_t = list_tools::OutputFormat::Text)]
    pub format: list_tools::OutputFormat,

    /// Command and arguments to run a stdio MCP server (after --).
    #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
    pub server_args: Vec<String>,
}

/// Arguments for `ahma tool info`.
#[derive(Parser, Debug)]
pub struct InfoArgs {
    /// Tool bundles to include (e.g. --tools rust --tools python,git).
    /// Repeat or comma-separate. Available: rust, python, git, kotlin, fileutils, github, simplify.
    #[arg(long = "tools", value_name = "NAME", value_delimiter = ',')]
    pub tool_bundles: Vec<String>,

    /// Path to the tools directory containing JSON tool definitions.
    /// Defaults to the auto-detected `.ahma/` in the current directory.
    #[arg(long)]
    pub tools_dir: Option<PathBuf>,

    /// Output format.
    #[arg(long, value_enum, default_value_t = list_tools::OutputFormat::Text)]
    pub format: list_tools::OutputFormat,

    /// Show only a specific tool by name.
    #[arg(value_name = "TOOL")]
    pub filter: Option<String>,
}

// ── vault ─────────────────────────────────────────────────────────────────────

/// Arguments for `ahma vault`.
#[derive(Parser, Debug)]
pub struct VaultArgs {
    #[command(subcommand)]
    pub command: VaultCommand,
}

#[derive(Subcommand, Debug)]
pub enum VaultCommand {
    /// Create a new task vault (a per-task isolated directory tree) for a user question
    /// containing inputs/, workdir/, outputs/, trash/, and audit.jsonl. Prints the root path.
    #[command(after_help = "EXAMPLES:
  ahma vault create summarise-q4-report
  ahma vault create \"analyse customer data\"")]
    Create(VaultCreateArgs),
    /// List all existing task vaults.
    List,
}

/// Arguments for `ahma vault create`.
#[derive(Parser, Debug)]
pub struct VaultCreateArgs {
    /// A short human-readable slug describing the task (becomes part of the directory name).
    #[arg(value_name = "SLUG")]
    pub slug: String,
}

// ── tui ───────────────────────────────────────────────────────────────────────

/// Arguments for `ahma tui`.
#[derive(Parser, Debug)]
#[command(after_help = "EXAMPLES:
  # Auto-detect the best available local transport (Unix socket, then HTTP)
  ahma tui

  # Connect to a specific HTTP bridge
  ahma tui --connect http://localhost:8080

  # Connect via a Unix domain socket
  ahma tui --connect unix:///tmp/ahma.sock")]
pub struct TuiArgs {
    /// URL of the ahma server to monitor.
    ///
    /// When omitted, `ahma tui` probes local transports in order:
    /// Unix socket (default `/tmp/ahma.sock`, or `AHMA_UNIX_SOCKET`) on Unix,
    /// then `http://localhost:3000`.
    ///
    /// Supported URL formats:
    ///   http://host:port        — plain HTTP / HTTP2 / HTTP3
    ///   https://host:port       — HTTPS
    ///   unix:///path/to.sock    — Unix domain socket (Unix only)
    #[arg(long = "connect")]
    pub connect: Option<String>,

    /// Launch directly with a specific agent profile.
    #[arg(long = "profile")]
    pub profile: Option<String>,

    /// Optional path to set as the sandbox scope (defaults to current directory if server is spawned).
    #[arg(index = 1, value_name = "PATH")]
    pub path: Option<PathBuf>,
}

// ── tls ───────────────────────────────────────────────────────────────────────

/// Arguments for `ahma tls`.
#[derive(Parser, Debug)]
#[command(after_help = "EXAMPLES:
  # Generate the initial TLS certificate for local QUIC (first-time setup)
  ahma tls init

  # Regenerate and replace the existing TLS certificate
  ahma tls rotate

  # Show the certificate status (path, age, rotation needed)
  ahma tls status")]
pub struct TlsArgs {
    #[command(subcommand)]
    pub command: TlsCommand,
}

#[derive(Subcommand, Debug)]
pub enum TlsCommand {
    /// Generate the local TLS certificate (first-time setup) under `~/.ahma/tls/`.
    /// The private key is written with mode 0600 on Unix. Safe to re-run.
    Init,
    /// Rotate the local TLS certificate. Deletes the existing certificate and generates
    /// a new self-signed certificate. Use when approaching expiry or compromised.
    Rotate,
    /// Print the local TLS certificate status (path, age, and rotation recommendation).
    Status,
}

// ── bundle ────────────────────────────────────────────────────────────────────

/// Arguments for `ahma bundle`.
#[derive(Parser, Debug)]
pub struct BundleArgs {
    #[command(subcommand)]
    pub command: BundleCommand,
}

#[derive(Subcommand, Debug)]
pub enum BundleCommand {
    /// Audit a bundle directory. Scans all JSON files for secrets, missing path
    /// validation, prompt-injection payloads, and other security risks.
    Audit(BundleAuditArgs),
    /// Verify a bundle directory against its content manifest.
    Verify(BundleVerifyArgs),
    /// Create a content manifest for a bundle directory.
    Sign(BundleSignArgs),
}

/// Arguments for `ahma bundle audit <path>`.
#[derive(Parser, Debug)]
pub struct BundleAuditArgs {
    /// Path to the bundle directory to audit.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,
    /// Exit with a non-zero code if any warnings are found (not just criticals).
    #[arg(long)]
    pub strict: bool,
}

/// Arguments for `ahma bundle verify <path>`.
#[derive(Parser, Debug)]
pub struct BundleVerifyArgs {
    /// Path to the bundle directory to verify.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,
}

/// Arguments for `ahma bundle sign <path>`.
#[derive(Parser, Debug)]
pub struct BundleSignArgs {
    /// Path to the bundle directory to sign.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,
}

// ── llm ───────────────────────────────────────────────────────────────────────

/// Arguments for `ahma llm`.
#[derive(Parser, Debug)]
#[command(after_help = "EXAMPLES:
  ahma llm list
  ahma llm add --name ollama-local --base-url http://localhost:11434/v1 --model llama3.2
  ahma llm add --name openai --base-url https://api.openai.com/v1 --model gpt-4o-mini --api-key '${OPENAI_API_KEY}'
  ahma llm test ollama-local
  ahma llm remove ollama-local")]
pub struct LlmArgs {
    #[command(subcommand)]
    pub command: LlmCommand,
}

#[derive(Subcommand, Debug)]
pub enum LlmCommand {
    /// List all named providers in ~/.ahma/config.toml.
    List,
    /// Add a named provider to ~/.ahma/config.toml.
    Add(LlmAddArgs),
    /// Test connectivity to a named provider (GET /v1/models).
    Test(LlmTestArgs),
    /// Remove a named provider from ~/.ahma/config.toml.
    Remove(LlmRemoveArgs),
}

/// Arguments for `ahma llm add`.
#[derive(Parser, Debug)]
pub struct LlmAddArgs {
    /// Unique name for this provider (e.g. "ollama-local").
    #[arg(long)]
    pub name: String,
    /// Wire-format family: "openai" (default, OpenAI-compatible) or "anthropic"
    /// (native Messages API).
    #[arg(long, default_value = "openai")]
    pub kind: String,
    /// Base URL of the API (OpenAI-compatible root, or https://api.anthropic.com/v1).
    #[arg(long)]
    pub base_url: String,
    /// Default model to use with this provider (e.g. "llama3.2", "claude-opus-4-8").
    #[arg(long)]
    pub model: String,
    /// Optional API key. Use \${ENV_VAR} notation to reference an environment variable.
    #[arg(long)]
    pub api_key: Option<String>,
}

/// Arguments for `ahma llm test`.
#[derive(Parser, Debug)]
pub struct LlmTestArgs {
    /// Name of the provider to test (must exist in ~/.ahma/config.toml).
    #[arg(value_name = "NAME")]
    pub name: String,
}

/// Arguments for `ahma llm remove`.
#[derive(Parser, Debug)]
pub struct LlmRemoveArgs {
    /// Name of the provider to remove.
    #[arg(value_name = "NAME")]
    pub name: String,
}

// ── cluster ───────────────────────────────────────────────────────────────────

/// Arguments for `ahma cluster`.
#[derive(Parser, Debug)]
#[command(after_help = "EXAMPLES:
  ahma cluster list
  ahma cluster add-peer --id workstation --addr http://workstation.local:3000 --models llama3.2,gemma4
  ahma cluster ping workstation
  ahma cluster status
  ahma cluster discover
  ahma cluster announce --port 3000 --models llama3.2,gemma4
  ahma cluster cert init --out-dir ~/.ahma/cluster/certs")]
pub struct ClusterArgs {
    /// Directory containing mTLS certificates (`ca.pem`, `cert.pem`, `key.pem`)
    /// generated by `ahma cluster cert init`. When set, outbound connections use mTLS.
    #[arg(long, value_name = "DIR")]
    pub tls_dir: Option<std::path::PathBuf>,

    #[command(subcommand)]
    pub command: ClusterCommand,
}

#[derive(Subcommand, Debug)]
pub enum ClusterCommand {
    /// List peers in ~/.ahma/cluster/peers.json.
    List,
    /// Add a worker peer to ~/.ahma/cluster/peers.json.
    #[command(name = "add-peer")]
    AddPeer(ClusterAddPeerArgs),
    /// Ping a peer's /health endpoint.
    Ping(ClusterPingArgs),
    /// Remove a worker peer from ~/.ahma/cluster/peers.json.
    Remove(ClusterRemoveArgs),
    /// Show status of all configured peers (reachability + capabilities).
    Status,
    /// Browse the local network for ahma worker peers via mDNS and print what
    /// is found within the discovery window.
    Discover,
    /// Announce this machine as an ahma worker peer via mDNS so remote peers
    /// can discover it automatically.
    Announce(ClusterAnnounceArgs),
    /// Manage mTLS certificates for cluster peer authentication.
    #[command(subcommand)]
    Cert(CertCommand),
}

/// Arguments for `ahma cluster announce`.
#[derive(Parser, Debug)]
pub struct ClusterAnnounceArgs {
    /// Unique peer ID broadcast in the mDNS TXT record (defaults to hostname).
    #[arg(long)]
    pub id: Option<String>,
    /// HTTP port the local ahma bridge is listening on.
    #[arg(long, default_value = "3000")]
    pub port: u16,
    /// Models available on this peer (comma-separated).
    #[arg(long, value_delimiter = ',', default_value = "")]
    pub models: Vec<String>,
}

/// Sub-commands for `ahma cluster cert`.
#[derive(Subcommand, Debug)]
pub enum CertCommand {
    /// Generate a self-signed CA, leaf certificate, and key under `--out-dir`.
    /// Share `ca.pem` with all other peers to allow mutual verification.
    Init {
        /// Directory where the generated PEM files are written.
        #[arg(long, default_value = "~/.ahma/cluster/certs")]
        out_dir: String,
    },
}

/// Arguments for `ahma cluster add-peer`.
#[derive(Parser, Debug)]
pub struct ClusterAddPeerArgs {
    /// Unique peer ID (hostname or UUID).
    #[arg(long)]
    pub id: String,
    /// HTTP address of the peer's ahma HTTP bridge (e.g. http://workstation.local:3000).
    #[arg(long)]
    pub addr: String,
    /// Comma-separated list of model names available on this peer.
    #[arg(long, value_delimiter = ',', default_value = "")]
    pub models: Vec<String>,
}

/// Arguments for `ahma cluster ping`.
#[derive(Parser, Debug)]
pub struct ClusterPingArgs {
    /// Peer ID to ping (must exist in ~/.ahma/cluster/peers.json).
    #[arg(value_name = "ID")]
    pub id: String,
}

/// Arguments for `ahma cluster remove`.
#[derive(Parser, Debug)]
pub struct ClusterRemoveArgs {
    /// Peer ID to remove (must exist in ~/.ahma/cluster/peers.json).
    #[arg(value_name = "ID")]
    pub id: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// AppConfig construction from CLI + env vars
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(unix)]
fn unix_socket_path_from_cli(cli: &Cli, s: &ahma_common::config::AhmaSettings) -> String {
    // R-CFG1.2: AHMA_UNIX_SOCKET is RETIRED — warn and ignore.
    warn_retired_env!("AHMA_UNIX_SOCKET");
    if let Some(path) = &cli.unix_socket_path {
        return path.clone();
    }
    match &cli.command {
        Subcommands::Serve(serve_args) => match &serve_args.transport {
            Some(ServeTransport::Unix(u)) => u
                .socket_path
                .clone()
                .or_else(|| s.http.unix_socket_path.clone())
                .unwrap_or_else(|| "/tmp/ahma.sock".to_string()),
            _ => s
                .http
                .unix_socket_path
                .clone()
                .unwrap_or_else(|| "/tmp/ahma.sock".to_string()),
        },
        _ => s
            .http
            .unix_socket_path
            .clone()
            .unwrap_or_else(|| "/tmp/ahma.sock".to_string()),
    }
}

#[cfg(not(unix))]
fn unix_socket_path_from_cli(_cli: &Cli, _s: &ahma_common::config::AhmaSettings) -> String {
    String::new()
}

struct ServeFields {
    http_host: String,
    http_port: u16,
}

fn extract_serve_fields(cmd: &Subcommands) -> ServeFields {
    // R-CFG1.2: AHMA_HTTP_PORT is RETIRED — warn and ignore.
    warn_retired_env!("AHMA_HTTP_PORT");

    if let Subcommands::Serve(s) = cmd {
        let (host, port) = match &s.transport {
            Some(ServeTransport::Http(h)) => (h.host.clone(), h.port),
            Some(ServeTransport::Stdio(_)) => ("127.0.0.1".to_string(), 3000u16),
            #[cfg(unix)]
            Some(ServeTransport::Unix(_)) => ("127.0.0.1".to_string(), 3000u16),
            None => ("127.0.0.1".to_string(), 3000u16),
        };
        ServeFields {
            http_host: host,
            http_port: port,
        }
    } else {
        ServeFields {
            http_host: "127.0.0.1".to_string(),
            http_port: 3000u16,
        }
    }
}

struct ToolFields {
    list_server: Option<String>,
    mcp_config: PathBuf,
    list_http: Option<String>,
    list_format: list_tools::OutputFormat,
    run_tool: Option<String>,
    run_tool_args: Vec<String>,
}

fn default_tool_fields() -> ToolFields {
    ToolFields {
        list_server: None,
        mcp_config: PathBuf::from("mcp.json"),
        list_http: None,
        list_format: list_tools::OutputFormat::Text,
        run_tool: None,
        run_tool_args: vec![],
    }
}

fn extract_tool_fields(cmd: &Subcommands) -> ToolFields {
    let Subcommands::Tool(ToolArgs { command }) = cmd else {
        return default_tool_fields();
    };
    match command {
        ToolCommand::List(la) => {
            let mut run_tool = None;
            let mut run_tool_args = vec![];
            if !la.server_args.is_empty() {
                run_tool = Some(la.server_args[0].clone());
                run_tool_args = la.server_args[1..].to_vec();
            }
            ToolFields {
                list_server: la.server.clone(),
                mcp_config: la.mcp_config.clone(),
                list_http: la.http.clone(),
                list_format: la.format.clone(),
                run_tool,
                run_tool_args,
            }
        }
        ToolCommand::Run(r) => ToolFields {
            list_server: None,
            mcp_config: PathBuf::from("mcp.json"),
            list_http: None,
            list_format: list_tools::OutputFormat::Text,
            run_tool: Some(r.tool.clone()),
            run_tool_args: r.tool_args.clone(),
        },
        _ => default_tool_fields(),
    }
}

/// Load `AhmaSettings` from the path determined by the CLI flags.
///
/// Respects `--no-settings` (skip loading entirely) and `--settings-path`
/// (load from an alternate path instead of `~/.ahma/settings.toml`).
pub fn load_settings(cli: &Cli) -> ahma_common::config::AhmaSettings {
    use ahma_common::config::{AhmaSettings, settings_path};
    if cli.no_settings {
        tracing::debug!("--no-settings: using compiled-in defaults");
        return AhmaSettings::default();
    }
    // Fail closed at startup (R-CFG6.1): a settings file that exists but does
    // not parse aborts the launch rather than silently reverting to defaults,
    // so a tampered or corrupt file cannot quietly change behavior. A missing
    // file is not an error.
    let path = match &cli.settings_path {
        Some(p) => Some(p.clone()),
        None => settings_path(),
    };
    let Some(path) = path else {
        return AhmaSettings::default();
    };
    match AhmaSettings::load_from_result(&path) {
        Ok(settings) => settings,
        Err(e) => {
            eprintln!(
                "ahma: fatal: {e}\n\
                 Fix or delete the file, or run `ahma settings init --force` to reset it."
            );
            std::process::exit(1);
        }
    }
}

fn parse_execution_settings(
    cli: &Cli,
    s: &ahma_common::config::AhmaSettings,
) -> (u64, bool, bool, bool) {
    // R-CFG1.2: preference-tier env vars are RETIRED — warn and ignore.
    warn_retired_env!("AHMA_TIMEOUT");
    warn_retired_env!("AHMA_SYNC");
    warn_retired_env!("AHMA_HOT_RELOAD");
    warn_retired_env!("AHMA_SKIP_PROBES");

    // CLI > settings > compiled-in default.
    let timeout_secs = cli.timeout.unwrap_or(s.tools.timeout_secs);
    let force_sync = cli.sync || s.tools.force_sync;
    let hot_reload_tools = cli.hot_reload || s.tools.hot_reload;
    let skip_availability_probes = cli.skip_probes || s.tools.skip_probes;

    (
        timeout_secs,
        force_sync,
        hot_reload_tools,
        skip_availability_probes,
    )
}

fn parse_sandbox_settings(
    cli: &Cli,
    s: &ahma_common::config::AhmaSettings,
) -> (bool, bool, bool, bool, bool, bool, u64, bool) {
    // Security-tier: AHMA_DISABLE_SANDBOX retired — warn and ignore.
    warn_retired_security_env!("AHMA_DISABLE_SANDBOX");
    let no_sandbox = cli.no_sandbox || s.sandbox.disable;

    // Security-tier: AHMA_SANDBOX_DEFER retired — warn and ignore.
    warn_retired_security_env!("AHMA_SANDBOX_DEFER");
    let defer_sandbox = cli.defer_sandbox || s.sandbox.defer;

    // Security-tier: AHMA_TMP_ACCESS retired — warn and ignore.
    warn_retired_security_env!("AHMA_TMP_ACCESS");
    let tmp_access = cli.tmp || s.sandbox.tmp_access;

    let use_sandbox_dir = cli.use_sandbox || s.sandbox.use_sandbox_directory;

    // Security-tier: AHMA_DISABLE_TEMP retired — warn and ignore.
    warn_retired_security_env!("AHMA_DISABLE_TEMP");
    let no_temp_files = cli.no_temp_files || s.sandbox.disable_temp;

    // R-CFG1.2: preference-tier env vars are RETIRED — warn and ignore.
    warn_retired_env!("AHMA_LOG_MONITOR");
    warn_retired_env!("AHMA_MONITOR_RATE_LIMIT");
    let log_monitor = cli.log_monitor || s.logging.log_monitor;
    let monitor_rate_limit_secs = cli
        .monitor_rate_limit
        .unwrap_or(s.logging.monitor_rate_limit_secs);

    // Security-tier: AHMA_NO_PACKAGE_CACHE_WRITE retired — warn and ignore.
    warn_retired_security_env!("AHMA_NO_PACKAGE_CACHE_WRITE");
    let package_cache_write = !cli.no_package_cache_write && s.sandbox.package_cache_write;

    (
        no_sandbox,
        defer_sandbox,
        tmp_access,
        use_sandbox_dir,
        no_temp_files,
        log_monitor,
        monitor_rate_limit_secs,
        package_cache_write,
    )
}

fn parse_http_settings(cli: &Cli, s: &ahma_common::config::AhmaSettings) -> (bool, bool, u64) {
    // R-CFG1.2: preference-tier env vars are RETIRED — warn and ignore.
    warn_retired_env!("AHMA_DISABLE_QUIC");
    warn_retired_env!("AHMA_DISABLE_HTTP1_1");
    warn_retired_env!("AHMA_HANDSHAKE_TIMEOUT");

    let no_quic = cli.disable_quic || s.http.disable_quic;
    let disable_http1_1 = cli.disable_http1_1 || s.http.disable_http1_1;
    let handshake_timeout_secs = cli
        .handshake_timeout
        .unwrap_or(s.http.handshake_timeout_secs);

    (no_quic, disable_http1_1, handshake_timeout_secs)
}

fn parse_auth_settings(
    cli: &Cli,
    s: &ahma_common::config::AhmaSettings,
) -> (Option<String>, Option<PathBuf>, u64, u32, String) {
    // Security-tier: AHMA_REQUIRE_TOKEN retired — warn and ignore.
    warn_retired_security_env!("AHMA_REQUIRE_TOKEN");
    let require_token = cli
        .require_token
        .clone()
        .or_else(|| s.auth.require_token.clone());

    // Security-tier: AHMA_REQUIRE_TOKEN_PATH retired — warn and ignore.
    warn_retired_security_env!("AHMA_REQUIRE_TOKEN_PATH");
    let require_token_path = cli.require_token_path.clone().or_else(|| {
        if s.auth.require_token_path.is_empty() {
            None
        } else {
            Some(PathBuf::from(&s.auth.require_token_path))
        }
    });

    // Security-tier: AHMA_RATE_LIMIT_RPS and AHMA_RATE_LIMIT_BURST retired — warn and ignore.
    warn_retired_security_env!("AHMA_RATE_LIMIT_RPS");
    let rate_limit_rps = cli.rate_limit_rps.unwrap_or(s.auth.rate_limit_rps);

    warn_retired_security_env!("AHMA_RATE_LIMIT_BURST");
    let rate_limit_burst = cli.rate_limit_burst.unwrap_or(s.auth.rate_limit_burst);

    // R-CFG1.2: AHMA_INSTANCE_LABEL is RETIRED — warn and ignore.
    warn_retired_env!("AHMA_INSTANCE_LABEL");
    let instance_label = cli
        .instance_label
        .clone()
        .unwrap_or_else(|| s.instance.label.clone());

    (
        require_token,
        require_token_path,
        rate_limit_rps,
        rate_limit_burst,
        instance_label,
    )
}

fn resolve_tool_bundles(cli: &Cli, s: &ahma_common::config::AhmaSettings) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut bundles = cli.tool_bundles.clone();
    if bundles.is_empty() {
        bundles = s.tools.tool_bundles.clone();
    }
    bundles
        .into_iter()
        .filter(|b| seen.insert(b.clone()))
        .collect()
}

fn resolve_sandbox_scopes_cli(cli: &Cli, s: &ahma_common::config::AhmaSettings) -> Vec<PathBuf> {
    // Security-tier: AHMA_SANDBOX_SCOPE retired — warn and ignore.
    warn_retired_security_env!("AHMA_SANDBOX_SCOPE");
    if !cli.sandbox_scopes.is_empty() {
        cli.sandbox_scopes
            .iter()
            .map(|p| expand_tilde(p.clone()))
            .collect()
    } else {
        s.sandbox
            .scopes
            .iter()
            .map(|p| expand_tilde(p.clone()))
            .collect()
    }
}

fn resolve_working_dirs_cli(cli: &Cli, s: &ahma_common::config::AhmaSettings) -> Vec<PathBuf> {
    // Security-tier: AHMA_WORKING_DIRS retired — warn and ignore.
    warn_retired_security_env!("AHMA_WORKING_DIRS");
    if !cli.working_dirs.is_empty() {
        cli.working_dirs
            .iter()
            .map(|p| expand_tilde(p.clone()))
            .collect()
    } else {
        s.sandbox
            .working_dirs
            .iter()
            .map(|p| expand_tilde(p.clone()))
            .collect()
    }
}

pub fn build_app_config(cli: &Cli) -> AppConfig {
    // Apply process-wide overrides from CLI flags BEFORE anything reads the
    // corresponding deprecated env vars — flags are the visible, diagnosable
    // configuration path (no ambient OS/ENV state leaking in).
    if let Some(dir) = &cli.log_dir {
        crate::utils::logging::set_log_dir_override(dir.clone());
    }
    if let Some(mode) = &cli.hooks_mode {
        crate::hooks::set_hooks_mode_override(mode);
    }
    if let Some(dir) = &cli.tls_dir {
        ahma_common::local_tls::LocalTlsConfig::set_dir_override(dir.clone());
    }
    if let Some(path) = &cli.daemon_socket {
        ahma_common::daemon_hub::set_socket_path_override(path.clone());
    }

    let serve = extract_serve_fields(&cli.command);
    let tool = extract_tool_fields(&cli.command);

    // Load user settings (priority layer 2: below CLI flags, above env vars)
    let s = load_settings(cli);

    // ── Tool loading ────────────────────────────────────────────────────────
    // R-CFG1.2: AHMA_TOOLS_DIR is RETIRED — warn and ignore.
    warn_retired_env!("AHMA_TOOLS_DIR");
    let explicit_tools_dir = cli.tools_dir.is_some();
    // CLI > settings > compiled-in default.
    let raw_tools_dir = cli.tools_dir.clone().or_else(|| s.tools.tools_dir.clone());
    let tools_dir = resolution::normalize_tools_dir(raw_tools_dir);

    // Flatten and deduplicate tool bundles
    let tool_bundles = resolve_tool_bundles(cli, &s);

    // ── Sandbox scope ───────────────────────────────────────────────────────
    let sandbox_scopes = resolve_sandbox_scopes_cli(cli, &s);

    let working_dirs = resolve_working_dirs_cli(cli, &s);

    // ── Parse settings sections via modular helper functions ─────────────────
    let (timeout_secs, force_sync, hot_reload_tools, skip_availability_probes) =
        parse_execution_settings(cli, &s);

    let (
        no_sandbox,
        defer_sandbox,
        tmp_access,
        use_sandbox_dir,
        no_temp_files,
        log_monitor,
        monitor_rate_limit_secs,
        package_cache_write,
    ) = parse_sandbox_settings(cli, &s);

    let idle_timeout_secs = cli.idle_timeout;

    let (no_quic, disable_http1_1, handshake_timeout_secs) = parse_http_settings(cli, &s);

    let (require_token, require_token_path, rate_limit_rps, rate_limit_burst, instance_label) =
        parse_auth_settings(cli, &s);

    // R-CFG1.2: AHMA_MINIMIZE_TOKENS / AHMA_SMALL_MODEL_HARNESS are RETIRED — warn and ignore.
    warn_retired_env!("AHMA_MINIMIZE_TOKENS");
    warn_retired_env!("AHMA_SMALL_MODEL_HARNESS");
    let minimize_tokens = cli.minimize_tokens || s.tools.minimize_tokens;
    let small_model_harness = cli.small_model_harness || s.tools.small_model_harness;
    let mutex_groups = s.tools.mutex_groups.clone();
    let separate_cargo_target = s.tools.separate_cargo_target;

    AppConfig {
        tools_dir,
        explicit_tools_dir,
        tool_bundles,
        timeout_secs,
        force_sync,
        hot_reload_tools,
        skip_availability_probes,
        minimize_tokens,
        small_model_harness,
        mutex_groups,
        separate_cargo_target,

        no_sandbox,
        sandbox_scopes,
        defer_sandbox,
        working_dirs,
        sandbox_directory: s.sandbox.sandbox_directory.clone(),
        use_sandbox_dir,
        tmp_access,
        no_temp_files,
        log_monitor,
        monitor_rate_limit_secs,
        package_cache_write,
        // Loaded from settings.toml (honors --no-settings via `s`); survives
        // roots/list because the subprocess reads it directly, not via the bridge.
        persistent_scopes: s.sandbox.persistent_scopes.clone(),
        trust_build_caches: s.sandbox.trust_build_caches,
        http_host: serve.http_host,
        http_port: serve.http_port,
        no_quic,
        disable_http1_1,
        handshake_timeout_secs,
        unix_socket_path: unix_socket_path_from_cli(cli, &s),
        observability: ahma_common::observability::ObservabilityConfig::from_env("ahma_mcp")
            .with_endpoint(cli.opentelemetry.as_deref()),
        list_server: tool.list_server,
        mcp_config: tool.mcp_config,
        list_http: tool.list_http,
        list_format: tool.list_format,
        run_tool: tool.run_tool,
        run_tool_args: tool.run_tool_args,
        task_vault: {
            // Security-tier: AHMA_TASK_VAULT retired — warn and ignore.
            warn_retired_security_env!("AHMA_TASK_VAULT");
            cli.task_vault
                .clone()
                .map(expand_tilde)
                .or_else(|| s.sandbox.task_vault.clone().map(expand_tilde))
        },
        require_token,
        require_token_path,
        rate_limit_rps,
        rate_limit_burst,
        instance_label,
        idle_timeout_secs,
        max_sessions: cli.max_sessions.unwrap_or(10),
        is_server_child: cli.server_child || std::env::var("AHMA_SERVER_CHILD").is_ok(),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

pub async fn run() -> Result<()> {
    // Load settings early so we can determine log target before initialising logging.
    // We do a minimal Cli parse just to capture --no-settings / --settings-path; the
    // full parse happens below.  We also honour the legacy AHMA_LOG_TARGET env var with
    // a deprecation-friendly approach: settings file wins, env var is a fallback.
    let cli = Cli::parse();

    // R-CFG1.2: AHMA_LOG_TARGET is RETIRED — warn and ignore.
    if std::env::var_os("AHMA_LOG_TARGET").is_some() {
        tracing::warn!(
            "AHMA env var AHMA_LOG_TARGET is set but IGNORED (retired per R-CFG1.2). \
             Use `logging.target = \"stderr\"` in ~/.ahma/settings.toml instead."
        );
    }
    let settings_for_log = load_settings(&cli);
    let log_to_stderr = settings_for_log.log_to_stderr() || cli.log_to_stderr;

    set_log_role(detect_log_role_from_startup());

    let cfg = build_app_config(&cli);
    let subcommand = cli.command;

    // Keep the guard alive for the duration of the process.
    let _telemetry_guard =
        init_logging_with_observability("info", !log_to_stderr, Some(cfg.observability.clone()))?;

    #[cfg(target_os = "windows")]
    check_powershell_available();

    dispatch_subcommand(subcommand, cfg).await
}

pub(crate) fn initialize_sandbox(cfg: &AppConfig) -> Result<Option<Arc<sandbox::Sandbox>>> {
    let policy = resolve_sandbox_policy(cfg);

    check_sandbox_availability(policy.no_sandbox)?;

    let scopes = resolve_sandbox_scopes(cfg)?;
    let scopes = add_temp_scope_if_requested(scopes, policy.tmp_access);
    let sandbox = create_sandbox_instance(scopes, &policy, cfg)?;

    log_sandbox_mode(policy.no_sandbox);
    Ok(sandbox)
}

fn run_validation_mode(target: &str) -> Result<()> {
    commands::run_validation_mode(target)
}

async fn run_tool_info_mode(args: InfoArgs) -> Result<()> {
    commands::run_tool_info_mode(args).await
}

/// Read a boolean env var ("1","true","yes","on" → true; anything else → false).
///
/// Public for use in tests.
pub fn env_flag_enabled(name: &str) -> bool {
    AppConfig::env_flag(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{IsTerminal, Write};
    use std::sync::{LazyLock, Mutex};
    use tempfile::tempdir;

    static ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn init_test() {
        crate::utils::logging::init_test_logging();
    }

    // ─── apply_build_cache_consent (P1a pre-lock build-cache consent) ─────────

    fn fake_cache(dir: PathBuf) -> sandbox::build_cache::BuildCache {
        sandbox::build_cache::BuildCache {
            tool: "sccache",
            dir,
            source: sandbox::build_cache::CacheSource::Env("SCCACHE_DIR"),
        }
    }

    #[test]
    fn build_cache_consent_grants_when_trusted() {
        init_test();
        let tmp = tempdir().unwrap();
        let cache_dir = tmp.path().join("sccache-cache");
        let caches = vec![fake_cache(cache_dir.clone())];
        let mut write_scopes = Vec::new();

        apply_build_cache_consent(true, &caches, &mut write_scopes);

        assert_eq!(write_scopes.len(), 1, "trusted cache should be granted");
        // ensure_sandbox_directory creates + canonicalizes the dir.
        assert!(cache_dir.exists(), "cache dir should be created");
        let canonical = dunce::canonicalize(&cache_dir).unwrap();
        assert_eq!(write_scopes[0], canonical);
    }

    #[test]
    fn build_cache_consent_denies_when_not_trusted() {
        init_test();
        let tmp = tempdir().unwrap();
        let cache_dir = tmp.path().join("sccache-cache");
        let caches = vec![fake_cache(cache_dir.clone())];
        let mut write_scopes = Vec::new();

        apply_build_cache_consent(false, &caches, &mut write_scopes);

        assert!(
            write_scopes.is_empty(),
            "without opt-in the sandbox must not be widened"
        );
        assert!(
            !cache_dir.exists(),
            "a non-trusted cache dir must not be auto-created"
        );
    }

    #[test]
    fn build_cache_consent_dedupes_already_present_scope() {
        init_test();
        let tmp = tempdir().unwrap();
        let cache_dir = tmp.path().join("sccache-cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let canonical = dunce::canonicalize(&cache_dir).unwrap();
        let caches = vec![fake_cache(cache_dir.clone())];
        // Scope already contains the (canonical) cache dir.
        let mut write_scopes = vec![canonical.clone()];

        apply_build_cache_consent(true, &caches, &mut write_scopes);

        assert_eq!(
            write_scopes.len(),
            1,
            "an already-present cache must not be added twice"
        );
    }

    #[test]
    fn build_cache_consent_empty_caches_is_noop() {
        init_test();
        let mut write_scopes = vec![PathBuf::from("/existing")];
        apply_build_cache_consent(true, &[], &mut write_scopes);
        assert_eq!(write_scopes, vec![PathBuf::from("/existing")]);
    }

    // ─── env_flag_enabled ───────────────────────────────────────────────────

    #[test]
    fn test_env_flag_enabled_unset() {
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe { std::env::remove_var("AHMA_TEST_FLAG_UNSET") };
        assert!(!env_flag_enabled("AHMA_TEST_FLAG_UNSET"));
    }

    #[test]
    fn test_env_flag_enabled_empty() {
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe { std::env::set_var("AHMA_TEST_FLAG_EMPTY", "") };
        let result = env_flag_enabled("AHMA_TEST_FLAG_EMPTY");
        unsafe { std::env::remove_var("AHMA_TEST_FLAG_EMPTY") };
        assert!(!result);
    }

    #[test]
    fn test_env_flag_enabled_whitespace_only() {
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe { std::env::set_var("AHMA_TEST_FLAG_WS", "   ") };
        let result = env_flag_enabled("AHMA_TEST_FLAG_WS");
        unsafe { std::env::remove_var("AHMA_TEST_FLAG_WS") };
        assert!(!result);
    }

    #[test]
    fn test_env_flag_enabled_true() {
        let _guard = ENV_MUTEX.lock().unwrap();
        for val in ["1", "true", "True", "TRUE", "yes", "Yes", "on", "ON"] {
            unsafe { std::env::set_var("AHMA_TEST_FLAG_VAL", val) };
            let result = env_flag_enabled("AHMA_TEST_FLAG_VAL");
            unsafe { std::env::remove_var("AHMA_TEST_FLAG_VAL") };
            assert!(result, "env_flag_enabled({:?}) should be true", val);
        }
    }

    #[test]
    fn test_env_flag_enabled_false() {
        let _guard = ENV_MUTEX.lock().unwrap();
        for val in ["0", "false", "no", "off", "x", ""] {
            if val.is_empty() {
                continue;
            }
            unsafe { std::env::set_var("AHMA_TEST_FLAG_FALSE", val) };
            let result = env_flag_enabled("AHMA_TEST_FLAG_FALSE");
            unsafe { std::env::remove_var("AHMA_TEST_FLAG_FALSE") };
            assert!(!result, "env_flag_enabled({:?}) should be false", val);
        }
    }

    // ─── R-CFG8.1: security-tier env vars are retired (warn + ignore) ────────

    /// Red-team (R-CFG8.1): a process that sets `AHMA_DISABLE_SANDBOX=1` in the
    /// environment must NOT disable the sandbox. Only the `--no-sandbox` flag
    /// (or nested-sandbox auto-detection) may do that. The resolved
    /// `no_sandbox` must stay `false` when the flag is absent.
    #[test]
    fn disable_sandbox_env_var_is_ignored() {
        let _guard = ENV_MUTEX.lock().unwrap();
        init_test();
        unsafe { std::env::set_var("AHMA_DISABLE_SANDBOX", "1") };
        // A default invocation with no --no-sandbox flag and default settings.
        let cli = Cli::parse_from(["ahma", "serve", "stdio"]);
        let settings = ahma_common::config::AhmaSettings::default();
        let resolved = parse_sandbox_settings(&cli, &settings);
        unsafe { std::env::remove_var("AHMA_DISABLE_SANDBOX") };
        assert!(
            !resolved.0,
            "AHMA_DISABLE_SANDBOX=1 must be ignored; no_sandbox should remain false without --no-sandbox"
        );
    }

    /// The `--no-sandbox` flag is still honored (the supported override).
    #[test]
    fn no_sandbox_flag_is_honored() {
        let _guard = ENV_MUTEX.lock().unwrap();
        init_test();
        unsafe { std::env::remove_var("AHMA_DISABLE_SANDBOX") };
        let cli = Cli::parse_from(["ahma", "--no-sandbox", "serve", "stdio"]);
        let settings = ahma_common::config::AhmaSettings::default();
        let resolved = parse_sandbox_settings(&cli, &settings);
        assert!(resolved.0, "--no-sandbox must set no_sandbox = true");
    }

    // ─── resolve_sandbox_policy ──────────────────────────────────────────────

    fn make_cfg() -> AppConfig {
        AppConfig {
            tools_dir: None,
            explicit_tools_dir: false,
            tool_bundles: vec![],
            timeout_secs: 360,
            force_sync: false,
            hot_reload_tools: false,
            skip_availability_probes: false,
            minimize_tokens: false,
            small_model_harness: false,
            mutex_groups: ahma_common::config::default_mutex_groups(),
            separate_cargo_target: false,

            no_sandbox: false,
            sandbox_scopes: vec![],
            defer_sandbox: false,
            working_dirs: vec![],
            sandbox_directory: None,
            use_sandbox_dir: false,
            tmp_access: false,
            no_temp_files: false,
            log_monitor: false,
            monitor_rate_limit_secs: 60,
            package_cache_write: true,
            persistent_scopes: vec![],
            trust_build_caches: false,
            http_host: "127.0.0.1".to_string(),
            http_port: 3000,
            no_quic: false,
            disable_http1_1: false,
            handshake_timeout_secs: 45,
            unix_socket_path: String::new(),
            list_server: None,
            mcp_config: PathBuf::from("mcp.json"),
            list_http: None,
            list_format: list_tools::OutputFormat::Text,
            run_tool: None,
            run_tool_args: vec![],
            observability: ahma_common::observability::ObservabilityConfig::default(),
            task_vault: None,
            require_token: None,
            require_token_path: None,
            rate_limit_rps: 0,
            rate_limit_burst: 10,
            instance_label: "ahma".to_string(),
            idle_timeout_secs: None,
            max_sessions: 10,
            is_server_child: false,
        }
    }

    #[test]
    fn test_resolve_sandbox_policy_no_sandbox_flag() {
        init_test();
        let cfg = AppConfig {
            no_sandbox: true,
            ..make_cfg()
        };
        let policy = resolve_sandbox_policy(&cfg);
        assert!(policy.no_sandbox);
        assert_eq!(policy.mode, sandbox::SandboxMode::Test);
    }

    #[test]
    fn test_resolve_sandbox_policy_strict_by_default() {
        let _guard = ENV_MUTEX.lock().unwrap();
        init_test();
        unsafe { std::env::remove_var("AHMA_DISABLE_SANDBOX") };
        let cfg = make_cfg();
        let policy = resolve_sandbox_policy(&cfg);
        assert!(!policy.no_sandbox);
        assert_eq!(policy.mode, sandbox::SandboxMode::Strict);
    }

    #[test]
    fn test_resolve_sandbox_policy_tmp_flag() {
        init_test();
        let cfg = AppConfig {
            tmp_access: true,
            ..make_cfg()
        };
        let policy = resolve_sandbox_policy(&cfg);
        assert!(policy.tmp_access);
    }

    #[test]
    fn test_resolve_sandbox_policy_tmp_access_flag() {
        // AHMA_TMP_ACCESS is retired; tmp_access is set via CLI flag or settings.toml.
        // Verify that AppConfig.tmp_access = true is honored by resolve_sandbox_policy.
        init_test();
        let cfg = AppConfig {
            tmp_access: true,
            ..make_cfg()
        };
        let policy = resolve_sandbox_policy(&cfg);
        assert!(policy.tmp_access);
    }

    // ─── canonicalize_paths (via resolve_sandbox_scopes) ─────────────────────

    #[test]
    fn test_canonicalize_paths_via_sandbox_scope() {
        init_test();
        let tmp = tempdir().unwrap();
        let path = tmp.path().to_path_buf();
        let cfg = AppConfig {
            no_sandbox: true,
            sandbox_scopes: vec![path.clone()],
            ..make_cfg()
        };
        let scopes = resolve_sandbox_scopes(&cfg).unwrap();
        assert!(scopes.is_some());
        let scopes = scopes.unwrap();
        assert_eq!(scopes.len(), 1);
        assert_eq!(dunce::canonicalize(&path).unwrap(), scopes[0]);
    }

    #[test]
    fn test_canonicalize_paths_invalid_fails() {
        init_test();
        // Use a platform-appropriate path that `create_dir_all` cannot create:
        // - Unix: /nonexistent/... fails (no root perms)
        // - Windows: Z:\nonexistent\... fails (non-existent drive)
        #[cfg(unix)]
        let bad_path = PathBuf::from("/nonexistent/path/that/does/not/exist");
        #[cfg(windows)]
        let bad_path = PathBuf::from("Z:\\nonexistent\\path\\that\\does\\not\\exist");
        let cfg = AppConfig {
            no_sandbox: true,
            sandbox_scopes: vec![bad_path],
            ..make_cfg()
        };
        let result = resolve_sandbox_scopes(&cfg);
        assert!(result.is_err());
    }

    // ─── resolve_sandbox_scopes ──────────────────────────────────────────────

    #[test]
    fn test_resolve_sandbox_scopes_explicit() {
        init_test();
        let tmp = tempdir().unwrap();
        let cfg = AppConfig {
            no_sandbox: true,
            sandbox_scopes: vec![tmp.path().to_path_buf()],
            ..make_cfg()
        };
        let scopes = resolve_sandbox_scopes(&cfg).unwrap();
        assert!(scopes.is_some());
        assert_eq!(scopes.unwrap().len(), 1);
    }

    #[test]
    fn test_resolve_sandbox_scopes_explicit_nonexistent_created() {
        init_test();
        let tmp = tempdir().unwrap();
        let nonexistent_sub = tmp.path().join("sub_dir_nonexistent");
        assert!(!nonexistent_sub.exists());
        let cfg = AppConfig {
            no_sandbox: true,
            sandbox_scopes: vec![nonexistent_sub.clone()],
            ..make_cfg()
        };
        let scopes = resolve_sandbox_scopes(&cfg).unwrap();
        assert!(scopes.is_some());
        let scopes = scopes.unwrap();
        assert_eq!(scopes.len(), 1);
        assert!(nonexistent_sub.exists());
        assert_eq!(dunce::canonicalize(&nonexistent_sub).unwrap(), scopes[0]);
    }

    #[test]
    fn test_resolve_sandbox_scopes_task_vault_precedence_and_layout() {
        init_test();
        let tmp = tempdir().unwrap();
        let vault_root = tmp.path().join("task-vault");
        let cfg = AppConfig {
            no_sandbox: true,
            sandbox_scopes: vec![tmp.path().to_path_buf()],
            task_vault: Some(vault_root.clone()),
            ..make_cfg()
        };

        let scopes = resolve_sandbox_scopes(&cfg).unwrap().unwrap();
        assert_eq!(scopes.len(), 3);

        let expected_workdir = dunce::canonicalize(vault_root.join("workdir")).unwrap();
        let expected_trash = dunce::canonicalize(vault_root.join("trash")).unwrap();
        let expected_audit = dunce::canonicalize(vault_root.join("audit.jsonl")).unwrap();
        assert_eq!(scopes[0], expected_workdir);
        assert_eq!(scopes[1], expected_trash);
        assert_eq!(scopes[2], expected_audit);
        assert!(vault_root.join("inputs").is_dir());
        assert!(vault_root.join("workdir").is_dir());
        assert!(vault_root.join("outputs").is_dir());
        assert!(vault_root.join("trash").is_dir());
        assert!(vault_root.join("audit.jsonl").is_file());
    }

    #[test]
    fn test_resolve_sandbox_scopes_ahma_sandbox_scope_env() {
        init_test();
        let tmp = tempdir().unwrap();
        let path = tmp.path().to_path_buf();
        // Simulate what build_app_config does: read env at config-build time
        let cfg = AppConfig {
            no_sandbox: true,
            sandbox_scopes: vec![path],
            ..make_cfg()
        };
        let result = resolve_sandbox_scopes(&cfg);
        assert!(result.is_ok());
        let scopes = result.unwrap();
        assert!(scopes.is_some());
        assert_eq!(scopes.unwrap().len(), 1);
    }

    // Note: CWD-fallback behaviour was removed in the R5.2.1 redesign (the launch
    // CWD is never inferred as a scope). See `resolve_sandbox_scopes_ignores_cwd_and_markers`
    // and `resolve_sandbox_scopes_no_default_awaits_roots`.

    // ─── resolve_deferred_scopes ─────────────────────────────────────────────

    #[test]
    fn test_resolve_deferred_scopes_with_working_dirs() {
        init_test();
        let tmp = tempdir().unwrap();
        let cfg = AppConfig {
            no_sandbox: true,
            defer_sandbox: true,
            working_dirs: vec![tmp.path().to_path_buf()],
            ..make_cfg()
        };
        let scopes = resolve_deferred_scopes(&cfg).unwrap();
        assert!(scopes.is_some());
        let scopes = scopes.unwrap();
        assert_eq!(scopes.len(), 1);
        assert_eq!(dunce::canonicalize(tmp.path()).unwrap(), scopes[0]);
    }

    #[test]
    fn test_resolve_deferred_scopes_without_working_dirs() {
        init_test();
        let cfg = AppConfig {
            no_sandbox: true,
            defer_sandbox: true,
            ..make_cfg()
        };
        let scopes = resolve_deferred_scopes(&cfg).unwrap();
        assert!(scopes.is_some());
        assert!(scopes.unwrap().is_empty());
    }

    #[test]
    fn test_resolve_sandbox_scopes_defer_takes_precedence() {
        init_test();
        let tmp = tempdir().unwrap();
        let cfg = AppConfig {
            no_sandbox: true,
            defer_sandbox: true,
            working_dirs: vec![tmp.path().to_path_buf()],
            ..make_cfg()
        };
        let scopes = resolve_sandbox_scopes(&cfg).unwrap();
        assert!(scopes.is_some());
        assert_eq!(scopes.unwrap().len(), 1);
    }

    /// Regression: a deferred sandbox with an explicit `--sandbox-scope` but no
    /// `--working-directories` must seed that scope as a provisional fallback,
    /// NOT return an empty scope set. The HTTP bridge forwards its `default_scope`
    /// to the subprocess as `--sandbox-scope <path> --defer-sandbox`; if defer
    /// resolution drops that scope, a client that doesn't support `roots/list`
    /// (Claude Desktop, Antigravity) gets `-32601`, the subprocess has no
    /// pre-configured scope to fall back to, and emits `notifications/sandbox/failed`
    /// — poisoning the session so every `tools/call` returns HTTP 409 forever.
    #[test]
    fn test_resolve_deferred_scopes_falls_back_to_sandbox_scope() {
        init_test();
        let tmp = tempdir().unwrap();
        let cfg = AppConfig {
            no_sandbox: true,
            defer_sandbox: true,
            sandbox_scopes: vec![tmp.path().to_path_buf()],
            ..make_cfg()
        };
        let scopes = resolve_deferred_scopes(&cfg)
            .unwrap()
            .expect("deferred scopes resolve to Some");
        assert_eq!(
            scopes.len(),
            1,
            "explicit --sandbox-scope must seed a provisional deferred scope, got {scopes:?}"
        );
        assert_eq!(dunce::canonicalize(tmp.path()).unwrap(), scopes[0]);
    }

    /// `--working-directories` still wins over `--sandbox-scope` in defer mode.
    #[test]
    fn test_resolve_deferred_scopes_working_dirs_win_over_sandbox_scope() {
        init_test();
        let work = tempdir().unwrap();
        let fallback = tempdir().unwrap();
        let cfg = AppConfig {
            no_sandbox: true,
            defer_sandbox: true,
            working_dirs: vec![work.path().to_path_buf()],
            sandbox_scopes: vec![fallback.path().to_path_buf()],
            ..make_cfg()
        };
        let scopes = resolve_deferred_scopes(&cfg).unwrap().unwrap();
        assert_eq!(scopes.len(), 1);
        assert_eq!(dunce::canonicalize(work.path()).unwrap(), scopes[0]);
    }

    // ─── add_temp_scope_if_requested ────────────────────────────────────────

    #[test]
    fn test_add_temp_scope_no_tmp_returns_unchanged() {
        init_test();
        let tmp = tempdir().unwrap();
        let scopes = Some(vec![tmp.path().to_path_buf()]);
        let result = add_temp_scope_if_requested(scopes.clone(), false);
        assert_eq!(result, scopes);
    }

    #[test]
    fn test_add_temp_scope_with_tmp_adds_temp_dir() {
        init_test();
        let tmp = tempdir().unwrap();
        let scopes = Some(vec![tmp.path().to_path_buf()]);
        let result = add_temp_scope_if_requested(scopes, true);
        assert!(result.is_some());
        let result = result.unwrap();
        let temp_dir = std::env::temp_dir();
        let canonical_temp = dunce::canonicalize(&temp_dir).unwrap();
        assert!(
            result.contains(&canonical_temp),
            "Expected temp dir in scopes: {:?}",
            result
        );
    }

    #[test]
    fn test_add_temp_scope_none_returns_none_when_no_tmp() {
        let result = add_temp_scope_if_requested(None, false);
        assert!(result.is_none());
    }

    #[test]
    fn test_add_temp_scope_tmp_with_none_returns_none() {
        init_test();
        // When scopes is None, scopes? returns early; temp is only added to existing scopes
        let result = add_temp_scope_if_requested(None, true);
        assert!(result.is_none());
    }

    #[test]
    fn test_add_temp_scope_does_not_seed_empty_scopes() {
        init_test();
        // Regression: with --tmp and a deferred/empty scope set, the temp dir must
        // NOT become the sole sandbox root. Seeding it would make the sandbox look
        // "already configured" and lock it to the temp directory, rejecting the
        // real workspace (the Cursor shared-process failure mode). The temp dir is
        // re-added by Sandbox::update_scopes once client roots arrive.
        let result = add_temp_scope_if_requested(Some(Vec::new()), true);
        assert_eq!(
            result,
            Some(Vec::new()),
            "empty scope set must stay empty so the sandbox waits for client roots"
        );
    }

    // ─── create_sandbox_instance & log_sandbox_mode ──────────────────────────

    #[test]
    fn test_create_sandbox_instance_none() {
        init_test();
        let cfg = AppConfig {
            no_sandbox: true,
            ..make_cfg()
        };
        let policy = resolve_sandbox_policy(&cfg);
        let sandbox = create_sandbox_instance(None, &policy, &cfg).unwrap();
        assert!(sandbox.is_none());
    }

    #[test]
    fn test_create_sandbox_instance_some() {
        init_test();
        let tmp = tempdir().unwrap();
        let scopes = Some(vec![tmp.path().to_path_buf()]);
        let cfg = AppConfig {
            no_sandbox: true,
            ..make_cfg()
        };
        let policy = resolve_sandbox_policy(&cfg);
        let sandbox = create_sandbox_instance(scopes, &policy, &cfg).unwrap();
        assert!(sandbox.is_some());
    }

    #[test]
    fn test_create_sandbox_instance_implicit_scopes_not_explicit() {
        init_test();
        // No explicit scope config => sandbox derived from CWD fallback.
        // It must NOT be marked explicit, so the service still requests
        // roots/list (the Cursor shared-process fix, SPEC R5.5).
        let tmp = tempdir().unwrap();
        let scopes = Some(vec![tmp.path().to_path_buf()]);
        let cfg = AppConfig {
            no_sandbox: true,
            ..make_cfg()
        };
        let policy = resolve_sandbox_policy(&cfg);
        let sandbox = create_sandbox_instance(scopes, &policy, &cfg)
            .unwrap()
            .unwrap();
        assert!(
            !sandbox.has_explicit_scopes(),
            "CWD-derived scopes must be implicit so roots/list is still requested"
        );
    }

    #[test]
    fn test_create_sandbox_instance_explicit_scopes_marked() {
        init_test();
        let tmp = tempdir().unwrap();
        let scopes = Some(vec![tmp.path().to_path_buf()]);
        let cfg = AppConfig {
            no_sandbox: true,
            sandbox_scopes: vec![tmp.path().to_path_buf()],
            ..make_cfg()
        };
        let policy = resolve_sandbox_policy(&cfg);
        let sandbox = create_sandbox_instance(scopes, &policy, &cfg)
            .unwrap()
            .unwrap();
        assert!(
            sandbox.has_explicit_scopes(),
            "--sandbox-scope must mark scopes explicit so roots/list is skipped (R5.5)"
        );
    }

    #[test]
    fn test_log_sandbox_mode_disabled() {
        init_test();
        log_sandbox_mode(true);
    }

    #[test]
    fn test_log_sandbox_mode_enabled() {
        init_test();
        log_sandbox_mode(false);
    }

    // ─── check_sandbox_availability ──────────────────────────────────────────

    #[test]
    fn test_check_sandbox_availability_ok_when_no_sandbox() {
        init_test();
        assert!(check_sandbox_availability(true).is_ok());
    }

    // ─── run_validation_mode ─────────────────────────────────────────────────

    #[test]
    fn test_run_validation_mode_valid_config() {
        init_test();
        let tmp = tempdir().unwrap();
        let tools_dir = tmp.path().join(".ahma");
        std::fs::create_dir_all(&tools_dir).unwrap();
        let valid_json = r#"{
            "name": "test_tool",
            "description": "Test",
            "command": "echo",
            "enabled": true,
            "subcommand": [{"name": "default", "description": "Default", "enabled": true}]
        }"#;
        let tool_file = tools_dir.join("test.json");
        std::fs::File::create(&tool_file)
            .unwrap()
            .write_all(valid_json.as_bytes())
            .unwrap();
        let target = tools_dir.to_str().unwrap();
        let result = run_validation_mode(target);
        assert!(result.is_ok(), "run_validation_mode failed: {:?}", result);
    }

    #[test]
    fn test_run_validation_mode_invalid_fails() {
        init_test();
        let tmp = tempdir().unwrap();
        let invalid_dir = tmp.path().join("nonexistent_validation_target");
        let result = run_validation_mode(invalid_dir.to_str().unwrap());
        assert!(result.is_err());
    }

    // ─── initialize_sandbox ───────────────────────────────────────────────────

    #[test]
    fn test_initialize_sandbox_no_sandbox() {
        init_test();
        let tmp = tempdir().unwrap();
        let cfg = AppConfig {
            no_sandbox: true,
            sandbox_scopes: vec![tmp.path().to_path_buf()],
            ..make_cfg()
        };
        let sandbox = initialize_sandbox(&cfg).unwrap();
        assert!(sandbox.is_some());
    }

    #[test]
    fn test_initialize_sandbox_defer_with_working_dirs() {
        init_test();
        let tmp = tempdir().unwrap();
        let cfg = AppConfig {
            no_sandbox: true,
            defer_sandbox: true,
            working_dirs: vec![tmp.path().to_path_buf()],
            ..make_cfg()
        };
        let sandbox = initialize_sandbox(&cfg).unwrap();
        assert!(sandbox.is_some());
    }

    // ─── check_stdio_not_interactive ─────────────────────────────────────────

    #[test]
    fn test_check_stdio_not_interactive() {
        init_test();
        // When tests are launched from an interactive terminal, this helper
        // intentionally exits the process. Only call it in the non-TTY case.
        if std::io::stdin().is_terminal() {
            return;
        }
        let result = check_stdio_not_interactive();
        assert!(result.is_ok());
    }

    // ─── CLI subcommand parsing ───────────────────────────────────────────────

    #[test]
    fn test_cli_parse_serve_stdio() {
        let cli = Cli::try_parse_from(["ahma", "serve", "stdio"]).unwrap();
        assert!(matches!(
            cli.command,
            Subcommands::Serve(ServeArgs {
                transport: Some(ServeTransport::Stdio(_)),
                ..
            })
        ));
    }

    #[test]
    fn test_cli_parse_serve_stdio_with_path() {
        let cli = Cli::try_parse_from(["ahma", "serve", "stdio", "/some/path"]).unwrap();
        if let Subcommands::Serve(ServeArgs {
            transport: Some(ServeTransport::Stdio(args)),
            ..
        }) = cli.command
        {
            assert_eq!(args.path, Some(PathBuf::from("/some/path")));
        } else {
            panic!("Expected ServeTransport::Stdio");
        }
    }

    #[test]
    fn test_cli_parse_serve_default_none() {
        let cli = Cli::try_parse_from(["ahma", "serve"]).unwrap();
        assert!(matches!(
            cli.command,
            Subcommands::Serve(ServeArgs {
                transport: None,
                ..
            })
        ));
    }

    #[test]
    fn test_cli_parse_serve_http_defaults() {
        let cli = Cli::try_parse_from(["ahma", "serve", "http"]).unwrap();
        assert!(!cli.disable_quic);
        if let Subcommands::Serve(ServeArgs {
            transport: Some(ServeTransport::Http(h)),
            ..
        }) = cli.command
        {
            assert_eq!(h.host, "127.0.0.1");
            assert_eq!(h.port, 3000);
        } else {
            panic!("expected serve http");
        }
    }

    #[test]
    fn test_cli_parse_serve_http_custom_port() {
        let cli = Cli::try_parse_from(["ahma", "serve", "http", "--port", "8080"]).unwrap();
        if let Subcommands::Serve(ServeArgs {
            transport: Some(ServeTransport::Http(h)),
            ..
        }) = cli.command
        {
            assert_eq!(h.port, 8080);
        } else {
            panic!("expected serve http");
        }
    }

    #[test]
    fn test_cli_parse_run_tool() {
        let cli =
            Cli::try_parse_from(["ahma", "tool", "run", "cargo_build", "--", "--release"]).unwrap();
        if let Subcommands::Tool(ToolArgs {
            command: ToolCommand::Run(r),
        }) = cli.command
        {
            assert_eq!(r.tool, "cargo_build");
            assert_eq!(r.tool_args, vec!["--release"]);
        } else {
            panic!("expected tool run subcommand");
        }
    }

    #[test]
    fn test_cli_parse_tool_validate_default() {
        let cli = Cli::try_parse_from(["ahma", "tool", "validate"]).unwrap();
        if let Subcommands::Tool(ToolArgs {
            command: ToolCommand::Validate(v),
        }) = cli.command
        {
            assert!(v.target.is_none());
        } else {
            panic!("expected tool validate");
        }
    }

    #[test]
    fn test_cli_parse_tool_validate_with_target() {
        let cli = Cli::try_parse_from(["ahma", "tool", "validate", ".ahma"]).unwrap();
        if let Subcommands::Tool(ToolArgs {
            command: ToolCommand::Validate(v),
        }) = cli.command
        {
            assert_eq!(v.target, Some(".ahma".to_string()));
        } else {
            panic!("expected tool validate with target");
        }
    }

    #[tokio::test]
    async fn test_dispatch_subcommand_tls_bails_for_custom_binary() {
        let err = dispatch_subcommand(
            Subcommands::Tls(TlsArgs {
                command: TlsCommand::Status,
            }),
            make_cfg(),
        )
        .await
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("tls commands are provided by the ahma_bin crate"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_cli_parse_tool_list() {
        let cli = Cli::try_parse_from(["ahma", "tool", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            Subcommands::Tool(ToolArgs {
                command: ToolCommand::List(_)
            })
        ));
    }

    #[test]
    fn test_cli_parse_serve_with_tool_bundle() {
        let cli =
            Cli::try_parse_from(["ahma", "serve", "stdio", "--tools", "rust,python"]).unwrap();
        assert!(cli.tool_bundles.contains(&"rust".to_string()));
        assert!(cli.tool_bundles.contains(&"python".to_string()));
    }

    #[test]
    fn test_cli_parse_update_defaults() {
        let cli = Cli::try_parse_from(["ahma", "update"]).unwrap();
        if let Subcommands::Update(args) = cli.command {
            assert!(args.reference.is_none());
            assert!(!args.force);
            assert!(!args.dry_run);
        } else {
            panic!("expected update subcommand");
        }
    }

    #[test]
    fn test_cli_parse_update_branch_ref() {
        let cli = Cli::try_parse_from(["ahma", "update", "feature/update"]).unwrap();
        if let Subcommands::Update(args) = cli.command {
            assert_eq!(args.reference.as_deref(), Some("feature/update"));
        } else {
            panic!("expected update subcommand");
        }
    }

    #[test]
    fn test_cli_parse_update_with_flags() {
        let cli = Cli::try_parse_from([
            "ahma",
            "update",
            "main",
            "--force",
            "--dry-run",
            "--install-dir",
            "/tmp/ahma-bin",
        ])
        .unwrap();
        if let Subcommands::Update(args) = cli.command {
            assert_eq!(args.reference.as_deref(), Some("main"));
            assert!(args.force);
            assert!(args.dry_run);
            assert_eq!(args.install_dir, Some("/tmp/ahma-bin".into()));
        } else {
            panic!("expected update subcommand");
        }
    }

    // ─── AppConfig::env_flag ─────────────────────────────────────────────────

    #[test]
    fn test_app_config_env_flag_via_helper() {
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe { std::env::set_var("AHMA_TEST_CFG_FLAG", "yes") };
        assert!(AppConfig::env_flag("AHMA_TEST_CFG_FLAG"));
        unsafe { std::env::remove_var("AHMA_TEST_CFG_FLAG") };
    }

    // ─── --sandbox flag / use_sandbox_dir ────────────────────────────────────

    /// --sandbox CLI flag is parsed to use_sandbox on Cli and threads into AppConfig.
    #[test]
    fn test_cli_parse_sandbox_flag() {
        let cli = Cli::try_parse_from(["ahma", "--sandbox", "serve", "stdio"]).unwrap();
        assert!(cli.use_sandbox, "--sandbox must set use_sandbox on Cli");
    }

    /// When CWD is inside the temp dir, resolve_sandbox_scopes falls back to
    /// sandbox_directory rather than locking to temp.
    #[test]
    fn test_resolve_sandbox_scopes_cwd_in_temp_uses_sandbox_directory() {
        init_test();
        let sandbox_dir_tmp = tempdir().unwrap();
        let cfg = AppConfig {
            no_sandbox: true,
            sandbox_scopes: vec![],
            sandbox_directory: Some(sandbox_dir_tmp.path().to_path_buf()),
            ..make_cfg()
        };

        // Temporarily set CWD to a path inside the system temp dir.
        let old_cwd = std::env::current_dir().unwrap();
        let temp_sub = tempdir().unwrap();
        let temp_sub_path = temp_sub.path().to_path_buf();
        std::env::set_current_dir(&temp_sub_path).unwrap();

        let scopes_result = resolve_sandbox_scopes(&cfg);
        std::env::set_current_dir(&old_cwd).unwrap();

        let scopes = scopes_result.unwrap().expect("must return some scopes");
        let expected_dir = dunce::canonicalize(sandbox_dir_tmp.path()).unwrap();
        assert_eq!(
            scopes,
            vec![expected_dir],
            "When CWD is in temp, must use sandbox_directory, not temp: {scopes:?}"
        );
    }

    /// When CWD is inside temp and no sandbox_directory is set, resolve_sandbox_scopes
    /// returns empty (waiting for roots/list) rather than locking to temp.
    #[test]
    fn test_resolve_sandbox_scopes_cwd_in_temp_no_sandbox_dir_returns_empty() {
        init_test();
        let cfg = AppConfig {
            no_sandbox: true,
            sandbox_scopes: vec![],
            sandbox_directory: None,
            ..make_cfg()
        };

        let old_cwd = std::env::current_dir().unwrap();
        let temp_sub = tempdir().unwrap();
        std::env::set_current_dir(temp_sub.path()).unwrap();

        let scopes_result = resolve_sandbox_scopes(&cfg);
        std::env::set_current_dir(&old_cwd).unwrap();

        let scopes = scopes_result.unwrap().expect("must return Some");
        assert!(
            scopes.is_empty(),
            "CWD-in-temp with no sandbox_directory must return empty: {scopes:?}"
        );
    }

    /// build_background_bridge_args does NOT include sandbox_scopes as --sandbox-scope
    /// when they are empty, and DOES forward --sandbox when use_sandbox_dir is set.
    #[test]
    fn test_build_background_bridge_args_forwards_sandbox_flag_not_resolved_scopes() {
        init_test();
        let tmp = tempdir().unwrap();
        let cfg = AppConfig {
            no_sandbox: true,
            sandbox_scopes: vec![], // no explicit scopes
            use_sandbox_dir: true,
            sandbox_directory: Some(tmp.path().to_path_buf()),
            ..make_cfg()
        };

        let args = super::super::modes::server::build_background_bridge_args(&cfg);
        let has_sandbox_scope = args.windows(2).any(|w| w[0] == "--sandbox-scope");
        assert!(
            !has_sandbox_scope,
            "empty sandbox_scopes must not produce --sandbox-scope in bridge args: {args:?}"
        );
        assert!(
            args.contains(&"--sandbox".to_string()),
            "--sandbox flag must be forwarded to bridge: {args:?}"
        );
    }

    /// build_background_bridge_args forwards explicit --sandbox-scope values but
    /// not the --sandbox flag when use_sandbox_dir is false.
    #[test]
    fn test_build_background_bridge_args_forwards_explicit_scope_only() {
        init_test();
        let tmp = tempdir().unwrap();
        let scope = tmp.path().to_path_buf();
        let cfg = AppConfig {
            no_sandbox: true,
            sandbox_scopes: vec![scope.clone()],
            use_sandbox_dir: false,
            ..make_cfg()
        };

        let args = super::super::modes::server::build_background_bridge_args(&cfg);
        let scope_idx = args
            .iter()
            .position(|a| a == "--sandbox-scope")
            .expect("explicit scope must be forwarded");
        let expected = scope.to_string_lossy().into_owned();
        assert!(
            args[scope_idx + 1].contains(expected.as_str()),
            "scope value must be present after --sandbox-scope: {args:?}"
        );
        assert!(
            !args.contains(&"--sandbox".to_string()),
            "--sandbox must not appear when use_sandbox_dir is false: {args:?}"
        );
    }

    // ─── CWD is never inferred as a scope (SPEC R5.2.1) ───────────────────────

    #[test]
    fn resolve_sandbox_scopes_ignores_cwd_and_markers() {
        // SPEC R5.2.1: the launch CWD must never become a sandbox scope by
        // inference — with or without project-marker files. With no explicit
        // scope configured, resolution must fall to the declared default
        // sandbox_directory (R5.2.3), NOT the current working directory — even
        // though the test process CWD (the crate dir) contains a Cargo.toml
        // marker that the old `is_plausible_workspace` heuristic would accept.
        let tmp = tempdir().unwrap();
        let mut cfg = make_cfg();
        cfg.sandbox_directory = Some(tmp.path().to_path_buf());

        let scopes = resolve_sandbox_scopes(&cfg).unwrap().unwrap();

        let expected = dunce::canonicalize(tmp.path()).unwrap();
        assert_eq!(
            scopes,
            vec![expected],
            "scope must be the default sandbox_directory, not the CWD"
        );
        let cwd = dunce::canonicalize(std::env::current_dir().unwrap()).unwrap();
        assert!(
            !scopes.contains(&cwd),
            "the launch CWD must never be used as a sandbox scope (R5.2.1)"
        );
    }

    #[test]
    fn resolve_sandbox_scopes_no_default_awaits_roots() {
        // SPEC R5.2.3 / R5.2: with neither an explicit scope nor a configured
        // sandbox_directory, resolution yields an empty provisional scope so
        // the server awaits the client's roots/list — never the CWD.
        let mut cfg = make_cfg();
        cfg.sandbox_directory = None;
        let scopes = resolve_sandbox_scopes(&cfg).unwrap().unwrap();
        assert!(
            scopes.is_empty(),
            "expected empty provisional scope awaiting roots/list, got {scopes:?}"
        );
    }

    // ─── split_version_and_build_id ──────────────────────────────────────────

    #[test]
    fn test_split_version_and_build_id_with_id() {
        let (semver, build_id) =
            super::super::modes::server::split_version_and_build_id("0.12.5+abc1234");
        assert_eq!(semver, "0.12.5");
        assert_eq!(build_id, Some("abc1234"));
    }

    #[test]
    fn test_split_version_and_build_id_without_id() {
        let (semver, build_id) = super::super::modes::server::split_version_and_build_id("0.12.5");
        assert_eq!(semver, "0.12.5");
        assert_eq!(build_id, None);
    }

    // ─── expand_tilde ────────────────────────────────────────────────────────

    #[test]
    fn test_expand_tilde_bare_tilde_is_home() {
        let home = dirs::home_dir().expect("home dir resolvable in test env");
        let expanded = expand_tilde(PathBuf::from("~"));
        assert_eq!(expanded, home, "bare ~ must expand to the home directory");
    }

    #[test]
    fn test_expand_tilde_with_slash_subpath() {
        let home = dirs::home_dir().expect("home dir resolvable in test env");
        let expanded = expand_tilde(PathBuf::from("~/projects/foo"));
        assert_eq!(expanded, home.join("projects/foo"));
    }

    #[test]
    fn test_expand_tilde_with_backslash_subpath() {
        // The `~\sub` branch is matched by a literal backslash and is pure string
        // logic, so it behaves identically on every platform.
        let home = dirs::home_dir().expect("home dir resolvable in test env");
        let expanded = expand_tilde(PathBuf::from("~\\sub"));
        assert_eq!(expanded, home.join("sub"));
    }

    #[test]
    fn test_expand_tilde_non_tilde_path_unchanged() {
        let p = PathBuf::from("/absolute/no/tilde");
        assert_eq!(expand_tilde(p.clone()), p, "absolute paths pass through");
    }

    #[test]
    fn test_expand_tilde_tilde_user_not_expanded() {
        // `~user` (no separator) is NOT a home reference and must pass through.
        let p = PathBuf::from("~someuser/dir");
        assert_eq!(expand_tilde(p.clone()), p);
    }

    // ─── resolve_persistent_scopes ───────────────────────────────────────────

    fn pscope(
        path: PathBuf,
        access: ahma_common::config::ScopeAccess,
    ) -> ahma_common::config::PersistentScope {
        ahma_common::config::PersistentScope {
            path,
            access,
            granted_by: Some("test".to_string()),
            granted_at: None,
            note: None,
        }
    }

    #[test]
    fn test_resolve_persistent_scopes_rw_creates_and_canonicalizes() {
        init_test();
        use ahma_common::config::ScopeAccess;
        let tmp = tempdir().unwrap();
        let rw_dir = tmp.path().join("rw-cache");
        assert!(!rw_dir.exists());
        let cfg = AppConfig {
            persistent_scopes: vec![pscope(rw_dir.clone(), ScopeAccess::Rw)],
            ..make_cfg()
        };
        let (write, read) = resolve_persistent_scopes(&cfg);
        assert!(rw_dir.exists(), "rw scope dir must be auto-created");
        assert_eq!(write, vec![dunce::canonicalize(&rw_dir).unwrap()]);
        assert!(read.is_empty());
    }

    #[test]
    fn test_resolve_persistent_scopes_rw_dedupes() {
        init_test();
        use ahma_common::config::ScopeAccess;
        let tmp = tempdir().unwrap();
        let rw_dir = tmp.path().join("rw-cache");
        let cfg = AppConfig {
            persistent_scopes: vec![
                pscope(rw_dir.clone(), ScopeAccess::Rw),
                pscope(rw_dir.clone(), ScopeAccess::Rw),
            ],
            ..make_cfg()
        };
        let (write, _read) = resolve_persistent_scopes(&cfg);
        assert_eq!(write.len(), 1, "duplicate rw scope must be deduped");
    }

    #[test]
    fn test_resolve_persistent_scopes_ro_existing_canonicalized() {
        init_test();
        use ahma_common::config::ScopeAccess;
        let tmp = tempdir().unwrap();
        let ro_dir = tmp.path().join("ro-data");
        std::fs::create_dir_all(&ro_dir).unwrap();
        let cfg = AppConfig {
            persistent_scopes: vec![pscope(ro_dir.clone(), ScopeAccess::Ro)],
            ..make_cfg()
        };
        let (write, read) = resolve_persistent_scopes(&cfg);
        assert!(write.is_empty());
        assert_eq!(read, vec![dunce::canonicalize(&ro_dir).unwrap()]);
    }

    #[test]
    fn test_resolve_persistent_scopes_ro_missing_skipped() {
        init_test();
        use ahma_common::config::ScopeAccess;
        let tmp = tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist-ro");
        let cfg = AppConfig {
            persistent_scopes: vec![pscope(missing, ScopeAccess::Ro)],
            ..make_cfg()
        };
        let (write, read) = resolve_persistent_scopes(&cfg);
        assert!(write.is_empty());
        assert!(
            read.is_empty(),
            "a missing read-only scope must be skipped, never created"
        );
    }

    // ─── ensure_task_vault_layout ────────────────────────────────────────────

    #[test]
    fn test_ensure_task_vault_layout_creates_tree_and_returns_workdir() {
        init_test();
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("vault");
        let workdir = ensure_task_vault_layout(&root).unwrap();
        assert_eq!(workdir, dunce::canonicalize(root.join("workdir")).unwrap());
        assert!(root.join("inputs").is_dir());
        assert!(root.join("workdir").is_dir());
        assert!(root.join("outputs").is_dir());
        assert!(root.join("trash").is_dir());
        assert!(root.join("audit.jsonl").is_file());
    }

    #[test]
    fn test_ensure_task_vault_layout_preserves_existing_audit_log() {
        init_test();
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("vault");
        // First call creates the layout.
        ensure_task_vault_layout(&root).unwrap();
        // Write content into the audit log.
        let audit = root.join("audit.jsonl");
        std::fs::write(&audit, b"existing-entry\n").unwrap();
        // Second call must NOT truncate the existing audit log (idempotent).
        ensure_task_vault_layout(&root).unwrap();
        let contents = std::fs::read_to_string(&audit).unwrap();
        assert_eq!(
            contents, "existing-entry\n",
            "an existing audit log must be preserved, not overwritten"
        );
    }

    // ─── extract_serve_fields ────────────────────────────────────────────────

    #[test]
    fn test_extract_serve_fields_http_custom() {
        let cmd = Cli::parse_from([
            "ahma", "serve", "http", "--host", "0.0.0.0", "--port", "9999",
        ])
        .command;
        let fields = extract_serve_fields(&cmd);
        assert_eq!(fields.http_host, "0.0.0.0");
        assert_eq!(fields.http_port, 9999);
    }

    #[test]
    fn test_extract_serve_fields_stdio_defaults() {
        let cmd = Cli::parse_from(["ahma", "serve", "stdio"]).command;
        let fields = extract_serve_fields(&cmd);
        assert_eq!(fields.http_host, "127.0.0.1");
        assert_eq!(fields.http_port, 3000);
    }

    #[test]
    fn test_extract_serve_fields_serve_none_defaults() {
        let cmd = Cli::parse_from(["ahma", "serve"]).command;
        let fields = extract_serve_fields(&cmd);
        assert_eq!(fields.http_host, "127.0.0.1");
        assert_eq!(fields.http_port, 3000);
    }

    #[test]
    fn test_extract_serve_fields_non_serve_defaults() {
        let cmd = Cli::parse_from(["ahma", "tool", "list"]).command;
        let fields = extract_serve_fields(&cmd);
        assert_eq!(fields.http_host, "127.0.0.1");
        assert_eq!(fields.http_port, 3000);
    }

    // ─── extract_tool_fields / default_tool_fields ───────────────────────────

    #[test]
    fn test_extract_tool_fields_list_with_server_args() {
        let cmd = Cli::parse_from(["ahma", "tool", "list", "--", "./server", "-x", "y"]).command;
        let fields = extract_tool_fields(&cmd);
        assert_eq!(fields.run_tool.as_deref(), Some("./server"));
        assert_eq!(
            fields.run_tool_args,
            vec!["-x".to_string(), "y".to_string()]
        );
    }

    #[test]
    fn test_extract_tool_fields_list_with_server_and_format() {
        let cmd = Cli::parse_from([
            "ahma", "tool", "list", "--server", "foo", "--format", "json",
        ])
        .command;
        let fields = extract_tool_fields(&cmd);
        assert_eq!(fields.list_server.as_deref(), Some("foo"));
        assert!(matches!(fields.list_format, list_tools::OutputFormat::Json));
        assert!(fields.run_tool.is_none());
    }

    #[test]
    fn test_extract_tool_fields_run() {
        let cmd = Cli::parse_from(["ahma", "tool", "run", "mytool", "--", "-a"]).command;
        let fields = extract_tool_fields(&cmd);
        assert_eq!(fields.run_tool.as_deref(), Some("mytool"));
        assert_eq!(fields.run_tool_args, vec!["-a".to_string()]);
        assert_eq!(fields.mcp_config, PathBuf::from("mcp.json"));
    }

    #[test]
    fn test_extract_tool_fields_validate_falls_back_to_defaults() {
        let cmd = Cli::parse_from(["ahma", "tool", "validate"]).command;
        let fields = extract_tool_fields(&cmd);
        assert!(fields.run_tool.is_none());
        assert!(fields.list_server.is_none());
        assert!(fields.list_http.is_none());
        assert_eq!(fields.mcp_config, PathBuf::from("mcp.json"));
    }

    #[test]
    fn test_extract_tool_fields_non_tool_command_defaults() {
        let cmd = Cli::parse_from(["ahma", "serve", "stdio"]).command;
        let fields = extract_tool_fields(&cmd);
        assert!(fields.run_tool.is_none());
        assert_eq!(fields.mcp_config, PathBuf::from("mcp.json"));
    }

    // ─── load_settings ───────────────────────────────────────────────────────

    #[test]
    fn test_load_settings_no_settings_returns_defaults() {
        let cli = Cli::parse_from(["ahma", "--no-settings", "serve", "stdio"]);
        let s = load_settings(&cli);
        assert_eq!(s.tools.timeout_secs, 600);
    }

    #[test]
    fn test_load_settings_explicit_path_parses_file() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("custom-settings.toml");
        std::fs::write(&path, "[tools]\ntimeout_secs = 1234\n").unwrap();
        let cli = Cli::parse_from([
            "ahma",
            "--settings-path",
            path.to_str().unwrap(),
            "serve",
            "stdio",
        ]);
        let s = load_settings(&cli);
        assert_eq!(s.tools.timeout_secs, 1234);
    }

    #[test]
    fn test_load_settings_explicit_missing_path_returns_defaults() {
        let tmp = tempdir().unwrap();
        let missing = tmp.path().join("nope.toml");
        let cli = Cli::parse_from([
            "ahma",
            "--settings-path",
            missing.to_str().unwrap(),
            "serve",
            "stdio",
        ]);
        let s = load_settings(&cli);
        assert_eq!(
            s.tools.timeout_secs, 600,
            "a missing settings file is not an error; defaults are used"
        );
    }

    // ─── parse_execution_settings ────────────────────────────────────────────

    #[test]
    fn test_parse_execution_settings_from_settings() {
        init_test();
        let cli = Cli::parse_from(["ahma", "serve", "stdio"]);
        let mut s = ahma_common::config::AhmaSettings::default();
        s.tools.timeout_secs = 42;
        s.tools.force_sync = true;
        s.tools.hot_reload = true;
        s.tools.skip_probes = true;
        let (timeout, sync, hot, skip) = parse_execution_settings(&cli, &s);
        assert_eq!(timeout, 42);
        assert!(sync && hot && skip);
    }

    #[test]
    fn test_parse_execution_settings_cli_overrides() {
        init_test();
        let cli = Cli::parse_from([
            "ahma",
            "--timeout",
            "120",
            "--sync",
            "--hot-reload",
            "--skip-probes",
            "serve",
            "stdio",
        ]);
        let s = ahma_common::config::AhmaSettings::default();
        let (timeout, sync, hot, skip) = parse_execution_settings(&cli, &s);
        assert_eq!(timeout, 120);
        assert!(sync && hot && skip);
    }

    // ─── parse_sandbox_settings ──────────────────────────────────────────────

    #[test]
    fn test_parse_sandbox_settings_from_settings() {
        let _guard = ENV_MUTEX.lock().unwrap();
        init_test();
        let cli = Cli::parse_from(["ahma", "serve", "stdio"]);
        let mut s = ahma_common::config::AhmaSettings::default();
        s.sandbox.disable = true;
        s.sandbox.defer = true;
        s.sandbox.tmp_access = true;
        s.sandbox.use_sandbox_directory = true;
        s.sandbox.disable_temp = true;
        s.logging.log_monitor = true;
        s.logging.monitor_rate_limit_secs = 30;
        s.sandbox.package_cache_write = true;
        let (no_sandbox, defer, tmp, use_dir, no_temp, mon, rate, pkg) =
            parse_sandbox_settings(&cli, &s);
        assert!(no_sandbox && defer && tmp && use_dir && no_temp && mon);
        assert_eq!(rate, 30);
        assert!(pkg, "package_cache_write follows settings when no CLI flag");
    }

    #[test]
    fn test_parse_sandbox_settings_cli_overrides_and_pkg_cache_flag() {
        let _guard = ENV_MUTEX.lock().unwrap();
        init_test();
        let cli = Cli::parse_from([
            "ahma",
            "--defer-sandbox",
            "--tmp",
            "--sandbox",
            "--disable-temp-files",
            "--log-monitor",
            "--monitor-rate-limit",
            "15",
            "--no-package-cache-write",
            "serve",
            "stdio",
        ]);
        let s = ahma_common::config::AhmaSettings::default();
        let (_no_sandbox, defer, tmp, use_dir, no_temp, mon, rate, pkg) =
            parse_sandbox_settings(&cli, &s);
        assert!(defer && tmp && use_dir && no_temp && mon);
        assert_eq!(rate, 15);
        assert!(
            !pkg,
            "--no-package-cache-write must disable package cache writes"
        );
    }

    // ─── parse_http_settings ─────────────────────────────────────────────────

    #[test]
    fn test_parse_http_settings_from_settings() {
        init_test();
        let cli = Cli::parse_from(["ahma", "serve", "http"]);
        let mut s = ahma_common::config::AhmaSettings::default();
        s.http.disable_quic = true;
        s.http.disable_http1_1 = true;
        s.http.handshake_timeout_secs = 99;
        let (no_quic, no_h1, hs) = parse_http_settings(&cli, &s);
        assert!(no_quic && no_h1);
        assert_eq!(hs, 99);
    }

    #[test]
    fn test_parse_http_settings_cli_overrides() {
        init_test();
        let cli = Cli::parse_from([
            "ahma",
            "--disable-quic",
            "--disable-http1-1",
            "--handshake-timeout",
            "7",
            "serve",
            "http",
        ]);
        let s = ahma_common::config::AhmaSettings::default();
        let (no_quic, no_h1, hs) = parse_http_settings(&cli, &s);
        assert!(no_quic && no_h1);
        assert_eq!(hs, 7);
    }

    // ─── parse_auth_settings ─────────────────────────────────────────────────

    #[test]
    fn test_parse_auth_settings_from_settings() {
        let _guard = ENV_MUTEX.lock().unwrap();
        init_test();
        let cli = Cli::parse_from(["ahma", "serve", "http"]);
        let mut s = ahma_common::config::AhmaSettings::default();
        s.auth.require_token = Some("secret".to_string());
        s.auth.require_token_path = "/etc/token".to_string();
        s.auth.rate_limit_rps = 50;
        s.auth.rate_limit_burst = 25;
        s.instance.label = "worker-7".to_string();
        let (tok, tok_path, rps, burst, label) = parse_auth_settings(&cli, &s);
        assert_eq!(tok.as_deref(), Some("secret"));
        assert_eq!(tok_path, Some(PathBuf::from("/etc/token")));
        assert_eq!(rps, 50);
        assert_eq!(burst, 25);
        assert_eq!(label, "worker-7");
    }

    #[test]
    fn test_parse_auth_settings_empty_token_path_is_none() {
        let _guard = ENV_MUTEX.lock().unwrap();
        init_test();
        let cli = Cli::parse_from(["ahma", "serve", "http"]);
        let s = ahma_common::config::AhmaSettings::default();
        let (tok, tok_path, rps, burst, label) = parse_auth_settings(&cli, &s);
        assert!(tok.is_none());
        assert!(
            tok_path.is_none(),
            "an empty require_token_path must resolve to None"
        );
        assert_eq!(rps, 0);
        assert_eq!(burst, 10);
        assert_eq!(label, "ahma");
    }

    #[test]
    fn test_parse_auth_settings_cli_overrides() {
        let _guard = ENV_MUTEX.lock().unwrap();
        init_test();
        let cli = Cli::parse_from([
            "ahma",
            "--require-token",
            "clitoken",
            "--require-token-path",
            "/cli/token",
            "--rate-limit-rps",
            "3",
            "--rate-limit-burst",
            "9",
            "--instance-label",
            "cli-label",
            "serve",
            "http",
        ]);
        let s = ahma_common::config::AhmaSettings::default();
        let (tok, tok_path, rps, burst, label) = parse_auth_settings(&cli, &s);
        assert_eq!(tok.as_deref(), Some("clitoken"));
        assert_eq!(tok_path, Some(PathBuf::from("/cli/token")));
        assert_eq!(rps, 3);
        assert_eq!(burst, 9);
        assert_eq!(label, "cli-label");
    }

    // ─── resolve_tool_bundles ────────────────────────────────────────────────

    #[test]
    fn test_resolve_tool_bundles_cli_dedupes() {
        let cli = Cli::parse_from(["ahma", "--tools", "rust,rust,python", "serve", "stdio"]);
        let s = ahma_common::config::AhmaSettings::default();
        let bundles = resolve_tool_bundles(&cli, &s);
        assert_eq!(bundles, vec!["rust".to_string(), "python".to_string()]);
    }

    #[test]
    fn test_resolve_tool_bundles_falls_back_to_settings() {
        let cli = Cli::parse_from(["ahma", "serve", "stdio"]);
        let mut s = ahma_common::config::AhmaSettings::default();
        s.tools.tool_bundles = vec!["git".to_string(), "git".to_string(), "kotlin".to_string()];
        let bundles = resolve_tool_bundles(&cli, &s);
        assert_eq!(bundles, vec!["git".to_string(), "kotlin".to_string()]);
    }

    #[test]
    fn test_resolve_tool_bundles_empty_when_none() {
        let cli = Cli::parse_from(["ahma", "serve", "stdio"]);
        let s = ahma_common::config::AhmaSettings::default();
        assert!(resolve_tool_bundles(&cli, &s).is_empty());
    }

    // ─── resolve_sandbox_scopes_cli / resolve_working_dirs_cli ───────────────

    #[test]
    fn test_resolve_sandbox_scopes_cli_expands_tilde() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let home = dirs::home_dir().expect("home dir");
        let cli = Cli::parse_from(["ahma", "--sandbox-scope", "~/foo", "serve", "stdio"]);
        let s = ahma_common::config::AhmaSettings::default();
        let scopes = resolve_sandbox_scopes_cli(&cli, &s);
        assert_eq!(scopes, vec![home.join("foo")]);
    }

    #[test]
    fn test_resolve_sandbox_scopes_cli_settings_fallback() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let home = dirs::home_dir().expect("home dir");
        let cli = Cli::parse_from(["ahma", "serve", "stdio"]);
        let mut s = ahma_common::config::AhmaSettings::default();
        s.sandbox.scopes = vec![PathBuf::from("~/bar")];
        let scopes = resolve_sandbox_scopes_cli(&cli, &s);
        assert_eq!(scopes, vec![home.join("bar")]);
    }

    #[test]
    fn test_resolve_working_dirs_cli_expands_tilde() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let home = dirs::home_dir().expect("home dir");
        let cli = Cli::parse_from(["ahma", "--working-dir", "~/wd", "serve", "stdio"]);
        let s = ahma_common::config::AhmaSettings::default();
        let dirs_ = resolve_working_dirs_cli(&cli, &s);
        assert_eq!(dirs_, vec![home.join("wd")]);
    }

    #[test]
    fn test_resolve_working_dirs_cli_settings_fallback() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let cli = Cli::parse_from(["ahma", "serve", "stdio"]);
        let mut s = ahma_common::config::AhmaSettings::default();
        s.sandbox.working_dirs = vec![PathBuf::from("/abs/wd")];
        let dirs_ = resolve_working_dirs_cli(&cli, &s);
        assert_eq!(dirs_, vec![PathBuf::from("/abs/wd")]);
    }

    // ─── unix_socket_path_from_cli ───────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn test_unix_socket_path_cli_flag_wins() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let cli = Cli::parse_from(["ahma", "--unix-socket-path", "/x/y.sock", "serve", "stdio"]);
        let s = ahma_common::config::AhmaSettings::default();
        assert_eq!(unix_socket_path_from_cli(&cli, &s), "/x/y.sock");
    }

    #[cfg(unix)]
    #[test]
    fn test_unix_socket_path_serve_unix_socket_path() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let cli = Cli::parse_from(["ahma", "serve", "unix", "--socket-path", "/a/b.sock"]);
        let s = ahma_common::config::AhmaSettings::default();
        assert_eq!(unix_socket_path_from_cli(&cli, &s), "/a/b.sock");
    }

    #[cfg(unix)]
    #[test]
    fn test_unix_socket_path_serve_unix_settings_fallback() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let cli = Cli::parse_from(["ahma", "serve", "unix"]);
        let mut s = ahma_common::config::AhmaSettings::default();
        s.http.unix_socket_path = Some("/from/settings.sock".to_string());
        assert_eq!(unix_socket_path_from_cli(&cli, &s), "/from/settings.sock");
    }

    #[cfg(unix)]
    #[test]
    fn test_unix_socket_path_default_when_unset() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let cli = Cli::parse_from(["ahma", "serve", "http"]);
        let s = ahma_common::config::AhmaSettings::default();
        assert_eq!(unix_socket_path_from_cli(&cli, &s), "/tmp/ahma.sock");
    }

    // ─── build_app_config ────────────────────────────────────────────────────

    #[test]
    fn test_build_app_config_defaults_with_no_settings() {
        let _guard = ENV_MUTEX.lock().unwrap();
        init_test();
        unsafe { std::env::remove_var("AHMA_SERVER_CHILD") };
        let cli = Cli::parse_from(["ahma", "--no-settings", "serve", "stdio"]);
        let cfg = build_app_config(&cli);
        assert_eq!(cfg.timeout_secs, 600);
        assert!(!cfg.force_sync);
        assert_eq!(cfg.http_host, "127.0.0.1");
        assert_eq!(cfg.http_port, 3000);
        assert!(!cfg.is_server_child);
        assert_eq!(cfg.max_sessions, 10);
        assert!(!cfg.explicit_tools_dir);
    }

    #[test]
    fn test_build_app_config_threads_cli_flags() {
        let _guard = ENV_MUTEX.lock().unwrap();
        init_test();
        unsafe { std::env::remove_var("AHMA_SERVER_CHILD") };
        let cli = Cli::parse_from([
            "ahma",
            "--no-settings",
            "--timeout",
            "222",
            "--sync",
            "--max-sessions",
            "3",
            "--server-child",
            "serve",
            "http",
            "--port",
            "8081",
        ]);
        let cfg = build_app_config(&cli);
        assert_eq!(cfg.timeout_secs, 222);
        assert!(cfg.force_sync);
        assert_eq!(cfg.http_port, 8081);
        assert_eq!(cfg.max_sessions, 3);
        assert!(
            cfg.is_server_child,
            "--server-child must set is_server_child"
        );
    }

    #[test]
    fn test_build_app_config_explicit_tools_dir() {
        let _guard = ENV_MUTEX.lock().unwrap();
        init_test();
        unsafe { std::env::remove_var("AHMA_SERVER_CHILD") };
        let tmp = tempdir().unwrap();
        let cli = Cli::parse_from([
            "ahma",
            "--no-settings",
            "--tools-dir",
            tmp.path().to_str().unwrap(),
            "serve",
            "stdio",
        ]);
        let cfg = build_app_config(&cli);
        assert!(
            cfg.explicit_tools_dir,
            "--tools-dir must mark the tools dir explicit"
        );
    }

    // ─── dispatch_subcommand bail arms (crate-split stubs) ────────────────────

    #[tokio::test]
    async fn test_dispatch_subcommand_vault_bails() {
        let err = dispatch_subcommand(
            Subcommands::Vault(VaultArgs {
                command: VaultCommand::List,
            }),
            make_cfg(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("vault commands are provided"));
    }

    #[tokio::test]
    async fn test_dispatch_subcommand_tui_bails() {
        let err = dispatch_subcommand(
            Subcommands::Tui(TuiArgs {
                connect: None,
                profile: None,
                path: None,
            }),
            make_cfg(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("tui is provided"));
    }

    #[tokio::test]
    async fn test_dispatch_subcommand_llm_bails() {
        let err = dispatch_subcommand(
            Subcommands::Llm(LlmArgs {
                command: LlmCommand::List,
            }),
            make_cfg(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("llm commands are provided"));
    }

    #[tokio::test]
    async fn test_dispatch_subcommand_cluster_bails() {
        let err = dispatch_subcommand(
            Subcommands::Cluster(ClusterArgs {
                tls_dir: None,
                command: ClusterCommand::List,
            }),
            make_cfg(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("cluster commands are provided"));
    }

    #[tokio::test]
    async fn test_dispatch_subcommand_daemon_bails() {
        let err = dispatch_subcommand(Subcommands::Daemon(DaemonArgs {}), make_cfg())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("daemon is provided"));
    }
}
