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
//! ahma hooks install [--platform cursor,claude,codex] [--scope user|project]
//! ahma update [REF] [--force] [--dry-run] [--install-dir PATH]
//! ahma verify [PATH] [--self]
//! ```
//!
//! Niche options that rarely need changing are controlled via environment variables.
//! See `docs/environment-variables.md` for the full reference.

use super::{list_tools, modes, resolution};

use crate::{sandbox, utils::logging::init_logging_with_observability};
use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use dunce;
use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
    sync::Arc,
};

// ─────────────────────────────────────────────────────────────────────────────
// AppConfig — single immutable application configuration
//
// Built once from CLI args + env vars, then passed as a shared reference.
// Never mutated after startup.
// ─────────────────────────────────────────────────────────────────────────────

/// Controls the initial tool-visibility profile for the MCP session.
///
/// In all profiles, built-in tools (`run_terminal_command`, `await`, `status`,
/// `activate_tools`) are always visible.  The profile determines whether
/// CLI-flagged bundles are auto-revealed at startup or kept hidden until an
/// explicit `activate_tools reveal` call.
///
/// Controlled by the `AHMA_REVEAL_PROFILE` environment variable:
///
/// | Value      | Profile          |
/// |------------|------------------|
/// | `minimal`  | [`Minimal`]      |
/// | `balanced` | [`Balanced`]     |
/// | `full`     | [`Full`]         |
/// | (absent)   | [`Minimal`]      |
///
/// [`Minimal`]: StartupProfile::Minimal
/// [`Balanced`]: StartupProfile::Balanced
/// [`Full`]: StartupProfile::Full
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum StartupProfile {
    /// **Default.** Only built-in tools visible initially; CLI-flagged bundles
    /// remain hidden until the LLM calls `activate_tools reveal <bundle>`.
    ///
    /// Best for local/small LLMs (Gemma, Qwen) and context-sensitive workflows.
    #[default]
    Minimal,
    /// Built-in tools visible; CLI-flagged bundles auto-revealed at startup.
    /// Other bundles still require an `activate_tools reveal` call.
    ///
    /// Equivalent to the previous default behaviour (`--tools rust` immediately
    /// exposed cargo tools in the tool list).
    Balanced,
    /// All loaded tools visible immediately; progressive disclosure disabled.
    ///
    /// Equivalent to `AHMA_PROGRESSIVE_DISCLOSURE_OFF=1`.  Useful when the
    /// client is a large-context model and tool-count does not matter.
    Full,
}

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
    /// Default command timeout in seconds (AHMA_TIMEOUT, default 360).
    pub timeout_secs: u64,
    /// Run all tools synchronously (AHMA_SYNC=1).
    pub force_sync: bool,
    /// Reload tools from disk when `.ahma/` changes (AHMA_HOT_RELOAD=1).
    pub hot_reload_tools: bool,
    /// Skip tool availability probes at startup (AHMA_SKIP_PROBES=1).
    pub skip_availability_probes: bool,
    /// Enable progressive disclosure (AHMA_PROGRESSIVE_DISCLOSURE=1).
    pub progressive_disclosure: bool,
    /// Startup visibility profile (AHMA_REVEAL_PROFILE: minimal|balanced|full).
    pub reveal_profile: StartupProfile,

    // ── Sandbox ─────────────────────────────────────────────────────────────
    /// Disable the kernel sandbox entirely (AHMA_DISABLE_SANDBOX=1).
    pub no_sandbox: bool,
    /// Explicit sandbox scope directories (from AHMA_SANDBOX_SCOPE).
    pub sandbox_scopes: Vec<PathBuf>,
    /// Defer sandbox lock until client provides roots/list (AHMA_SANDBOX_DEFER=1).
    pub defer_sandbox: bool,
    /// Working directories seeded when defer mode lacks client roots (AHMA_WORKING_DIRS).
    pub working_dirs: Vec<PathBuf>,
    /// Add system temp dir to sandbox scopes (AHMA_TMP_ACCESS=1).
    pub tmp_access: bool,
    /// Block writes to temp directories (AHMA_DISABLE_TEMP=1).
    pub no_temp_files: bool,
    /// Enable live-log monitoring mode (AHMA_LOG_MONITOR=1).
    pub log_monitor: bool,
    /// Minimum seconds between log-monitor alerts (AHMA_MONITOR_RATE_LIMIT, default 60).
    pub monitor_rate_limit_secs: u64,

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
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            tools_dir: None,
            explicit_tools_dir: false,
            tool_bundles: vec![],
            timeout_secs: 360,
            force_sync: false,
            hot_reload_tools: false,
            skip_availability_probes: false,
            progressive_disclosure: true,
            reveal_profile: StartupProfile::Minimal,
            no_sandbox: false,
            sandbox_scopes: vec![],
            defer_sandbox: false,
            working_dirs: vec![],
            tmp_access: false,
            no_temp_files: false,
            log_monitor: false,
            monitor_rate_limit_secs: 60,
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

    /// Read a u64 env var, returning `default` if absent or unparseable.
    fn env_u64(name: &str, default: u64) -> u64 {
        std::env::var(name)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(default)
    }

    /// Parse `AHMA_SANDBOX_SCOPE` using the platform path-list separator.
    fn env_sandbox_scopes() -> Vec<PathBuf> {
        std::env::var_os("AHMA_SANDBOX_SCOPE")
            .map(|paths| {
                std::env::split_paths(&paths)
                    .filter(|path| !path.as_os_str().is_empty())
                    .map(expand_tilde)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Parse `AHMA_WORKING_DIRS` using the platform path-list separator.
    fn env_working_dirs() -> Vec<PathBuf> {
        std::env::var_os("AHMA_WORKING_DIRS")
            .map(|paths| {
                std::env::split_paths(&paths)
                    .filter(|path| !path.as_os_str().is_empty())
                    .map(expand_tilde)
                    .collect()
            })
            .unwrap_or_default()
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
        tracing::warn!("Ahma sandbox disabled via AHMA_DISABLE_SANDBOX or --serve flag");
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
        return Ok(Some(vec![workdir]));
    }

    if cfg.defer_sandbox {
        return resolve_deferred_scopes(cfg);
    }

    if !cfg.sandbox_scopes.is_empty() {
        let scopes = canonicalize_paths(&cfg.sandbox_scopes, "sandbox scope")?;
        return Ok(Some(scopes));
    }

    let cwd = std::env::current_dir()
        .context("Failed to get current working directory for sandbox scope")?;
    Ok(Some(vec![cwd]))
}

fn resolve_deferred_scopes(cfg: &AppConfig) -> Result<Option<Vec<PathBuf>>> {
    if !cfg.working_dirs.is_empty() {
        let scopes = canonicalize_paths(&cfg.working_dirs, "working directory")?;
        tracing::info!("Sandbox initialized from AHMA_WORKING_DIRS: {:?}", scopes);
        Ok(Some(scopes))
    } else {
        tracing::info!("Sandbox initialization deferred - will be set from client roots/list");
        Ok(Some(Vec::new()))
    }
}

fn add_temp_scope_if_requested(
    scopes: Option<Vec<PathBuf>>,
    tmp_access: bool,
) -> Option<Vec<PathBuf>> {
    if !tmp_access {
        return scopes;
    }

    let mut scopes = scopes?;

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

fn create_sandbox_instance(
    scopes: Option<Vec<PathBuf>>,
    policy: &SandboxPolicy,
    cfg: &AppConfig,
) -> Result<Option<Arc<sandbox::Sandbox>>> {
    let Some(scopes) = scopes else {
        return Ok(None);
    };

    let s = sandbox::Sandbox::new(
        scopes.clone(),
        policy.mode,
        cfg.no_temp_files,
        cfg.log_monitor,
        policy.tmp_access,
    )
    .context("Failed to initialize sandbox")?;

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
                sandbox.read_scopes(),
                sandbox.is_no_temp_files(),
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
        ServeTransport::Stdio => {
            let sandbox = initialize_sandbox(&cfg)?;
            let sandbox =
                sandbox.ok_or_else(|| anyhow!("Sandbox failed to initialize for stdio mode"))?;
            check_stdio_not_interactive()?;
            tracing::info!("Running in STDIO server mode");
            modes::run_server_mode(cfg, sandbox).await
        }
        ServeTransport::Http(_) => {
            tracing::info!("Running in HTTP bridge mode");
            modes::run_http_bridge_mode(cfg).await
        }
        #[cfg(unix)]
        ServeTransport::Unix(u) => {
            let path = u.socket_path.as_deref().unwrap_or("/tmp/ahma.sock");
            tracing::info!("Running in Unix socket bridge mode on {}", path);
            modes::run_unix_bridge_mode(cfg).await
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
            crate::update::run(args).await
        }
        Subcommands::Verify(args) => {
            tracing::info!("Running in verify mode");
            crate::update::verify::run_cli(args).await
        }
        Subcommands::Setup(args) => {
            tracing::info!("Running in setup mode");
            crate::setup::run(args).await
        }
        Subcommands::Daemon(_) => {
            anyhow::bail!(
                "daemon is provided by the ahma_bin crate. \
                 If you are running a custom binary, implement daemon dispatch \
                 using ahma_common::daemon_hub::run_daemon."
            )
        }
    }
}

fn audit_bundle_command(audit_args: BundleAuditArgs) -> Result<()> {
    println!("Auditing bundle: {}", audit_args.path.display());
    let result =
        crate::bundle::signing::audit_bundle(&audit_args.path).context("Bundle audit failed")?;

    println!(
        "Files checked: {} | Findings: {}",
        result.files_checked,
        result.findings.len()
    );

    for finding in &result.findings {
        let sev = match finding.severity {
            crate::bundle::BundleAuditSeverity::Info => "INFO    ",
            crate::bundle::BundleAuditSeverity::Warning => "WARNING ",
            crate::bundle::BundleAuditSeverity::Critical => "CRITICAL",
        };
        println!("[{sev}] {}: {}", finding.file, finding.description);
        println!("         Recommendation: {}", finding.recommendation);
    }

    if result.passed {
        println!("PASS Bundle audit passed.");
        return Ok(());
    }
    if audit_args.strict && !result.findings.is_empty() {
        anyhow::bail!("Bundle audit found issues (--strict mode).")
    }
    anyhow::bail!("Bundle audit found critical issues.")
}

fn verify_bundle_command(verify_args: BundleVerifyArgs) -> Result<()> {
    println!("Verifying bundle: {}", verify_args.path.display());
    let verifier = crate::bundle::BundleVerifier::new(
        dirs::home_dir()
            .unwrap_or_default()
            .join(".ahma")
            .join("keys")
            .join("trusted"),
    );
    match verifier.verify(&verify_args.path)? {
        true => {
            println!("PASS Bundle verification passed.");
            Ok(())
        }
        false => anyhow::bail!("FAIL Bundle verification failed."),
    }
}

fn sign_bundle_command(sign_args: BundleSignArgs) -> Result<()> {
    println!("Signing bundle: {}", sign_args.path.display());
    let digests =
        crate::bundle::BundleSigner::sign(&sign_args.path).context("Bundle signing failed")?;
    println!("Manifest written with {} file hashes.", digests.len());
    Ok(())
}

fn dispatch_bundle_command(args: BundleArgs) -> Result<()> {
    match args.command {
        BundleCommand::Audit(audit_args) => audit_bundle_command(audit_args),
        BundleCommand::Verify(verify_args) => verify_bundle_command(verify_args),
        BundleCommand::Sign(sign_args) => sign_bundle_command(sign_args),
    }
}

fn check_stdio_not_interactive() -> Result<()> {
    if !std::io::stdin().is_terminal() {
        return Ok(());
    }

    eprintln!(
        "\nFAIL Error: ahma_mcp is an MCP server designed for JSON-RPC communication over stdio.\n"
    );
    eprintln!("It cannot be run directly from an interactive terminal.\n");
    eprintln!("Usage options:");
    eprintln!("  1. Run as stdio MCP server (requires MCP client):");
    eprintln!("     ahma serve stdio\n");
    eprintln!("  2. Run as HTTP bridge server:");
    eprintln!("     ahma serve http --port 3000\n");
    eprintln!("  3. Execute a single tool command:");
    eprintln!("     ahma tool run <tool_name> [-- tool_arguments...]\n");
    eprintln!("For more information, run: ahma --help\n");
    std::process::exit(1);
}

// ─────────────────────────────────────────────────────────────────────────────
// CLI argument types (clap)
// ─────────────────────────────────────────────────────────────────────────────

/// Ahma MCP: A secure, config-driven adapter for CLI tools.
///
/// Environment variables control all non-essential options.
/// See docs/environment-variables.md for the full reference.
#[derive(Parser, Debug)]
#[command(
    name = "ahma",
    author,
    version,
    about = "Ahma MCP: secure, config-driven adapter for CLI tools"
)]
pub struct Cli {
    /// Emit the full CLI reference as Markdown and exit.
    ///
    /// Pipe into a file to regenerate `docs/cli-reference.md`:
    ///
    ///   ahma --markdown-help > docs/cli-reference.md
    #[arg(long, global = true, hide = true)]
    pub markdown_help: bool,

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
    /// Start the TUI hub daemon for multi-instance aggregation.
    ///
    /// The daemon is a lightweight process that collects operation events from
    /// all running ahma instances (including stdio processes spawned by IDEs)
    /// and fans them out to TUI subscribers.  It is started automatically on
    /// first use and exits automatically after 60 s of idle.
    Daemon(DaemonArgs),
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

/// Arguments for `ahma daemon`.
///
/// Currently no flags are needed; the daemon is configured entirely via
/// environment variables (`AHMA_DAEMON_SOCK`).
#[derive(clap::Args, Debug, Clone)]
pub struct DaemonArgs {}

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
    pub transport: ServeTransport,

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
    /// Override with AHMA_TOOLS_DIR env var.
    #[arg(long)]
    pub tools_dir: Option<PathBuf>,

    /// Add the system temp directory to the sandbox scope.
    /// Useful for workflows that need scratch space (compilers, build systems).
    /// Equivalent to AHMA_TMP_ACCESS=1.
    #[arg(long = "tmp", global = true)]
    pub tmp: bool,

    /// Enable live log monitoring. Ahma tails the configured log stream through
    /// an LLM to detect issues in real time and push alerts as MCP progress
    /// notifications. Equivalent to AHMA_LOG_MONITOR=1.
    #[arg(long = "log-monitor", global = true)]
    pub log_monitor: bool,

    /// Minimum seconds between successive log-monitor alerts.
    /// Prevents alert storms when a persistent issue triggers repeated matches.
    /// Equivalent to AHMA_MONITOR_RATE_LIMIT. Default: 60.
    #[arg(long = "monitor-rate-limit", value_name = "SECS", global = true)]
    pub monitor_rate_limit: Option<u64>,

    /// Disable the kernel sandbox entirely.
    /// UNSAFE: the AI can read and write anywhere on the filesystem.
    /// Use only in environments that provide their own containment (Docker, CI containers).
    /// Equivalent to AHMA_DISABLE_SANDBOX=1.
    #[arg(long = "no-sandbox", global = true)]
    pub no_sandbox: bool,

    /// Default tool execution timeout in seconds.
    /// Individual tools can override this via the timeout_seconds field in their JSON definition.
    /// Equivalent to AHMA_TIMEOUT. Default: 360.
    #[arg(long = "timeout", value_name = "SECS", global = true)]
    pub timeout: Option<u64>,

    /// Force all tools to run synchronously.
    /// By default, tools are async-first: if a result arrives within 5 seconds it is
    /// returned inline; otherwise an operation ID is returned and the result is pushed
    /// as a notification. Equivalent to AHMA_SYNC=1.
    #[arg(long = "sync", global = true)]
    pub sync: bool,

    /// OTLP endpoint for distributed tracing export.
    /// Providing this flag enables tracing. Equivalent to OTEL_EXPORTER_OTLP_ENDPOINT.
    #[arg(long = "opentelemetry", value_name = "URL", global = true)]
    pub opentelemetry: Option<String>,

    /// Run this server session inside an existing task vault.
    ///
    /// Sets the sandbox scope to <vault>/workdir/, initializes an audit log at
    /// <vault>/audit.jsonl, and wires two-phase delete through <vault>/trash/.
    /// The vault must exist (create with `ahma vault create <slug>` first).
    #[arg(long = "task-vault", value_name = "PATH", global = true)]
    pub task_vault: Option<PathBuf>,

    /// Immediately reveal all loaded `--tools` bundles at startup without
    /// requiring an `activate_tools` call. Equivalent to `AHMA_AUTO_REVEAL=1`
    /// or `AHMA_REVEAL_PROFILE=balanced`.
    ///
    /// Deprecated: prefer the `AHMA_REVEAL_PROFILE=balanced` environment
    /// variable; this flag is kept for backward compatibility.
    #[arg(long = "auto-reveal", global = true)]
    pub auto_reveal: bool,
}

#[derive(Subcommand, Debug)]
pub enum ServeTransport {
    /// Serve over stdio — the standard transport for MCP clients.
    ///
    /// The MCP client (Cursor, VS Code, Claude Desktop, …) spawns
    /// ahma as a child process and communicates over stdin/stdout.
    /// No network port is opened; sandboxing is applied per-session.
    ///
    /// To wire ahma into an MCP client add an entry like this to
    /// your `mcp.json` (exact key names vary by client):
    ///
    ///   "ahma": {
    ///     "command": "ahma",
    ///     "args": ["serve", "stdio", "--tool", "rust,git"]
    ///   }
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
    Stdio,
    /// Serve over HTTP — a persistent multi-session bridge.
    ///
    /// Listens on a TCP port and routes MCP sessions over HTTP/2 and
    /// (optionally) HTTP/3/QUIC.  Multiple MCP clients can connect
    /// concurrently.  Suitable for CI runners, shared machines, or
    /// remote integrations.
    Http(HttpArgs),
    /// Serve over a Unix domain socket (UDS) — for local IPC and Kubernetes sidecar proxies.
    ///
    /// Listens on a filesystem UDS path and routes MCP Streamable HTTP traffic
    /// through that socket.  Useful when TCP/DNS is unavailable (e.g. Envoy
    /// sidecars in K8s pods) or when you want socket-file-level access control
    /// without a network port.
    ///
    /// Linux abstract sockets are supported with the `@` prefix:
    ///   --socket-path @my-socket  →  binds `\0my-socket` in the kernel namespace
    ///
    /// Filesystem socket files are removed automatically on graceful shutdown.
    ///
    /// Not available on Windows; use `serve http` on that platform.
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

    /// Disable HTTP/3 (QUIC). Serve HTTP/2 over TCP only.
    #[arg(long = "disable-quic")]
    pub no_quic: bool,

    /// Require HTTP/2+; reject HTTP/1.1 connections.
    #[arg(long = "disable-http1-1")]
    pub disable_http1_1: bool,
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
    /// Execute a single tool command and print the result.
    ///
    /// Loads tool configurations, applies sandboxing, runs the named tool
    /// with the supplied arguments, and prints the output to stdout.
    /// Useful for scripting, CI pipelines, and debugging tool behaviour
    /// outside the MCP protocol.
    #[command(after_help = "EXAMPLES:
  # Run a cargo build in release mode
  ahma tool run cargo_build -- --release

  # Run git status
  ahma tool run git_status

  # Run with a custom tools directory
  AHMA_TOOLS_DIR=/path/to/.ahma ahma tool run my_tool -- --flag value")]
    Run(RunArgs),
    /// Show locally configured tools with descriptions and parameters.
    ///
    /// Loads tool definitions from the `.ahma/` directory (or `--tools-dir`)
    /// and built-in bundles (activated with `--tools`), then prints a summary
    /// of each tool including its subcommands, parameters, and hints.
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
    /// Create a new task vault for a user question.
    ///
    /// Creates ~/.ahma/tasks/<date>-<slug>-<id>/ with inputs/, workdir/,
    /// outputs/, trash/, and audit.jsonl.  Prints the vault root path to stdout.
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
    /// Generate the local TLS certificate (first-time setup).
    ///
    /// Creates a self-signed certificate under `~/.ahma/tls/` (or `AHMA_TLS_DIR`).
    /// The private key is written with mode 0600 on Unix.  Safe to re-run —
    /// does nothing if the certificate already exists.
    Init,
    /// Rotate the local TLS certificate (replace with a freshly generated one).
    ///
    /// Deletes the existing certificate and generates a new self-signed certificate
    /// under `~/.ahma/tls/`.  Use this when the certificate is approaching expiry
    /// or has been compromised.
    Rotate,
    /// Print the local TLS certificate status.
    ///
    /// Shows the certificate path, creation time, age, and whether rotation is
    /// recommended (certificate older than 30 days).
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
    /// Audit a bundle directory for security issues.
    ///
    /// Scans all JSON files for embedded secrets, missing path validation,
    /// prompt-injection payloads, and other supply-chain risks.
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
    /// Base URL of the OpenAI-compatible API (e.g. http://localhost:11434/v1).
    #[arg(long)]
    pub base_url: String,
    /// Default model to use with this provider (e.g. "llama3.2").
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
    /// Directory containing mTLS certificates for peer authentication.
    ///
    /// When set, all outbound peer connections use mTLS.  The directory must
    /// contain `ca.pem`, `cert.pem`, and `key.pem` generated by
    /// `ahma cluster cert init`.
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
    /// Generate a self-signed CA plus a leaf certificate and key for this peer.
    ///
    /// Writes `ca.pem`, `cert.pem`, and `key.pem` to `--out-dir`.
    /// Share `ca.pem` with all other peers so they can verify each other.
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

// ─────────────────────────────────────────────────────────────────────────────
// AppConfig construction from CLI + env vars
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(unix)]
fn unix_socket_path_from_cli(cli: &Cli) -> String {
    match &cli.command {
        Subcommands::Serve(s) => match &s.transport {
            ServeTransport::Unix(u) => u
                .socket_path
                .clone()
                .or_else(|| std::env::var("AHMA_UNIX_SOCKET").ok())
                .unwrap_or_else(|| "/tmp/ahma.sock".to_string()),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

#[cfg(not(unix))]
fn unix_socket_path_from_cli(_cli: &Cli) -> String {
    String::new()
}

struct ServeFields {
    tool_bundles: Vec<String>,
    tools_dir: Option<PathBuf>,
    http_host: String,
    http_port: u16,
    no_quic: bool,
    disable_http1_1: bool,
    tmp: bool,
    log_monitor: bool,
    monitor_rate_limit: Option<u64>,
    no_sandbox: bool,
    timeout: Option<u64>,
    sync: bool,
    opentelemetry: Option<String>,
    task_vault: Option<PathBuf>,
    auto_reveal: bool,
}

fn extract_serve_fields(cmd: &Subcommands) -> ServeFields {
    if let Subcommands::Serve(s) = cmd {
        let (host, port, no_quic, disable_http1_1) = match &s.transport {
            ServeTransport::Http(h) => (h.host.clone(), h.port, h.no_quic, h.disable_http1_1),
            ServeTransport::Stdio => ("127.0.0.1".to_string(), 3000u16, false, false),
            #[cfg(unix)]
            ServeTransport::Unix(_) => ("127.0.0.1".to_string(), 3000u16, true, false),
        };
        ServeFields {
            tool_bundles: s.tool_bundles.clone(),
            tools_dir: s.tools_dir.clone(),
            http_host: host,
            http_port: port,
            no_quic,
            disable_http1_1,
            tmp: s.tmp,
            log_monitor: s.log_monitor,
            monitor_rate_limit: s.monitor_rate_limit,
            no_sandbox: s.no_sandbox,
            timeout: s.timeout,
            sync: s.sync,
            opentelemetry: s.opentelemetry.clone(),
            task_vault: s.task_vault.clone(),
            auto_reveal: s.auto_reveal,
        }
    } else {
        ServeFields {
            tool_bundles: vec![],
            tools_dir: None,
            http_host: "127.0.0.1".to_string(),
            http_port: 3000u16,
            no_quic: false,
            disable_http1_1: false,
            tmp: false,
            log_monitor: false,
            monitor_rate_limit: None,
            no_sandbox: false,
            timeout: None,
            sync: false,
            opentelemetry: None,
            task_vault: None,
            auto_reveal: false,
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

fn extract_tool_fields(cmd: &Subcommands) -> ToolFields {
    if let Subcommands::Tool(ToolArgs { command }) = cmd {
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
            _ => ToolFields {
                list_server: None,
                mcp_config: PathBuf::from("mcp.json"),
                list_http: None,
                list_format: list_tools::OutputFormat::Text,
                run_tool: None,
                run_tool_args: vec![],
            },
        }
    } else {
        ToolFields {
            list_server: None,
            mcp_config: PathBuf::from("mcp.json"),
            list_http: None,
            list_format: list_tools::OutputFormat::Text,
            run_tool: None,
            run_tool_args: vec![],
        }
    }
}

pub fn build_app_config(cli: &Cli) -> AppConfig {
    let serve = extract_serve_fields(&cli.command);
    let tool = extract_tool_fields(&cli.command);

    // Env-var overrides for tools_dir
    let env_tools_dir = std::env::var("AHMA_TOOLS_DIR").ok().map(PathBuf::from);
    let explicit_tools_dir = serve.tools_dir.is_some();
    let raw_tools_dir = serve.tools_dir.or(env_tools_dir);
    let tools_dir = resolution::normalize_tools_dir(raw_tools_dir);

    // Flatten and deduplicate tool bundles (support comma-separation already handled by clap delimiter)
    let tool_bundles = {
        let mut seen = std::collections::HashSet::new();
        serve
            .tool_bundles
            .into_iter()
            .filter(|b| seen.insert(b.clone()))
            .collect()
    };

    // Sandbox scopes: --sandbox-scope CLI flag is gone; use AHMA_SANDBOX_SCOPE env var
    let sandbox_scopes = AppConfig::env_sandbox_scopes();
    let working_dirs = AppConfig::env_working_dirs();

    // HTTP quic override from env
    let no_quic = serve.no_quic || AppConfig::env_flag("AHMA_DISABLE_QUIC");
    let disable_http1_1 = serve.disable_http1_1 || AppConfig::env_flag("AHMA_DISABLE_HTTP1_1");

    AppConfig {
        tools_dir,
        explicit_tools_dir,
        tool_bundles,
        timeout_secs: serve
            .timeout
            .unwrap_or_else(|| AppConfig::env_u64("AHMA_TIMEOUT", 360)),
        force_sync: serve.sync || AppConfig::env_flag("AHMA_SYNC"),
        hot_reload_tools: AppConfig::env_flag("AHMA_HOT_RELOAD"),
        skip_availability_probes: AppConfig::env_flag("AHMA_SKIP_PROBES"),
        progressive_disclosure: AppConfig::env_flag("AHMA_PROGRESSIVE_DISCLOSURE"),
        reveal_profile: match std::env::var("AHMA_REVEAL_PROFILE")
            .as_deref()
            .unwrap_or("")
        {
            "balanced" => StartupProfile::Balanced,
            "full" => StartupProfile::Full,
            _ if serve.auto_reveal || AppConfig::env_flag("AHMA_AUTO_REVEAL") => {
                StartupProfile::Balanced
            }
            _ => StartupProfile::Minimal,
        },
        no_sandbox: serve.no_sandbox || AppConfig::env_flag("AHMA_DISABLE_SANDBOX"),
        sandbox_scopes,
        defer_sandbox: AppConfig::env_flag("AHMA_SANDBOX_DEFER"),
        working_dirs,
        tmp_access: serve.tmp || AppConfig::env_flag("AHMA_TMP_ACCESS"),
        no_temp_files: AppConfig::env_flag("AHMA_DISABLE_TEMP"),
        log_monitor: serve.log_monitor || AppConfig::env_flag("AHMA_LOG_MONITOR"),
        monitor_rate_limit_secs: serve
            .monitor_rate_limit
            .unwrap_or_else(|| AppConfig::env_u64("AHMA_MONITOR_RATE_LIMIT", 60)),
        http_host: serve.http_host,
        http_port: serve.http_port,
        no_quic,
        disable_http1_1,
        handshake_timeout_secs: AppConfig::env_u64("AHMA_HANDSHAKE_TIMEOUT", 45),
        unix_socket_path: unix_socket_path_from_cli(cli),
        observability: ahma_common::observability::ObservabilityConfig::from_env("ahma_mcp")
            .with_endpoint(serve.opentelemetry.as_deref()),
        list_server: tool.list_server,
        mcp_config: tool.mcp_config,
        list_http: tool.list_http,
        list_format: tool.list_format,
        run_tool: tool.run_tool,
        run_tool_args: tool.run_tool_args,
        task_vault: serve
            .task_vault
            .or_else(|| std::env::var("AHMA_TASK_VAULT").ok().map(PathBuf::from)),
        require_token: std::env::var("AHMA_REQUIRE_TOKEN")
            .ok()
            .filter(|s| !s.is_empty()),
        require_token_path: std::env::var("AHMA_REQUIRE_TOKEN_PATH")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from),
        rate_limit_rps: std::env::var("AHMA_RATE_LIMIT_RPS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        rate_limit_burst: std::env::var("AHMA_RATE_LIMIT_BURST")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10),
        instance_label: std::env::var("AHMA_INSTANCE_LABEL").unwrap_or_else(|_| "ahma".to_string()),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

pub async fn run() -> Result<()> {
    // Determine log target before parsing full CLI so logging works for all subcommands.
    // RUST_LOG controls verbosity; AHMA_LOG_TARGET=stderr routes to stderr (default: file).
    let log_to_stderr = std::env::var("AHMA_LOG_TARGET")
        .map(|v| v.trim().eq_ignore_ascii_case("stderr"))
        .unwrap_or(false);

    let cli = Cli::parse();
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
    let result = crate::validation::run_validation(target)?;
    if result.all_valid {
        println!("All configurations are valid.");
        Ok(())
    } else {
        anyhow::bail!(
            "Validation failed: {}/{} files invalid.",
            result.files_failed,
            result.files_checked
        )
    }
}

async fn run_tool_info_mode(args: InfoArgs) -> Result<()> {
    use crate::config;

    // Build a minimal AppConfig with the requested bundles + tools_dir
    let env_tools_dir = std::env::var("AHMA_TOOLS_DIR").ok().map(PathBuf::from);
    let raw_tools_dir = args.tools_dir.or(env_tools_dir);
    let tools_dir = resolution::normalize_tools_dir(raw_tools_dir);

    let mini_cfg = AppConfig {
        tool_bundles: args.tool_bundles,
        tools_dir: tools_dir.clone(),
        ..AppConfig::default()
    };

    let configs = config::load_tool_configs(&mini_cfg, tools_dir.as_deref()).await?;

    // Optionally filter to a single tool
    let mut tools: Vec<(&String, &config::ToolConfig)> = configs.iter().collect();
    if let Some(ref filter) = args.filter {
        tools.retain(|(name, _)| name.as_str() == filter.as_str());
        if tools.is_empty() {
            anyhow::bail!(
                "Tool '{}' not found. Run without a filter to see all available tools.",
                filter
            );
        }
    }
    tools.sort_by_key(|(name, _)| (*name).clone());

    match args.format {
        list_tools::OutputFormat::Text => print_tool_info_text(&tools),
        list_tools::OutputFormat::Json => print_tool_info_json(&tools)?,
    }

    Ok(())
}

fn print_command_arg_flag(opt: &crate::config::CommandOption) {
    let req = if opt.required.unwrap_or(false) {
        "required"
    } else {
        "optional"
    };
    print!("        --{} ({}, {})", opt.name, opt.option_type, req);
    if let Some(ref desc) = opt.description {
        print!(": {}", desc);
    }
    println!();
}

fn print_command_arg_positional(arg: &crate::config::CommandOption) {
    let req = if arg.required.unwrap_or(false) {
        "required"
    } else {
        "optional"
    };
    print!("        <{}> ({}, {})", arg.name, arg.option_type, req);
    if let Some(ref desc) = arg.description {
        print!(": {}", desc);
    }
    println!();
}

fn print_optional_command_args(
    args: Option<&[crate::config::CommandOption]>,
    printer: fn(&crate::config::CommandOption),
) {
    if let Some(args) = args {
        for arg in args {
            printer(arg);
        }
    }
}

fn print_subcommand(sub: &crate::config::SubcommandConfig) {
    let status = if sub.enabled { "" } else { " (disabled)" };
    println!("    - {}{}: {}", sub.name, status, sub.description);
    print_optional_command_args(sub.options.as_deref(), print_command_arg_flag);
    print_optional_command_args(sub.positional_args.as_deref(), print_command_arg_positional);
}

fn print_subcommands(subs: &[crate::config::SubcommandConfig]) {
    println!("  Subcommands:");
    for sub in subs {
        print_subcommand(sub);
    }
}

fn print_hint_line(label: &str, value: &str) {
    println!("    {}: {}", label, value);
}

fn print_hints(h: &crate::config::ToolHints) {
    let standard_hints = [
        ("build", h.build.as_deref()),
        ("test", h.test.as_deref()),
        ("dependencies", h.dependencies.as_deref()),
        ("clean", h.clean.as_deref()),
        ("run", h.run.as_deref()),
    ];
    let custom_hints = h.custom.as_ref().filter(|custom| !custom.is_empty());

    if !standard_hints.iter().any(|(_, value)| value.is_some()) && custom_hints.is_none() {
        return;
    }

    println!("  Hints:");

    for (label, value) in standard_hints {
        if let Some(value) = value {
            print_hint_line(label, value);
        }
    }

    if let Some(custom) = custom_hints {
        for (k, v) in custom {
            print_hint_line(k, v);
        }
    }
}

fn print_availability_check(ac: &crate::config::AvailabilityCheck) {
    print!("  Availability check:");
    if let Some(ref cmd) = ac.command {
        print!(" {}", cmd);
    }
    if !ac.args.is_empty() {
        print!(" {}", ac.args.join(" "));
    }
    println!();
}

fn print_tool_info_header(total_tools: usize) {
    println!("Local Tool Configurations");
    println!("=========================");
    println!();
    println!("Total tools: {}", total_tools);
    println!();
}

#[allow(deprecated)]
fn print_tool_info_entry(name: &str, config: &crate::config::ToolConfig) {
    println!("Tool: {}", name);
    println!("  Description: {}", config.description);
    println!("  Command:     {}", config.command);
    println!("  Enabled:     {}", config.enabled);
    if let Some(timeout) = config.timeout_seconds {
        println!("  Timeout:     {}s", timeout);
    }
    if let Some(sync) = config.synchronous {
        println!("  Synchronous: {}", sync);
    }
    if let Some(ref subs) = config.subcommand {
        print_subcommands(subs);
    }
    print_hints(&config.hints);
    if let Some(ref ac) = config.availability_check {
        print_availability_check(ac);
    }
    if let Some(ref inst) = config.install_instructions {
        println!("  Install: {}", inst);
    }
    println!();
}

fn print_tool_info_text(tools: &[(&String, &crate::config::ToolConfig)]) {
    print_tool_info_header(tools.len());

    for (name, config) in tools {
        print_tool_info_entry(name, config);
    }
}

#[allow(deprecated)]
fn print_tool_info_json(tools: &[(&String, &crate::config::ToolConfig)]) -> Result<()> {
    let output: Vec<_> = tools
        .iter()
        .map(|(name, config)| {
            serde_json::json!({
                "name": name,
                "description": config.description,
                "command": config.command,
                "enabled": config.enabled,
                "timeout_seconds": config.timeout_seconds,
                "synchronous": config.synchronous,
                "subcommands": config.subcommand.as_ref().map(|subs| {
                    subs.iter().map(|s| {
                        serde_json::json!({
                            "name": s.name,
                            "description": s.description,
                            "enabled": s.enabled,
                            "options": s.options.as_ref().map(|opts| {
                                opts.iter().map(|o| serde_json::json!({
                                    "name": o.name,
                                    "type": o.option_type,
                                    "required": o.required.unwrap_or(false),
                                    "description": o.description,
                                })).collect::<Vec<_>>()
                            }),
                            "positional_args": s.positional_args.as_ref().map(|args| {
                                args.iter().map(|a| serde_json::json!({
                                    "name": a.name,
                                    "type": a.option_type,
                                    "required": a.required.unwrap_or(false),
                                    "description": a.description,
                                })).collect::<Vec<_>>()
                            }),
                        })
                    }).collect::<Vec<_>>()
                }),
                "install_instructions": config.install_instructions,
            })
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
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
    use std::io::Write;
    use tempfile::tempdir;

    fn init_test() {
        crate::utils::logging::init_test_logging();
    }

    // ─── env_flag_enabled ───────────────────────────────────────────────────

    #[test]
    fn test_env_flag_enabled_unset() {
        unsafe { std::env::remove_var("AHMA_TEST_FLAG_UNSET") };
        assert!(!env_flag_enabled("AHMA_TEST_FLAG_UNSET"));
    }

    #[test]
    fn test_env_flag_enabled_empty() {
        unsafe { std::env::set_var("AHMA_TEST_FLAG_EMPTY", "") };
        let result = env_flag_enabled("AHMA_TEST_FLAG_EMPTY");
        unsafe { std::env::remove_var("AHMA_TEST_FLAG_EMPTY") };
        assert!(!result);
    }

    #[test]
    fn test_env_flag_enabled_whitespace_only() {
        unsafe { std::env::set_var("AHMA_TEST_FLAG_WS", "   ") };
        let result = env_flag_enabled("AHMA_TEST_FLAG_WS");
        unsafe { std::env::remove_var("AHMA_TEST_FLAG_WS") };
        assert!(!result);
    }

    #[test]
    fn test_env_flag_enabled_true() {
        for val in ["1", "true", "True", "TRUE", "yes", "Yes", "on", "ON"] {
            unsafe { std::env::set_var("AHMA_TEST_FLAG_VAL", val) };
            let result = env_flag_enabled("AHMA_TEST_FLAG_VAL");
            unsafe { std::env::remove_var("AHMA_TEST_FLAG_VAL") };
            assert!(result, "env_flag_enabled({:?}) should be true", val);
        }
    }

    #[test]
    fn test_env_flag_enabled_false() {
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

    #[test]
    fn test_env_sandbox_scopes_single_path() {
        let temp = tempdir().expect("Failed to create temp dir");
        unsafe { std::env::set_var("AHMA_SANDBOX_SCOPE", temp.path()) };
        let scopes = AppConfig::env_sandbox_scopes();
        unsafe { std::env::remove_var("AHMA_SANDBOX_SCOPE") };

        assert_eq!(scopes, vec![temp.path().to_path_buf()]);
    }

    #[test]
    fn test_env_sandbox_scopes_tilde() {
        unsafe { std::env::set_var("AHMA_SANDBOX_SCOPE", "~") };
        let scopes = AppConfig::env_sandbox_scopes();
        unsafe { std::env::remove_var("AHMA_SANDBOX_SCOPE") };

        if let Some(home) = dirs::home_dir() {
            assert_eq!(scopes, vec![home]);
        }
    }

    #[test]
    fn test_env_sandbox_scopes_tilde_slash() {
        unsafe { std::env::set_var("AHMA_SANDBOX_SCOPE", "~/test_sandbox") };
        let scopes = AppConfig::env_sandbox_scopes();
        unsafe { std::env::remove_var("AHMA_SANDBOX_SCOPE") };

        if let Some(home) = dirs::home_dir() {
            assert_eq!(scopes, vec![home.join("test_sandbox")]);
        }
    }

    #[test]
    fn test_env_working_dirs_multiple_paths() {
        let temp_a = tempdir().expect("Failed to create first temp dir");
        let temp_b = tempdir().expect("Failed to create second temp dir");
        let joined =
            std::env::join_paths([temp_a.path(), temp_b.path()]).expect("Failed to join path list");

        unsafe { std::env::set_var("AHMA_WORKING_DIRS", joined) };
        let dirs = AppConfig::env_working_dirs();
        unsafe { std::env::remove_var("AHMA_WORKING_DIRS") };

        assert_eq!(
            dirs,
            vec![temp_a.path().to_path_buf(), temp_b.path().to_path_buf()]
        );
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
            progressive_disclosure: false,
            reveal_profile: StartupProfile::Minimal,
            no_sandbox: false,
            sandbox_scopes: vec![],
            defer_sandbox: false,
            working_dirs: vec![],
            tmp_access: false,
            no_temp_files: false,
            log_monitor: false,
            monitor_rate_limit_secs: 60,
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
    fn test_resolve_sandbox_policy_ahma_tmp_access_env() {
        init_test();
        unsafe { std::env::set_var("AHMA_TMP_ACCESS", "1") };
        let cfg = AppConfig {
            tmp_access: AppConfig::env_flag("AHMA_TMP_ACCESS"),
            ..make_cfg()
        };
        unsafe { std::env::remove_var("AHMA_TMP_ACCESS") };
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
        let cfg = AppConfig {
            no_sandbox: true,
            sandbox_scopes: vec![PathBuf::from("/nonexistent/path/that/does/not/exist")],
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
        assert_eq!(scopes.len(), 1);

        let expected_workdir = dunce::canonicalize(vault_root.join("workdir")).unwrap();
        assert_eq!(scopes[0], expected_workdir);
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

    #[test]
    fn test_resolve_sandbox_scopes_cwd_fallback() {
        init_test();
        let cfg = AppConfig {
            no_sandbox: true,
            sandbox_scopes: vec![],
            ..make_cfg()
        };
        let scopes = resolve_sandbox_scopes(&cfg).unwrap();
        assert!(scopes.is_some());
        assert_eq!(scopes.unwrap().len(), 1);
    }

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
                transport: ServeTransport::Stdio,
                ..
            })
        ));
    }

    #[test]
    fn test_cli_parse_serve_http_defaults() {
        let cli = Cli::try_parse_from(["ahma", "serve", "http"]).unwrap();
        if let Subcommands::Serve(ServeArgs {
            transport: ServeTransport::Http(h),
            ..
        }) = cli.command
        {
            assert_eq!(h.host, "127.0.0.1");
            assert_eq!(h.port, 3000);
            assert!(!h.no_quic);
        } else {
            panic!("expected serve http");
        }
    }

    #[test]
    fn test_cli_parse_serve_http_custom_port() {
        let cli = Cli::try_parse_from(["ahma", "serve", "http", "--port", "8080"]).unwrap();
        if let Subcommands::Serve(ServeArgs {
            transport: ServeTransport::Http(h),
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
        if let Subcommands::Serve(s) = cli.command {
            assert!(s.tool_bundles.contains(&"rust".to_string()));
            assert!(s.tool_bundles.contains(&"python".to_string()));
        } else {
            panic!("expected serve stdio");
        }
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
        unsafe { std::env::set_var("AHMA_TEST_CFG_FLAG", "yes") };
        assert!(AppConfig::env_flag("AHMA_TEST_CFG_FLAG"));
        unsafe { std::env::remove_var("AHMA_TEST_CFG_FLAG") };
    }

    // ─── --auto-reveal / AHMA_AUTO_REVEAL compatibility ──────────────────────

    #[test]
    fn test_cli_parse_auto_reveal_flag() {
        // Regression: `--auto-reveal` must be accepted (not rejected) by clap.
        let cli = Cli::try_parse_from(["ahma", "serve", "stdio", "--auto-reveal"]).unwrap();
        if let Subcommands::Serve(s) = cli.command {
            assert!(
                s.auto_reveal,
                "auto_reveal should be true when --auto-reveal is passed"
            );
        } else {
            panic!("expected serve stdio subcommand");
        }
    }

    #[test]
    fn test_build_app_config_auto_reveal_flag_maps_to_balanced() {
        let cli = Cli::try_parse_from(["ahma", "serve", "stdio", "--auto-reveal"]).unwrap();
        let cfg = build_app_config(&cli);
        assert_eq!(
            cfg.reveal_profile,
            StartupProfile::Balanced,
            "--auto-reveal should set reveal_profile to Balanced"
        );
    }

    #[test]
    fn test_build_app_config_ahma_auto_reveal_env_maps_to_balanced() {
        let cli = Cli::try_parse_from(["ahma", "serve", "stdio"]).unwrap();
        unsafe { std::env::set_var("AHMA_AUTO_REVEAL", "1") };
        let cfg = build_app_config(&cli);
        unsafe { std::env::remove_var("AHMA_AUTO_REVEAL") };
        assert_eq!(
            cfg.reveal_profile,
            StartupProfile::Balanced,
            "AHMA_AUTO_REVEAL=1 should set reveal_profile to Balanced"
        );
    }

    #[test]
    fn test_build_app_config_reveal_profile_env_takes_precedence_over_auto_reveal() {
        // AHMA_REVEAL_PROFILE wins; --auto-reveal should not override it.
        let cli = Cli::try_parse_from(["ahma", "serve", "stdio", "--auto-reveal"]).unwrap();
        unsafe { std::env::set_var("AHMA_REVEAL_PROFILE", "full") };
        let cfg = build_app_config(&cli);
        unsafe { std::env::remove_var("AHMA_REVEAL_PROFILE") };
        assert_eq!(
            cfg.reveal_profile,
            StartupProfile::Full,
            "AHMA_REVEAL_PROFILE=full should override --auto-reveal"
        );
    }

    #[test]
    fn test_build_app_config_default_reveal_profile_is_minimal() {
        let cli = Cli::try_parse_from(["ahma", "serve", "stdio"]).unwrap();
        // Ensure no relevant env vars are set
        unsafe { std::env::remove_var("AHMA_REVEAL_PROFILE") };
        unsafe { std::env::remove_var("AHMA_AUTO_REVEAL") };
        let cfg = build_app_config(&cli);
        assert_eq!(
            cfg.reveal_profile,
            StartupProfile::Minimal,
            "default reveal_profile should be Minimal"
        );
    }
}
