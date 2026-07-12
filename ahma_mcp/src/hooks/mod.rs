use crate::shell::cli::AppConfig;
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;

mod consent;
pub use consent::HookConsentStore;

pub mod post_exec;

const MANAGED_ID_DEFAULT_SHELL_V1: &str = "ahma-default-shell-v1";
const WRAPPED_BY_MARKER: &str = "ahma-hooks-wrapper-v1";
const HOOK_TIMEOUT_SECS: u64 = 30;
const PATH_LOOKUP_BINARY: &str = "ahma";
/// Cursor hook event for observing finished shell commands (post-execution).
/// Used to surface a host-sandbox denial that the deferring pre-execution hook
/// cannot see (SPEC R7 — defer to host, but still disclose failures).
const CURSOR_OBSERVE_EVENT_KEY: &str = "afterShellExecution";

/// The decision `compute_exec_decision` makes for each tool invocation.
///
/// When ahma can sandbox, the command is passed through or rewritten to route
/// through the kernel sandbox. When ahma **cannot** sandbox, the behaviour is
/// governed by one-time session consent (SPEC R5.5.3): the first such command
/// fails closed ([`DenyPendingConsent`](HooksDecision::DenyPendingConsent)); only
/// after explicit consent does it run unsandboxed with a loud, persistent
/// warning. ahma never silently runs a command unsandboxed.
#[derive(Debug)]
enum HooksDecision {
    /// Allow the command through without modification (passthrough to default terminal).
    AllowUnchanged,
    /// Allow with a rewritten command that routes through ahma's kernel sandbox.
    AllowRewrite(Value),
    /// **Fail closed (pending consent)**: ahma cannot sandbox this command and
    /// the user has not consented to unsandboxed execution this session. The
    /// command is DENIED with an actionable message (R5.5.3).
    DenyPendingConsent {
        user_message: String,
        agent_message: String,
    },
    /// ahma cannot sandbox, but the user has consented to unsandboxed execution
    /// this session. Allow the original command UNSANDBOXED with a loud,
    /// persistent warning (R5.5.3).
    AllowWithWarning {
        user_message: String,
        agent_message: String,
    },
    /// A host sandbox (Cursor, VS Code, Docker, …) was detected, so ahma defers to
    /// it: the original command is allowed UNCHANGED to run inside the host's
    /// kernel sandbox, and ahma does NOT re-wrap it (avoids the redundant
    /// double-sandbox and the host's build-cache env friction). A loud disclosure
    /// states which sandbox is protecting the command. This is distinct from
    /// `AllowWithWarning`: it is NOT an unsandboxed bypass and is not counted as
    /// one — protection is provided by the host (R7).
    DeferToHost {
        user_message: String,
        agent_message: String,
    },
}

/// Manage terminal hooks for external AI tools.
#[derive(Args, Debug)]
#[command(
    // Keep this list in sync with `HookPlatform::all()` — it is the source of truth.
    about = "Manage terminal hooks for Cursor, Claude Code, Codex, GitHub Copilot CLI, and Antigravity",
    after_help = "SUPPORTED CLIENTS: cursor, claude, codex, copilot, antigravity

WHAT THIS DOES:
  Terminal hooks transparently route the shell commands an agent runs through its
  NATIVE terminal/Bash tool into ahma's kernel sandbox. (The ahma MCP server only
  sandboxes the tools the agent calls explicitly; hooks cover the rest.)

ACTIVE vs INSTALLED:
  `install` only writes the hook file. In the default `auto` mode a hook is only
  ACTIVE when an ahma MCP server is detected; otherwise it passes commands through
  UNSANDBOXED. Run `ahma hooks status` to see the effective state. Force with
  AHMA_HOOKS=on|off (alias AHMA_DISABLE_HOOKS=1) or the `--hooks on|off|auto` flag.

EXAMPLES:
  # Install user-scoped hooks for all supported tools (including Cursor)
  ahma hooks install

  # Install project-scoped hooks for Claude Code and Codex
  ahma hooks install --platform claude,codex --scope project

  # Show effective state plus both user and project hook status
  ahma hooks status

  # Remove only the Cursor project hook
  ahma hooks uninstall --platform cursor --scope project"
)]
pub struct HooksArgs {
    #[command(subcommand)]
    pub command: HooksCommand,
}

#[derive(Subcommand, Debug)]
pub enum HooksCommand {
    /// Install or update managed shell hooks.
    Install(HooksInstallArgs),
    /// Remove managed shell hooks.
    Uninstall(HooksUninstallArgs),
    /// Show where managed shell hooks are installed.
    Status(HooksStatusArgs),
    /// Internal hook entrypoint used by external tools.
    #[command(hide = true)]
    Exec(HooksExecArgs),
    /// Internal post-execution observer used by managed hooks to surface a
    /// host-sandbox denial that the pre-execution (defer-to-host) hook cannot see.
    #[command(hide = true)]
    Observe(HooksObserveArgs),
    /// Internal shell wrapper used by managed hooks.
    #[command(name = "run-shell", hide = true)]
    RunShell(HooksRunShellArgs),
    /// Allow hook commands to run UNSANDBOXED for this session when ahma cannot
    /// sandbox them (SPEC R5.5.3). Consent is session-scoped and never persists
    /// across a reboot; revoke it with `ahma hooks revoke`.
    #[command(name = "approve-unsandboxed")]
    ApproveUnsandboxed,
    /// Revoke this session's unsandboxed-execution consent. Hooks that cannot be
    /// sandboxed will fail closed again.
    Revoke,
    /// Diagnose why ahma cannot sandbox (binary path/version, kernel backend) and
    /// print repair guidance, plus the current consent state.
    Doctor,
}

#[derive(Args, Debug)]
pub struct HooksInstallArgs {
    /// Platform(s) to configure. Defaults to all supported platforms.
    #[arg(long = "platform", value_enum, value_delimiter = ',')]
    pub platforms: Vec<HookPlatform>,

    /// Where to write the hook configuration.
    #[arg(long, value_enum, default_value_t = HookScope::User)]
    pub scope: HookScope,

    /// Show planned changes without writing files.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct HooksUninstallArgs {
    /// Platform(s) to clean up. Defaults to all supported platforms.
    #[arg(long = "platform", value_enum, value_delimiter = ',')]
    pub platforms: Vec<HookPlatform>,

    /// Which hook scope to clean up.
    #[arg(long, value_enum, default_value_t = HookScope::User)]
    pub scope: HookScope,

    /// Show planned changes without writing files.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct HooksStatusArgs {
    /// Platform(s) to inspect. Defaults to all supported platforms.
    #[arg(long = "platform", value_enum, value_delimiter = ',')]
    pub platforms: Vec<HookPlatform>,

    /// Restrict status to one scope. By default both scopes are shown.
    #[arg(long, value_enum)]
    pub scope: Option<HookScope>,
}

#[derive(Args, Debug)]
pub struct HooksExecArgs {
    #[arg(long, value_enum)]
    pub platform: HookPlatform,

    #[arg(long, value_enum)]
    pub scope: HookScope,

    #[arg(long, hide = true, default_value = MANAGED_ID_DEFAULT_SHELL_V1)]
    pub managed_id: String,
}

#[derive(Args, Debug)]
pub struct HooksObserveArgs {
    #[arg(long, value_enum)]
    pub platform: HookPlatform,

    #[arg(long, value_enum)]
    pub scope: HookScope,

    #[arg(long, hide = true, default_value = MANAGED_ID_DEFAULT_SHELL_V1)]
    pub managed_id: String,
}

#[derive(Args, Debug)]
pub struct HooksRunShellArgs {
    #[arg(long = "payload-base64")]
    pub payload_base64: String,

    #[arg(long, hide = true, default_value = WRAPPED_BY_MARKER)]
    pub wrapped_by: String,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum HookPlatform {
    Cursor,
    Claude,
    Codex,
    Copilot,
    Antigravity,
}

impl HookPlatform {
    fn all() -> Vec<Self> {
        vec![
            Self::Cursor,
            Self::Claude,
            Self::Codex,
            Self::Copilot,
            Self::Antigravity,
        ]
    }

    fn label(self) -> &'static str {
        match self {
            Self::Cursor => "Cursor",
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Copilot => "GitHub Copilot CLI",
            Self::Antigravity => "Antigravity",
        }
    }

    fn config_path(self, scope_root: &Path, scope: HookScope) -> PathBuf {
        scope_root.join(self.config_relative_path(scope))
    }

    fn config_relative_path(self, scope: HookScope) -> PathBuf {
        match self {
            Self::Cursor => PathBuf::from(".cursor/hooks.json"),
            Self::Claude => PathBuf::from(".claude/settings.json"),
            Self::Codex => PathBuf::from(".codex/hooks.json"),
            Self::Copilot => match scope {
                HookScope::User => PathBuf::from(".copilot/hooks/ahma.json"),
                HookScope::Project => PathBuf::from(".github/hooks/ahma.json"),
            },
            Self::Antigravity => match scope {
                HookScope::User => PathBuf::from(".gemini/config/hooks.json"),
                HookScope::Project => PathBuf::from(".agents/hooks.json"),
            },
        }
    }

    fn event_key(self) -> &'static str {
        match self {
            Self::Cursor => "preToolUse",
            Self::Claude | Self::Codex | Self::Antigravity => "PreToolUse",
            Self::Copilot => "preToolUse",
        }
    }

    fn cli_name(self) -> &'static str {
        match self {
            Self::Cursor => "cursor",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Copilot => "copilot",
            Self::Antigravity => "antigravity",
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum HookScope {
    User,
    Project,
}

impl HookScope {
    fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }

    fn cli_name(self) -> &'static str {
        self.label()
    }
}

#[derive(Debug, Clone)]
struct HookEnvironment {
    home_dir: PathBuf,
    project_root: PathBuf,
    current_exe: PathBuf,
}

impl HookEnvironment {
    fn detect() -> Result<Self> {
        let home_dir = dirs::home_dir().context("Failed to determine home directory")?;
        let project_root = detect_project_root()?;
        let current_exe =
            std::env::current_exe().context("Failed to resolve current ahma binary")?;
        let current_exe = dunce::canonicalize(&current_exe).unwrap_or(current_exe);

        Ok(Self {
            home_dir,
            project_root,
            current_exe,
        })
    }

    fn scope_root(&self, scope: HookScope) -> &Path {
        match scope {
            HookScope::User => &self.home_dir,
            HookScope::Project => &self.project_root,
        }
    }

    fn config_path(&self, platform: HookPlatform, scope: HookScope) -> PathBuf {
        platform.config_path(self.scope_root(scope), scope)
    }
}

#[derive(Debug, Clone)]
enum BinaryReference {
    Absolute(PathBuf),
    PathLookup,
}

impl BinaryReference {
    fn for_scope(env: &HookEnvironment, scope: HookScope) -> Self {
        match scope {
            HookScope::User => Self::Absolute(env.current_exe.clone()),
            HookScope::Project => Self::PathLookup,
        }
    }

    fn build_command(&self, args: &[String]) -> String {
        match self {
            Self::PathLookup => std::iter::once(PATH_LOOKUP_BINARY.to_string())
                .chain(args.iter().cloned())
                .collect::<Vec<_>>()
                .join(" "),
            Self::Absolute(path) => build_absolute_shell_command(path, args),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WrappedShellPayload {
    cwd: String,
    command: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileAction {
    Created,
    Updated,
    Unchanged,
}

pub fn any_managed_hooks_installed(scope: HookScope) -> Result<bool> {
    let env = HookEnvironment::detect()?;
    any_managed_hooks_installed_in_env(scope, &env)
}

fn any_managed_hooks_installed_in_env(scope: HookScope, env: &HookEnvironment) -> Result<bool> {
    for platform in HookPlatform::all() {
        let path = env.config_path(platform, scope);
        if path.exists() && platform_hook_installed(&load_hook_document(&path)?, platform) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub async fn run(args: HooksArgs, cfg: AppConfig) -> Result<()> {
    match args.command {
        HooksCommand::Install(args) => run_install(args),
        HooksCommand::Uninstall(args) => run_uninstall(args),
        HooksCommand::Status(args) => run_status(args),
        HooksCommand::Exec(args) => run_exec(args),
        HooksCommand::Observe(args) => run_observe(args),
        HooksCommand::RunShell(args) => run_shell(args, cfg).await,
        HooksCommand::ApproveUnsandboxed => run_approve_unsandboxed(),
        HooksCommand::Revoke => run_revoke_consent(),
        HooksCommand::Doctor => run_doctor(),
    }
}

/// Last-resort guard for the hook entrypoint when ahma's own CLI args fail to
/// parse (for example a managed hook command written by one ahma version invoked
/// against a different `ahma` resolved on `PATH`). The editor interprets the
/// hook's stdout + exit code, so letting clap print its top-level usage/help
/// would dump the entire CLI banner as the "Hook blocked with message" payload.
/// Instead, detect a `hooks exec` invocation from the raw argv and emit a concise
/// fail-open `allow` decision so the user's terminal is never wedged by ahma's
/// own breakage (the same fail-open philosophy [`run_exec`] uses for malformed
/// stdin payloads).
///
/// Returns `true` when it handled the invocation (the caller must then exit 0
/// without letting clap print anything). Returns `false` for non-hook
/// invocations, where the normal clap error/help should be shown.
pub fn try_emit_exec_parse_error_fallback() -> bool {
    let args: Vec<String> = std::env::args().collect();
    // The post-execution observer is purely informational: on a parse failure
    // emit an empty object (no added context) rather than a usage dump.
    let is_hook_observe = args
        .windows(2)
        .any(|w| w[0] == "hooks" && w[1] == "observe");
    if is_hook_observe {
        let _ = write_exec_output(&json!({}));
        return true;
    }
    let is_hook_exec = args.windows(2).any(|w| w[0] == "hooks" && w[1] == "exec");
    if !is_hook_exec {
        return false;
    }
    let platform = sniff_platform(&args).unwrap_or(HookPlatform::Cursor);
    let output = build_exec_output(HooksDecision::AllowUnchanged, platform);
    // Best-effort: even if the write fails, an empty stdout is far better than a
    // usage dump — the editor treats "no decision" as allow.
    let _ = write_exec_output(&output);
    true
}

/// Parse `--platform <value>` (or `--platform=<value>`) out of raw argv for the
/// parse-error fallback, where clap's own parsing is unavailable. Returns `None`
/// for a missing/unknown value so the caller can apply its default.
fn sniff_platform(args: &[String]) -> Option<HookPlatform> {
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        let val = if let Some(v) = a.strip_prefix("--platform=") {
            Some(v.to_string())
        } else if a == "--platform" {
            iter.next().cloned()
        } else {
            None
        };
        if let Some(v) = val {
            return parse_platform_cli_name(&v);
        }
    }
    None
}

/// Resolve a `--platform` CLI value (e.g. `"cursor"`) to its [`HookPlatform`],
/// reusing [`HookPlatform::cli_name`] as the single source of truth so this and
/// clap's own `ValueEnum` parsing can never drift apart.
fn parse_platform_cli_name(value: &str) -> Option<HookPlatform> {
    HookPlatform::all()
        .into_iter()
        .find(|p| p.cli_name() == value)
}

/// `ahma hooks approve-unsandboxed` — grant session-scoped consent (R5.5.3).
fn run_approve_unsandboxed() -> Result<()> {
    let store = HookConsentStore::current();
    store
        .grant()
        .context("Failed to record unsandboxed-execution consent")?;
    println!(
        "✓ Unsandboxed hook execution APPROVED for this session.\n\
         Commands that ahma cannot sandbox will now run UNSANDBOXED with a warning.\n\
         This consent does NOT persist across a reboot. Revoke anytime: `ahma hooks revoke`.\n\
         Prefer to fix the root cause? Run `ahma hooks doctor`."
    );
    Ok(())
}

/// `ahma hooks revoke` — clear session consent; fall-open fails closed again.
fn run_revoke_consent() -> Result<()> {
    HookConsentStore::current()
        .revoke()
        .context("Failed to revoke unsandboxed-execution consent")?;
    println!(
        "✓ Unsandboxed hook consent REVOKED. Commands ahma cannot sandbox will now \
         fail closed (be blocked) until you repair ahma or re-approve."
    );
    Ok(())
}

/// `ahma hooks doctor` — report why ahma may be unable to sandbox, plus consent state.
fn run_doctor() -> Result<()> {
    println!("ahma hooks doctor\n");

    match std::env::current_exe() {
        Ok(p) => println!("  binary       : {}", p.display()),
        Err(e) => println!("  binary       : <unknown> ({e})"),
    }
    println!("  version      : {}", env!("CARGO_PKG_VERSION"));

    match crate::sandbox::test_sandbox_exec_available() {
        Ok(()) => println!("  kernel sandbox: AVAILABLE"),
        Err(e) => println!(
            "  kernel sandbox: UNAVAILABLE — {e}\n\
             \n  Repair: reinstall/update ahma so the hook binary can enforce the sandbox,\n\
             then retry. On macOS ensure `sandbox-exec` is present; on Linux ensure a\n\
             Landlock-capable kernel (5.13+)."
        ),
    }

    let store = HookConsentStore::current();
    if store.is_consented() {
        println!(
            "  consent      : GRANTED ({} unsandboxed run(s) this session)",
            store.count()
        );
        if let Some(banner) = store.banner() {
            println!("\n{banner}");
        }
    } else {
        println!("  consent      : none (fall-open fails closed; commands are blocked)");
    }
    Ok(())
}

pub fn run_install(args: HooksInstallArgs) -> Result<()> {
    let env = HookEnvironment::detect()?;
    let selected = if args.platforms.is_empty() {
        HookPlatform::all()
    } else {
        args.platforms.clone()
    };

    for platform in selected {
        let path = env.config_path(platform, args.scope);
        let existed = path.exists();
        let mut document = load_hook_document(&path)?;
        let before = document.clone();

        install_platform_hook(&mut document, platform, args.scope, &env)?;
        let action = write_hook_document(&path, &before, &document, args.dry_run, existed)?;

        println!(
            "{} {} hook {} at {}",
            platform.label(),
            args.scope.label(),
            action_message(action, args.dry_run),
            path.display()
        );
    }

    Ok(())
}

pub fn run_uninstall(args: HooksUninstallArgs) -> Result<()> {
    let env = HookEnvironment::detect()?;

    for platform in selected_platforms(&args.platforms) {
        uninstall_single_platform_hook(platform, args.scope, args.dry_run, &env)?;
    }

    Ok(())
}

fn uninstall_single_platform_hook(
    platform: HookPlatform,
    scope: HookScope,
    dry_run: bool,
    env: &HookEnvironment,
) -> Result<()> {
    let path = env.config_path(platform, scope);
    if !path.exists() {
        println!(
            "{} {} hook not installed (missing {})",
            platform.label(),
            scope.label(),
            path.display()
        );
        return Ok(());
    }

    let mut document = load_hook_document(&path)?;
    let before = document.clone();
    let changed = uninstall_platform_hook(&mut document, platform)?;

    if !changed {
        println!(
            "{} {} hook already absent at {}",
            platform.label(),
            scope.label(),
            path.display()
        );
        return Ok(());
    }

    let action = write_hook_document(&path, &before, &document, dry_run, true)?;
    println!(
        "{} {} hook {} at {}",
        platform.label(),
        scope.label(),
        action_message(action, dry_run),
        path.display()
    );
    Ok(())
}

fn hook_status_string(path: &Path, platform: HookPlatform) -> Result<String> {
    if !path.exists() {
        return Ok("missing".to_string());
    }
    let document = load_hook_document(path)?;
    if platform_hook_installed(&document, platform) {
        Ok("installed".to_string())
    } else {
        Ok("not installed".to_string())
    }
}

/// Returns `true` when ahma's terminal hooks should route commands through the sandbox.
///
/// Controlled by the `AHMA_HOOKS` env var (`on`/`off`/`auto`, default `auto`).
/// `AHMA_DISABLE_HOOKS=1` is an alias for `AHMA_HOOKS=off`.
///
/// In `auto` mode ahma is considered active if an ahma MCP server is present in any
/// detected editor config file. When the user removes/disables ahma from the MCP config,
/// the hook automatically passes commands through to the default terminal.
fn is_ahma_hooks_active() -> bool {
    is_ahma_hooks_active_with_configs(&detect_active_mcp_configs())
}

/// Process-wide hooks-mode override set from the `--hooks` CLI flag.
/// `Some(true)` = forced on, `Some(false)` = forced off, `None` = auto.
static HOOKS_MODE_OVERRIDE: std::sync::OnceLock<Option<bool>> = std::sync::OnceLock::new();

/// Set hooks behaviour from the `--hooks on|off|auto` CLI flag.
/// Call once, early in startup. Takes precedence over `AHMA_HOOKS` /
/// `AHMA_DISABLE_HOOKS` (which remain supported because hook subprocesses
/// can only be configured through the environment).
pub fn set_hooks_mode_override(mode: &str) {
    let parsed = match mode.to_lowercase().as_str() {
        "off" | "0" | "false" | "no" => Some(false),
        "on" | "1" | "true" | "yes" => Some(true),
        _ => None, // "auto" and anything else
    };
    let _ = HOOKS_MODE_OVERRIDE.set(parsed);
}

/// Testable core of [`is_ahma_hooks_active`].
///
/// Delegates to [`describe_activation`] (same precedence: forced override,
/// `AHMA_HOOKS`, `AHMA_DISABLE_HOOKS`, then auto-detection) and discards the
/// human-readable reason, so the precedence chain has exactly one implementation.
fn is_ahma_hooks_active_with_configs(active_mcps: &[PathBuf]) -> bool {
    describe_activation(active_mcps).0
}

/// The effective hook activation and a short human reason, mirroring the exact
/// precedence in [`is_ahma_hooks_active_with_configs`]. Used by `ahma hooks status`
/// so the user can see whether installed hooks actually *do* anything — installed
/// but inactive hooks pass every command through UNSANDBOXED.
fn describe_activation(active_mcps: &[PathBuf]) -> (bool, String) {
    if let Some(Some(forced)) = HOOKS_MODE_OVERRIDE.get() {
        return (
            *forced,
            format!("--hooks {} flag", if *forced { "on" } else { "off" }),
        );
    }
    if let Ok(val) = std::env::var("AHMA_HOOKS") {
        match val.to_lowercase().as_str() {
            "off" | "0" | "false" | "no" => return (false, "AHMA_HOOKS=off".to_string()),
            "on" | "1" | "true" | "yes" => return (true, "AHMA_HOOKS=on".to_string()),
            _ => {}
        }
    }
    if std::env::var("AHMA_DISABLE_HOOKS")
        .ok()
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return (false, "AHMA_DISABLE_HOOKS=1".to_string());
    }
    if active_mcps.is_empty() {
        (
            false,
            "auto: no ahma MCP server found in any known client config".to_string(),
        )
    } else {
        (
            true,
            "auto: ahma MCP server detected in client config".to_string(),
        )
    }
}

fn detect_active_mcp_configs() -> Vec<PathBuf> {
    // Use `dirs::home_dir()` (not `$HOME`): on Windows `$HOME` is usually unset,
    // which previously made auto-detection silently report "inactive" there.
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    detect_active_mcp_configs_in(&home, detect_project_root().ok().as_deref())
}

/// Candidate MCP-config files, one per client `ahma setup` can write to. This MUST
/// stay in sync with `setup.rs::Platform::configure_mcp`: if a client writes its
/// ahma MCP server to a path that is not listed here, `auto` mode will fail to
/// detect ahma and silently pass commands through UNSANDBOXED even though the hook
/// is installed. The previous list omitted Claude Code (`~/.claude.json`), Codex
/// (`~/.codex/config.toml`) and Antigravity (`~/.gemini/config/mcp_config.json`) —
/// exactly the clients that get both hooks and an MCP server.
fn mcp_config_candidates(home: &Path, project_root: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = vec![
        // Cursor
        home.join(".cursor").join("mcp.json"),
        // Claude Code (`ahma setup` writes the MCP server here)
        home.join(".claude.json"),
        // Codex CLI
        home.join(".codex").join("config.toml"),
        // Antigravity / Gemini
        home.join(".gemini").join("config").join("mcp_config.json"),
        // LM Studio
        home.join(".lmstudio").join("mcp.json"),
        // VS Code (GitHub Copilot Chat)
        home.join("Library/Application Support/Code/User/mcp.json"),
        home.join(".config/Code/User/mcp.json"),
        home.join("AppData/Roaming/Code/User/mcp.json"),
        // Claude Desktop
        home.join("Library/Application Support/Claude/claude_desktop_config.json"),
        home.join(".config/Claude/claude_desktop_config.json"),
        home.join("AppData/Roaming/Claude/claude_desktop_config.json"),
    ];
    if let Some(project_root) = project_root {
        paths.push(project_root.join(".vscode").join("mcp.json"));
    }
    paths
}

/// Testable core of [`detect_active_mcp_configs`]: return the config files that
/// mention an ahma MCP server. The match is a case-insensitive `ahma` substring so
/// it works across both JSON (`"Ahma": { … }`) and Codex's TOML
/// (`[mcp_servers.Ahma]`, which has no quotes). Erring toward "detected" is the
/// fail-secure direction — these files are only consulted when the user already
/// chose to install hooks, so a false positive merely sandboxes more, never less.
fn detect_active_mcp_configs_in(home: &Path, project_root: Option<&Path>) -> Vec<PathBuf> {
    let mut active = Vec::new();
    for path in mcp_config_candidates(home, project_root) {
        if path.exists()
            && let Ok(content) = std::fs::read_to_string(&path)
            && content.to_lowercase().contains("ahma")
        {
            active.push(path);
        }
    }
    active
}

fn run_status(args: HooksStatusArgs) -> Result<()> {
    let env = HookEnvironment::detect()?;
    let scopes = match args.scope {
        Some(scope) => vec![scope],
        None => vec![HookScope::User, HookScope::Project],
    };

    // Lead with the *effective* state. "installed" only means the hook file exists;
    // in `auto` mode an installed hook is inert (passes commands through
    // UNSANDBOXED) until an ahma MCP server is detected. The user must be able to
    // tell these apart.
    let active_mcps = detect_active_mcp_configs();
    let (active, reason) = describe_activation(&active_mcps);
    print_effective_activation_banner(active, &reason);

    println!("{:<14} {:<8} {:<14} Config", "Platform", "Scope", "Status");
    println!("{:-<14} {:-<8} {:-<14} {:-<6}", "", "", "", "");

    let installed_hooks = print_hook_status_rows(&env, &scopes, &args.platforms)?;

    if !installed_hooks.is_empty() && !active_mcps.is_empty() {
        print_hooks_mcp_coexistence_note(&installed_hooks, &active_mcps);
    }

    Ok(())
}

/// Print the "Effective: ACTIVE/INACTIVE" banner shown at the top of `hooks status`.
/// Split out of [`run_status`] so that function reads as a sequence of steps rather
/// than an if/else concentrated alongside the status-table loop.
fn print_effective_activation_banner(active: bool, reason: &str) {
    if active {
        println!(
            "\x1b[1mEffective: \x1b[32mACTIVE\x1b[0m\x1b[1m\x1b[0m ({reason}) — installed hooks route shell commands through ahma's sandbox.\n"
        );
    } else {
        println!(
            "\x1b[1mEffective: \x1b[33mINACTIVE\x1b[0m ({reason}) — installed hooks pass commands through to the default terminal \x1b[1mUNSANDBOXED\x1b[0m.\n      Force on with AHMA_HOOKS=on, or fix ahma's MCP config so `auto` detects it.\n"
        );
    }
}

/// Print one status row per (scope, platform) pair and return the ones that are
/// installed. Split out of [`run_status`] so the nested scope/platform loop is a
/// single self-contained step in that function's flow.
fn print_hook_status_rows(
    env: &HookEnvironment,
    scopes: &[HookScope],
    requested_platforms: &[HookPlatform],
) -> Result<Vec<(HookPlatform, HookScope, PathBuf)>> {
    let mut installed_hooks = Vec::new();
    for &scope in scopes {
        for platform in selected_platforms(requested_platforms) {
            let path = env.config_path(platform, scope);
            let status = hook_status_string(&path, platform)?;
            if status == "installed" {
                installed_hooks.push((platform, scope, path.clone()));
            }
            println!(
                "{:<14} {:<8} {:<14} {}",
                platform.label(),
                scope.label(),
                status,
                path.display()
            );
        }
    }
    Ok(installed_hooks)
}

/// Print the explanatory note shown when terminal hooks AND an ahma MCP server are
/// both active. Split out of [`run_status`] so that function stays focused on
/// gathering state; this is pure presentation with no branching logic of its own.
fn print_hooks_mcp_coexistence_note(
    installed_hooks: &[(HookPlatform, HookScope, PathBuf)],
    active_mcps: &[PathBuf],
) {
    println!(
        "\n\x1b[36mnote\x1b[0m\x1b[1m: terminal hooks and an MCP server are both active for \"ahma\" — this is supported\x1b[0m"
    );
    println!(
        "      The two are \x1b[1mcomplementary\x1b[0m, not redundant. They sandbox different"
    );
    println!("      command streams:");
    println!(
        "        \x1b[1m• MCP server\x1b[0m   — the named, async tools the agent calls explicitly"
    );
    println!("                         (run_terminal_command, file-tools, git, …) with output");
    println!("                         capture, monitoring and concurrency.");
    println!(
        "        \x1b[1m• terminal hooks\x1b[0m — transparently sandbox the shell commands the agent"
    );
    println!("                         runs through its NATIVE terminal/Bash tool, which never");
    println!("                         pass through MCP. Without hooks those run unsandboxed.");
    println!("      Together they give full sandbox coverage. A command is only ever wrapped");
    println!("      once (already-wrapped and MCP tool calls are passed through untouched).");
    println!();
    println!("  \x1b[1mactive terminal hooks:\x1b[0m");
    for (platform, scope, path) in installed_hooks {
        println!(
            "    - {} ({} scope) at {}",
            platform.label(),
            scope.label(),
            path.display()
        );
    }
    println!();
    println!("  \x1b[1mactive MCP configurations:\x1b[0m");
    for path in active_mcps {
        println!("    - {}", path.display());
    }
    println!();
    println!(
        "  \x1b[1mtradeoff\x1b[0m: hooks add a small per-command sandbox cold-start. If your agent only"
    );
    println!(
        "            uses ahma's MCP tools and never its native terminal, you can drop hooks:"
    );

    let has_user = installed_hooks
        .iter()
        .any(|(_, s, _)| *s == HookScope::User);
    let has_project = installed_hooks
        .iter()
        .any(|(_, s, _)| *s == HookScope::Project);
    if has_user {
        println!("              ahma hooks uninstall --scope user");
    }
    if has_project {
        println!("              ahma hooks uninstall --scope project");
    }
    println!();
}

fn run_exec(args: HooksExecArgs) -> Result<()> {
    // Parse stdin first. On any failure allow through — the editor may have sent an
    // empty or malformed payload (e.g. during IDE shutdown) and we must not block.
    let stdin = match read_stdin_json() {
        Ok(s) => s,
        Err(_) => {
            let output = build_exec_output(HooksDecision::AllowUnchanged, args.platform);
            return write_exec_output(&output);
        }
    };

    // Detect the runtime environment. On failure we FAIL OPEN: the command runs
    // unsandboxed via the default terminal, accompanied by a loud warning. We
    // must never block the user's terminal just because ahma broke.
    let env = match HookEnvironment::detect() {
        Ok(e) => e,
        Err(e) => {
            let decision = if is_ahma_hooks_active() {
                let consented = HookConsentStore::current().is_consented();
                unsandboxable_decision(&format!("ahma hook setup failed: {e}"), consented)
            } else {
                HooksDecision::AllowUnchanged
            };
            return emit_decision(decision, args.platform);
        }
    };

    let decision = compute_exec_decision(&stdin, args.scope, &env);
    emit_decision(decision, args.platform)
}

/// `ahma hooks observe` — the post-execution observer (SPEC R7 disclosure).
///
/// Invoked by the managed `afterShellExecution` hook *after* a deferred-to-host
/// command finishes. It scans the command's output for a host-sandbox denial and,
/// on a hit, returns an actionable remediation the editor can surface. It is
/// strictly observational: it NEVER blocks, denies, or errors — a post hook that
/// failed the editor would be worse than the silent failure it is trying to fix.
/// Any parse problem or absent signature emits an empty object (no-op).
fn run_observe(_args: HooksObserveArgs) -> Result<()> {
    let stdin = match read_stdin_json() {
        Ok(s) => s,
        Err(_) => return write_exec_output(&json!({})),
    };

    let output = extract_command_output(&stdin);
    let failed = command_failed(&stdin);

    match post_exec::surface_sandbox_denial(&output, failed) {
        Some(remediation) => write_exec_output(&build_cursor_observe_output(&remediation)),
        None => write_exec_output(&json!({})),
    }
}

/// Collect the textual output of a finished command from the post-execution hook
/// payload. Cursor's `afterShellExecution` payload shape is not contractually
/// fixed, so this reads defensively from the keys that have carried command text
/// (top-level and a nested `tool_output` object), concatenating any it finds.
const COMMAND_OUTPUT_KEYS: &[&str] = &[
    "output",
    "stdout",
    "stderr",
    "result",
    "outputText",
    "text",
    "error",
];

fn extract_command_output(input: &Value) -> String {
    let mut combined = String::new();
    if let Some(object) = input.as_object() {
        collect_command_output_fields(object, &mut combined);
        if let Some(nested) = object.get("tool_output").and_then(Value::as_object) {
            collect_command_output_fields(nested, &mut combined);
        }
    }
    combined
}

/// Append the text found under any of [`COMMAND_OUTPUT_KEYS`] in `object` to `into`,
/// newline-joined. Split out of [`extract_command_output`] so the two levels of the
/// payload (top-level and nested `tool_output`) share one code path.
fn collect_command_output_fields(object: &Map<String, Value>, into: &mut String) {
    for key in COMMAND_OUTPUT_KEYS {
        if let Some(s) = object.get(*key).and_then(Value::as_str)
            && !s.is_empty()
        {
            if !into.is_empty() {
                into.push('\n');
            }
            into.push_str(s);
        }
    }
}

/// Whether the finished command should be treated as failed. An explicit non-zero
/// `exit_code`/`exitCode`, or an `aborted: true`, means failure; an unknown status
/// is treated as failed so a clear denial signature is never suppressed (the
/// signature only appears on failure anyway).
fn command_failed(input: &Value) -> bool {
    if let Some(code) = input
        .get("exit_code")
        .or_else(|| input.get("exitCode"))
        .and_then(Value::as_i64)
    {
        return code != 0;
    }
    if let Some(aborted) = input.get("aborted").and_then(Value::as_bool) {
        return aborted;
    }
    true
}

/// Build the Cursor post-hook output carrying the remediation. `additional_context`
/// is the documented `postToolUse`/post-event field (injected into the agent's
/// context); `agent_message`/`user_message` are emitted too as belt-and-suspenders
/// (Cursor ignores fields an event does not support).
fn build_cursor_observe_output(remediation: &str) -> Value {
    json!({
        "additional_context": remediation,
        "agent_message": remediation,
        "user_message": remediation,
    })
}

/// Build the managed `afterShellExecution` observe command for Cursor.
fn build_observe_command(scope: HookScope, env: &HookEnvironment) -> String {
    BinaryReference::for_scope(env, scope).build_command(&observe_args(scope))
}

fn observe_args(scope: HookScope) -> Vec<String> {
    vec![
        "hooks".to_string(),
        "observe".to_string(),
        "--platform".to_string(),
        HookPlatform::Cursor.cli_name().to_string(),
        "--scope".to_string(),
        scope.cli_name().to_string(),
        "--managed-id".to_string(),
        MANAGED_ID_DEFAULT_SHELL_V1.to_string(),
    ]
}

/// Build the decision for a command ahma cannot sandbox (SPEC R5.5.3).
///
/// Fails closed unless the user has consented to unsandboxed execution this
/// session, in which case it allows the command unsandboxed with a loud warning.
/// Pass `consented` from [`HookConsentStore::is_consented`] (threaded in so the
/// decision logic stays pure and unit-testable).
fn unsandboxable_decision(reason: &str, consented: bool) -> HooksDecision {
    if consented {
        HooksDecision::AllowWithWarning {
            user_message: format!(
                "⚠️  ahma sandbox UNAVAILABLE — {reason}. Running UNSANDBOXED \
                 (you approved this session with `ahma hooks approve-unsandboxed`). \
                 Run `ahma hooks doctor` to repair, or `ahma hooks revoke` to stop."
            ),
            agent_message: format!(
                "WARNING: ahma sandboxing is unavailable ({reason}). This command is \
                 running WITHOUT kernel-level sandboxing under a session consent. \
                 File writes are NOT confined to the workspace. Proceed with caution."
            ),
        }
    } else {
        HooksDecision::DenyPendingConsent {
            user_message: format!(
                "⛔ ahma sandbox UNAVAILABLE — {reason}. The command was BLOCKED to \
                 avoid running unsandboxed. Fix: run `ahma hooks doctor` to repair the \
                 ahma installation. To run unsandboxed for THIS session anyway, run \
                 `ahma hooks approve-unsandboxed` and retry. To stop sandboxing entirely, \
                 run `ahma hooks uninstall` or set AHMA_HOOKS=off."
            ),
            agent_message: format!(
                "BLOCKED: ahma could not sandbox this command ({reason}) and unsandboxed \
                 execution has not been approved this session. The command did NOT run. \
                 Tell the user to run `ahma hooks doctor` (repair) or \
                 `ahma hooks approve-unsandboxed` (allow unsandboxed this session), then retry."
            ),
        }
    }
}

/// Whether the user has explicitly asked ahma to apply its OWN sandbox even when
/// nested inside a host sandbox (re-introducing the redundant double-sandbox and
/// the host build-cache friction, but giving ahma's tighter scope). Opt-in via
/// `AHMA_PREFER_OWN_SANDBOX` (truthy).
fn prefer_own_sandbox() -> bool {
    match std::env::var("AHMA_PREFER_OWN_SANDBOX") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !(v.is_empty() || v == "0" || v == "false" || v == "no")
        }
        Err(_) => false,
    }
}

/// Build the decision to defer to a detected host sandbox (R7). The original
/// command is allowed UNCHANGED (so it runs inside the host's kernel sandbox) and
/// a loud, honest disclosure states which sandbox is protecting it.
fn defer_to_host_decision(host: crate::sandbox::HostSandbox) -> HooksDecision {
    let disclosure = crate::sandbox::ActiveSandbox::DeferredToHost(host).disclosure_line();
    HooksDecision::DeferToHost {
        user_message: disclosure.clone(),
        agent_message: format!(
            "{disclosure} ahma did not re-sandbox this command (deferred to host to avoid a \
             redundant double-sandbox). To force ahma's own sandbox instead, set \
             AHMA_PREFER_OWN_SANDBOX=1."
        ),
    }
}

/// Write the decision to stdout for the IDE, and mirror any fail-open warning to
/// stderr so it is visible even when the IDE does not surface allow-time messages.
fn emit_decision(decision: HooksDecision, platform: HookPlatform) -> Result<()> {
    match &decision {
        HooksDecision::AllowWithWarning { user_message, .. }
        | HooksDecision::DenyPendingConsent { user_message, .. }
        | HooksDecision::DeferToHost { user_message, .. } => {
            eprintln!("{user_message}");
        }
        _ => {}
    }
    let output = build_exec_output(decision, platform);
    write_exec_output(&output)
}

fn write_exec_output(output: &Value) -> Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    serde_json::to_writer(&mut handle, output)?;
    handle.write_all(b"\n")?;
    Ok(())
}

async fn run_shell(args: HooksRunShellArgs, cfg: AppConfig) -> Result<()> {
    let payload = decode_wrapped_shell_payload(&args.payload_base64)?;
    std::env::set_current_dir(&payload.cwd)
        .with_context(|| format!("Failed to change directory to {}", payload.cwd))?;

    let cfg = AppConfig {
        run_tool: Some("run_terminal_command".to_string()),
        run_tool_args: vec![payload.command.clone()],
        skip_availability_probes: true,
        // Sandbox the command in the working directory the IDE hook explicitly
        // handed us. This is not a spoofable CWD inference (SPEC R5.2.1): the IDE
        // passes the command's own execution directory in the hook payload, which
        // is exactly the scope this command should be confined to — the same role
        // roots/list plays for the MCP server. Without this the sandbox falls back
        // to the default `~/sandbox`, so every real project command is rejected as
        // "outside the sandbox root" and the hook blocks it (fail-closed, R5.5.3).
        sandbox_scopes: vec![PathBuf::from(&payload.cwd)],
        use_sandbox_dir: false,
        ..cfg
    };

    // FAIL OPEN: if the sandbox cannot be initialized (missing kernel support,
    // bad scope, etc.) we must not wedge the user's terminal. Run the command
    // unsandboxed with a loud warning instead of bubbling up an error.
    let sandbox = match crate::shell::cli::initialize_sandbox(&cfg) {
        Ok(Some(sandbox)) => sandbox,
        Ok(None) => {
            return run_command_unsandboxed(&payload.cwd, &payload.command, "sandbox is disabled")
                .await;
        }
        Err(e) => {
            return run_command_unsandboxed(
                &payload.cwd,
                &payload.command,
                &format!("sandbox initialization failed: {e}"),
            )
            .await;
        }
    };

    let monitor_config = crate::operation_monitor::MonitorConfig::with_timeout(
        std::time::Duration::from_secs(cfg.timeout_secs),
    );
    let operation_monitor = std::sync::Arc::new(crate::operation_monitor::OperationMonitor::new(
        monitor_config,
    ));

    let shell_pool_config = crate::shell_pool::ShellPoolConfig {
        command_timeout: std::time::Duration::from_secs(cfg.timeout_secs),
        ..Default::default()
    };
    let shell_pool_manager =
        std::sync::Arc::new(crate::shell_pool::ShellPoolManager::new(shell_pool_config));

    let mutex_registry = std::sync::Arc::new(crate::adapter::CommandMutexRegistry::from_config(
        &cfg.mutex_groups,
    ));
    let adapter = std::sync::Arc::new(crate::adapter::Adapter::new_with_registry(
        operation_monitor,
        shell_pool_manager,
        sandbox,
        mutex_registry,
    )?);

    let mut adapter_args = serde_json::Map::new();
    adapter_args.insert(
        "command".to_string(),
        serde_json::Value::String(payload.command.clone()),
    );
    adapter_args.insert("c_flag".to_string(), serde_json::Value::Bool(true));

    let timeout = Some(cfg.timeout_secs);
    let subcommand_config = crate::AhmaMcpService::build_shell_subcommand_config(
        timeout,
        &crate::adapter::ExecutionMode::Synchronous,
    );

    let result = adapter
        .execute_sync_in_dir(
            crate::shell_pool::platform_shell_program(),
            Some(adapter_args),
            &payload.cwd,
            timeout,
            Some(&subcommand_config),
        )
        .await;

    match result {
        Ok(output) => {
            println!("{}", output);
            Ok(())
        }
        // The sandbox initialized successfully (the `initialize_sandbox` arms
        // above handle the "cannot sandbox" case), so the command DID run inside
        // the sandbox. A non-zero exit — a failed build/test, a `grep` with no
        // match, OR a write the kernel sandbox correctly denied — is the
        // command's own result and is surfaced verbatim. We deliberately do NOT
        // re-run it unsandboxed here: doing so would (1) misreport every ordinary
        // command failure as "sandbox unavailable / BLOCKED", which is exactly
        // what made hooks look like they were blocking CLI calls, and (2) let a
        // command the sandbox just blocked succeed on the unsandboxed retry — a
        // silent confinement bypass. The SPEC R5.5.3 unsandboxed fallback applies
        // only when the sandbox cannot be initialized at all.
        //
        // One refinement: when the failure was an out-of-scope *runtime* denial
        // (`SandboxError::RuntimeDenial`), append an actionable `ahma sandbox
        // grant ...` recovery line so the agent gets a next step instead of a raw
        // `os error 1`. The native-terminal hook uses the CLI grant path (not the
        // MCP grant/restart tools).
        Err(e) => {
            if let Some(crate::sandbox::SandboxError::RuntimeDenial { path, access, .. }) =
                e.downcast_ref::<crate::sandbox::SandboxError>()
            {
                let remediation =
                    crate::sandbox::grant_channel::runtime_denial_remediation_cli(path, *access);
                eprintln!("{e}\n\n{remediation}");
                return Err(anyhow!("{e}\n\n{remediation}"));
            }
            Err(e)
        }
    }
}

/// Fallback path when ahma's own execution cannot sandbox the command (SPEC
/// R5.5.3). Fails closed unless the user consented to unsandboxed execution this
/// session; when consented, runs `command` in the platform shell with a loud,
/// counted warning and forwards stdout/stderr and exit status.
async fn run_command_unsandboxed(cwd: &str, command: &str, reason: &str) -> Result<()> {
    let store = HookConsentStore::current();
    if !store.is_consented() {
        // Fail closed: do NOT run the command unsandboxed without explicit consent.
        bail!(
            "⛔ ahma sandbox UNAVAILABLE — {reason}. Command BLOCKED (not run) to avoid \
             unsandboxed execution. Fix: `ahma hooks doctor` to repair, or \
             `ahma hooks approve-unsandboxed` to allow unsandboxed for this session, then retry."
        );
    }
    let count = store.record_unsandboxed_run().unwrap_or(0);
    eprintln!(
        "\n⚠️  ahma sandbox UNAVAILABLE — {reason}.\n\
         ⚠️  Running UNSANDBOXED ({count} this session) under your `approve-unsandboxed` consent.\n\
         ⚠️  File writes are NOT confined to the workspace. Run `ahma hooks doctor` to repair,\n\
         ⚠️  or `ahma hooks revoke` to stop allowing unsandboxed execution.\n"
    );

    let shell = crate::shell_pool::platform_shell_program();
    let mut cmd = tokio::process::Command::new(shell);
    cmd.current_dir(cwd).kill_on_drop(true);

    #[cfg(target_os = "windows")]
    {
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", command]);
    }
    #[cfg(not(target_os = "windows"))]
    {
        cmd.args(["-c", command]);
    }

    let status = cmd
        .status()
        .await
        .with_context(|| format!("Failed to spawn fallback shell '{shell}'"))?;

    if status.success() {
        Ok(())
    } else {
        // Mirror the command's failure so the agent sees a non-zero exit, but do
        // not wrap it as an ahma error — the command ran, it just failed.
        Err(anyhow!(
            "command exited with status {} (ran unsandboxed: {reason})",
            status.code().unwrap_or(-1)
        ))
    }
}

fn action_message(action: FileAction, dry_run: bool) -> &'static str {
    match (action, dry_run) {
        (FileAction::Created, true) => "would be created",
        (FileAction::Updated, true) => "would be updated",
        (FileAction::Unchanged, true) => "already matches",
        (FileAction::Created, false) => "installed",
        (FileAction::Updated, false) => "updated",
        (FileAction::Unchanged, false) => "already matches",
    }
}

fn selected_platforms(requested: &[HookPlatform]) -> Vec<HookPlatform> {
    if requested.is_empty() {
        HookPlatform::all()
    } else {
        requested.to_vec()
    }
}

fn detect_project_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("Failed to determine current working directory")?;
    let cwd = dunce::canonicalize(&cwd).unwrap_or(cwd);

    for ancestor in cwd.ancestors() {
        if ancestor.join(".git").exists() {
            return Ok(ancestor.to_path_buf());
        }
    }

    Ok(cwd)
}

#[derive(Debug)]
struct ExtractedToolArgs {
    tool_input: Map<String, Value>,
    command: String,
    arg_key: String,
}

fn extract_tool_args(input: &Value) -> Result<Option<ExtractedToolArgs>> {
    let Some(raw_args) = input.get("tool_input").or_else(|| input.get("toolArgs")) else {
        // Not a shell tool invocation — allow through without modification
        return Ok(None);
    };

    let args_val = if let Some(s) = raw_args.as_str() {
        serde_json::from_str(s).context("Failed to parse toolArgs JSON string")?
    } else {
        raw_args.clone()
    };

    let args_obj = args_val
        .as_object()
        .ok_or_else(|| anyhow!("Tool arguments must be a JSON object"))?;

    let (command, key) = if let Some(c) = args_obj.get("command").and_then(Value::as_str) {
        (c, "command")
    } else if let Some(c) = args_obj.get("CommandLine").and_then(Value::as_str) {
        (c, "CommandLine")
    } else {
        // Tool does not invoke a shell command (e.g. editFiles, createFile) — allow through
        return Ok(None);
    };

    Ok(Some(ExtractedToolArgs {
        tool_input: args_obj.clone(),
        command: command.to_string(),
        arg_key: key.to_string(),
    }))
}

/// Compute the hook decision for a given tool invocation.
///
/// This is the testable core of [`run_exec`]. The decision tree:
/// 1. Non-shell tools (no `command`/`CommandLine` field) → allow unchanged.
/// 2. Already-wrapped commands → allow unchanged (prevent double-wrap).
/// 3. `AHMA_HOOKS=off` or auto with no MCP configured → allow unchanged
///    (passthrough — the command runs unsandboxed in the default terminal; this is
///    the only genuinely "open" path, and it is silent by design because ahma is
///    not active).
/// 4. Shell command + ahma active → rewrite to `ahma hooks run-shell` (sandbox path).
/// 5. Active but ahma cannot sandbox (no cwd / wrapper build fails) → **fail
///    closed pending consent** (R5.5.3): the command is DENIED with actionable
///    guidance until the user runs `ahma hooks approve-unsandboxed`, after which it
///    runs unsandboxed with a loud, counted warning. ahma never silently runs a
///    command unsandboxed while it believes it is active.
fn compute_exec_decision(input: &Value, scope: HookScope, env: &HookEnvironment) -> HooksDecision {
    let consented = HookConsentStore::current().is_consented();
    // Detect a host sandbox to defer to (R7), unless the user prefers ahma's own.
    let host = if prefer_own_sandbox() {
        None
    } else {
        crate::sandbox::detect_host_sandbox()
    };
    let decision =
        compute_exec_decision_internal(input, scope, env, is_ahma_hooks_active(), consented, host);
    // When we are about to allow an UNSANDBOXED run under consent, count it so the
    // persistent banner reflects how many commands have bypassed the sandbox.
    if matches!(decision, HooksDecision::AllowWithWarning { .. }) {
        let _ = HookConsentStore::current().record_unsandboxed_run();
    }
    decision
}

fn compute_exec_decision_internal(
    input: &Value,
    scope: HookScope,
    env: &HookEnvironment,
    active: bool,
    consented: bool,
    host: Option<crate::sandbox::HostSandbox>,
) -> HooksDecision {
    let args = match extract_tool_args(input) {
        Ok(Some(a)) => a,
        Ok(None) | Err(_) => return HooksDecision::AllowUnchanged,
    };

    if is_wrapped_shell_command(&args.command) {
        return HooksDecision::AllowUnchanged;
    }

    if !active {
        return HooksDecision::AllowUnchanged;
    }

    // Capability-based deferral (R7): when a host already kernel-sandboxes this
    // command (Cursor, VS Code, Docker), ahma re-wrapping it would only add the
    // redundant double-sandbox and fight the host's build-cache env injection.
    // Defer to the host and disclose loudly. `host` is resolved by the caller
    // (None when the user set AHMA_PREFER_OWN_SANDBOX or no host was detected).
    if let Some(host) = host {
        return defer_to_host_decision(host);
    }

    // FAIL OPEN: if ahma cannot determine the working directory it cannot
    // sandbox the command. Rather than block the user's terminal, run it
    // unsandboxed with a loud warning.
    let cwd = match extract_command_cwd(input, &args.tool_input) {
        Ok(cwd) => cwd,
        Err(e) => {
            return unsandboxable_decision(
                &format!("could not determine working directory: {e}"),
                consented,
            );
        }
    };

    match build_wrapped_shell_command(scope, env, &cwd, &args.command) {
        Ok(wrapped) => {
            let updated = updated_tool_input(&args.tool_input, wrapped, &args.arg_key);
            HooksDecision::AllowRewrite(updated)
        }
        // ahma cannot build the sandbox wrapper — fail closed unless consented (R5.5.3).
        Err(e) => {
            unsandboxable_decision(&format!("failed to build sandbox wrapper: {e}"), consented)
        }
    }
}

fn build_exec_output(decision: HooksDecision, platform: HookPlatform) -> Value {
    match platform {
        HookPlatform::Cursor => build_cursor_hook_output(decision),
        HookPlatform::Claude
        | HookPlatform::Codex
        | HookPlatform::Copilot
        | HookPlatform::Antigravity => build_structured_hook_output(decision),
    }
}

fn extract_command_cwd(input: &Value, tool_input: &Map<String, Value>) -> Result<String> {
    if let Some(cwd) = tool_input
        .get("working_directory")
        .and_then(Value::as_str)
        .or_else(|| input.get("cwd").and_then(Value::as_str))
    {
        return Ok(cwd.to_string());
    }

    let cwd = std::env::current_dir().context("Failed to determine working directory for hook")?;
    Ok(cwd.to_string_lossy().into_owned())
}

fn updated_tool_input(tool_input: &Map<String, Value>, command: String, key: &str) -> Value {
    let mut updated = tool_input.clone();
    updated.insert(key.to_string(), Value::String(command));
    Value::Object(updated)
}

fn build_cursor_hook_output(decision: HooksDecision) -> Value {
    match decision {
        HooksDecision::AllowUnchanged => json!({"permission": "allow"}),
        HooksDecision::AllowRewrite(updated_input) => json!({
            "permission": "allow",
            "updated_input": updated_input,
        }),
        // Fail open: allow the original (unsandboxed) command but attach warnings.
        // Cursor ignores unknown fields, so the messages surface where supported.
        HooksDecision::AllowWithWarning {
            user_message,
            agent_message,
        } => json!({
            "permission": "allow",
            "user_message": user_message,
            "agent_message": agent_message,
        }),
        // Deferred to the host sandbox: allow the original command unchanged so it
        // runs in the host's sandbox; attach the disclosure (R7).
        HooksDecision::DeferToHost {
            user_message,
            agent_message,
        } => json!({
            "permission": "allow",
            "user_message": user_message,
            "agent_message": agent_message,
        }),
        // Fail closed (R5.5.3): deny the command outright.
        HooksDecision::DenyPendingConsent {
            user_message,
            agent_message,
        } => json!({
            "permission": "deny",
            "user_message": user_message,
            "agent_message": agent_message,
        }),
    }
}

fn build_structured_hook_output(decision: HooksDecision) -> Value {
    let hook_specific = match decision {
        HooksDecision::AllowUnchanged => json!({
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
        }),
        HooksDecision::AllowRewrite(updated_input) => json!({
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "updatedInput": updated_input.clone(),
            "modifiedArgs": updated_input,
        }),
        // Fail open: allow the original (unsandboxed) command, but surface the
        // warning to the agent (and a system message where the client shows it).
        HooksDecision::AllowWithWarning {
            user_message,
            agent_message,
        } => json!({
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "agentMessage": agent_message,
            "systemMessage": user_message,
        }),
        // Deferred to the host sandbox: allow unchanged, disclose loudly (R7).
        HooksDecision::DeferToHost {
            user_message,
            agent_message,
        } => json!({
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "agentMessage": agent_message,
            "systemMessage": user_message,
        }),
        // Fail closed (R5.5.3): deny with reason.
        HooksDecision::DenyPendingConsent {
            user_message,
            agent_message,
        } => json!({
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": user_message,
            "agentMessage": agent_message,
            "systemMessage": user_message,
        }),
    };
    json!({"hookSpecificOutput": hook_specific})
}

fn build_wrapped_shell_command(
    scope: HookScope,
    env: &HookEnvironment,
    cwd: &str,
    command: &str,
) -> Result<String> {
    let payload = WrappedShellPayload {
        cwd: cwd.to_string(),
        command: command.to_string(),
    };
    let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload)?);
    let args = vec![
        "hooks".to_string(),
        "run-shell".to_string(),
        "--payload-base64".to_string(),
        encoded,
        "--wrapped-by".to_string(),
        WRAPPED_BY_MARKER.to_string(),
    ];

    Ok(BinaryReference::for_scope(env, scope).build_command(&args))
}

fn decode_wrapped_shell_payload(encoded: &str) -> Result<WrappedShellPayload> {
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .context("Failed to decode wrapped shell payload")?;
    serde_json::from_slice(&bytes).context("Failed to parse wrapped shell payload")
}

fn is_wrapped_shell_command(command: &str) -> bool {
    command.contains(WRAPPED_BY_MARKER) || command.contains("run_terminal_command")
}

fn install_platform_hook(
    document: &mut Value,
    platform: HookPlatform,
    scope: HookScope,
    env: &HookEnvironment,
) -> Result<()> {
    match platform {
        HookPlatform::Cursor => install_cursor_hook(document, scope, env),
        HookPlatform::Claude => install_claude_hook(document, scope, env),
        HookPlatform::Codex => install_codex_hook(document, scope, env),
        HookPlatform::Copilot => install_copilot_hook(document, scope, env),
        HookPlatform::Antigravity => install_grouped_hook(document, platform, scope, env),
    }
}

fn uninstall_platform_hook(document: &mut Value, platform: HookPlatform) -> Result<bool> {
    match platform {
        HookPlatform::Cursor => uninstall_cursor_hook(document),
        HookPlatform::Claude => uninstall_grouped_hook(document, platform),
        HookPlatform::Codex => uninstall_grouped_hook(document, platform),
        HookPlatform::Copilot => uninstall_copilot_hook(document),
        HookPlatform::Antigravity => uninstall_grouped_hook(document, platform),
    }
}

fn platform_hook_installed(document: &Value, platform: HookPlatform) -> bool {
    match platform {
        HookPlatform::Cursor => cursor_hook_installed(document),
        HookPlatform::Claude | HookPlatform::Codex | HookPlatform::Antigravity => {
            grouped_hook_installed(document, platform)
        }
        HookPlatform::Copilot => copilot_hook_installed(document),
    }
}

fn install_cursor_hook(
    document: &mut Value,
    scope: HookScope,
    env: &HookEnvironment,
) -> Result<()> {
    let root = ensure_root_object(document)?;
    root.entry("version".to_string())
        .or_insert_with(|| Value::Number(1.into()));

    let hooks = ensure_child_object(root, "hooks")?;
    let entries = ensure_child_array(hooks, HookPlatform::Cursor.event_key())?;
    entries.retain(|entry| !is_managed_cursor_entry(entry));
    entries.push(json!({
        "matcher": "Shell",
        "command": build_exec_command(HookPlatform::Cursor, scope, env),
        "timeout": HOOK_TIMEOUT_SECS,
        // Fail OPEN: if the ahma hook binary is missing, crashes, or times out,
        // Cursor runs the command anyway (unsandboxed) instead of blocking the
        // user's terminal. ahma's own exec path emits a loud warning in that case.
        "failClosed": false,
    }));

    // Post-execution observer: when the pre-execution hook defers to the host
    // sandbox (R7), the host can still deny a write its pre-hook never sees (the
    // `aws-lc-sys` build-cache copy). This `afterShellExecution` hook scans the
    // finished command's output and surfaces a remediation. Strictly
    // observational and fail-open — it can only add context, never block.
    let observe_entries = ensure_child_array(hooks, CURSOR_OBSERVE_EVENT_KEY)?;
    observe_entries.retain(|entry| !is_managed_cursor_entry(entry));
    observe_entries.push(json!({
        "matcher": "Shell",
        "command": build_observe_command(scope, env),
        "timeout": HOOK_TIMEOUT_SECS,
        "failClosed": false,
    }));

    Ok(())
}

fn uninstall_cursor_hook(document: &mut Value) -> Result<bool> {
    let pre = remove_managed_hook_entries(
        document,
        "Cursor",
        HookPlatform::Cursor.event_key(),
        is_managed_cursor_entry,
    )?;
    let observe = remove_managed_hook_entries(
        document,
        "Cursor",
        CURSOR_OBSERVE_EVENT_KEY,
        is_managed_cursor_entry,
    )?;
    Ok(pre || observe)
}

fn cursor_hook_installed(document: &Value) -> bool {
    document
        .get("hooks")
        .and_then(Value::as_object)
        .and_then(|hooks| hooks.get(HookPlatform::Cursor.event_key()))
        .and_then(Value::as_array)
        .map(|entries| entries.iter().any(is_managed_cursor_entry))
        .unwrap_or(false)
}

fn install_claude_hook(
    document: &mut Value,
    scope: HookScope,
    env: &HookEnvironment,
) -> Result<()> {
    install_grouped_hook(document, HookPlatform::Claude, scope, env)
}

fn install_codex_hook(document: &mut Value, scope: HookScope, env: &HookEnvironment) -> Result<()> {
    install_grouped_hook(document, HookPlatform::Codex, scope, env)
}

fn install_grouped_hook(
    document: &mut Value,
    platform: HookPlatform,
    scope: HookScope,
    env: &HookEnvironment,
) -> Result<()> {
    let root = ensure_root_object(document)?;
    let hooks = ensure_child_object(root, "hooks")?;
    let entries = ensure_child_array(hooks, platform.event_key())?;
    entries.retain(|entry| !is_managed_group_entry(entry));
    entries.push(managed_group_entry(platform, scope, env));
    Ok(())
}

fn uninstall_grouped_hook(document: &mut Value, platform: HookPlatform) -> Result<bool> {
    remove_managed_hook_entries(
        document,
        platform.label(),
        platform.event_key(),
        is_managed_group_entry,
    )
}

fn remove_managed_hook_entries(
    document: &mut Value,
    config_label: &str,
    event_key: &str,
    is_managed: fn(&Value) -> bool,
) -> Result<bool> {
    let Some(root) = document.as_object_mut() else {
        bail!("{config_label} hook config must be a JSON object");
    };
    let Some(hooks) = root.get_mut("hooks") else {
        return Ok(false);
    };
    let Some(hooks_object) = hooks.as_object_mut() else {
        bail!("{config_label} hook config field 'hooks' must be an object");
    };
    let Some(entries) = hooks_object.get_mut(event_key) else {
        return Ok(false);
    };
    let Some(entries_array) = entries.as_array_mut() else {
        bail!("{config_label} hook list must be an array");
    };

    let before_len = entries_array.len();
    entries_array.retain(|entry| !is_managed(entry));
    let changed = entries_array.len() != before_len;
    cleanup_empty_hook_tree(root, event_key);
    Ok(changed)
}

fn grouped_hook_installed(document: &Value, platform: HookPlatform) -> bool {
    document
        .get("hooks")
        .and_then(Value::as_object)
        .and_then(|hooks| hooks.get(platform.event_key()))
        .and_then(Value::as_array)
        .map(|entries| entries.iter().any(is_managed_group_entry))
        .unwrap_or(false)
}

/// Build a grouped `type: command` hook entry whose **command is fully
/// self-contained**: the binary path and every `hooks exec` argument are joined
/// into the single `command` string (with a `commandWindows` variant where
/// relevant), and there is no separate `args` array.
///
/// The command MUST be self-contained. Some consumers execute only the
/// `command` field and ignore a sibling `args` array — notably Cursor, which
/// also imports Claude Code's `~/.claude/settings.json` PreToolUse hooks and
/// runs just their `command`. A split `command` + `args` entry then runs bare
/// `ahma` with no subcommand, which dumps the CLI usage banner and exits
/// non-zero; the editor surfaces that as a hard "Hook blocked with message:
/// <banner>" on every shell command. Codex, Claude and Antigravity share this
/// grouped format, so they all build the command the same self-contained way
/// (differing only in the tool-name `matcher` and the status message).
fn single_command_group_entry(
    platform: HookPlatform,
    scope: HookScope,
    env: &HookEnvironment,
    matcher: &str,
    status_message: &str,
) -> Value {
    let args = exec_args(platform, scope);
    let binary_ref = BinaryReference::for_scope(env, scope);
    let command = binary_ref.build_command(&args);
    let mut handler = Map::new();
    handler.insert("type".to_string(), Value::String("command".to_string()));
    handler.insert("command".to_string(), Value::String(command));
    if matches!(scope, HookScope::Project) || cfg!(target_os = "windows") {
        let windows_command = match binary_ref {
            BinaryReference::PathLookup => std::iter::once(PATH_LOOKUP_BINARY.to_string())
                .chain(args.iter().cloned())
                .collect::<Vec<_>>()
                .join(" "),
            BinaryReference::Absolute(ref path) => build_windows_absolute_command(path, &args),
        };
        handler.insert("commandWindows".to_string(), Value::String(windows_command));
    }
    handler.insert(
        "timeout".to_string(),
        Value::Number(HOOK_TIMEOUT_SECS.into()),
    );
    handler.insert(
        "statusMessage".to_string(),
        Value::String(status_message.to_string()),
    );
    json!({
        "matcher": matcher,
        "hooks": [Value::Object(handler)],
    })
}

fn codex_group_entry(scope: HookScope, env: &HookEnvironment) -> Value {
    single_command_group_entry(
        HookPlatform::Codex,
        scope,
        env,
        "^Bash$",
        "Routing Bash through ahma",
    )
}

fn managed_group_entry(platform: HookPlatform, scope: HookScope, env: &HookEnvironment) -> Value {
    match platform {
        HookPlatform::Claude => {
            single_command_group_entry(platform, scope, env, "Bash", "Routing Bash through ahma")
        }
        HookPlatform::Antigravity => single_command_group_entry(
            platform,
            scope,
            env,
            "run_command",
            "Routing run_command through ahma",
        ),
        HookPlatform::Codex => codex_group_entry(scope, env),
        HookPlatform::Cursor | HookPlatform::Copilot => {
            unreachable!("Cursor/Copilot do not use grouped hooks")
        }
    }
}

fn install_copilot_hook(
    document: &mut Value,
    scope: HookScope,
    env: &HookEnvironment,
) -> Result<()> {
    let root = ensure_root_object(document)?;
    root.entry("version".to_string())
        .or_insert_with(|| Value::Number(1.into()));

    let hooks = ensure_child_object(root, "hooks")?;
    let entries = ensure_child_array(hooks, HookPlatform::Copilot.event_key())?;
    entries.retain(|entry| !is_managed_copilot_entry(entry));

    let args = exec_args(HookPlatform::Copilot, scope);
    let binary_ref = BinaryReference::for_scope(env, scope);

    let bash_command = match &binary_ref {
        BinaryReference::PathLookup => std::iter::once(PATH_LOOKUP_BINARY.to_string())
            .chain(args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" "),
        BinaryReference::Absolute(path) => {
            std::iter::once(shell_quote_posix(&path.to_string_lossy()))
                .chain(args.iter().map(|arg| shell_quote_posix(arg)))
                .collect::<Vec<_>>()
                .join(" ")
        }
    };

    let powershell_command = match &binary_ref {
        BinaryReference::PathLookup => std::iter::once(PATH_LOOKUP_BINARY.to_string())
            .chain(args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" "),
        BinaryReference::Absolute(path) => build_windows_absolute_command(path, &args),
    };

    entries.push(json!({
        "type": "command",
        "bash": bash_command,
        "powershell": powershell_command,
        "timeoutSec": HOOK_TIMEOUT_SECS,
    }));

    Ok(())
}

fn uninstall_copilot_hook(document: &mut Value) -> Result<bool> {
    remove_managed_hook_entries(
        document,
        "GitHub Copilot",
        HookPlatform::Copilot.event_key(),
        is_managed_copilot_entry,
    )
}

fn copilot_hook_installed(document: &Value) -> bool {
    document
        .get("hooks")
        .and_then(Value::as_object)
        .and_then(|hooks| hooks.get(HookPlatform::Copilot.event_key()))
        .and_then(Value::as_array)
        .map(|entries| entries.iter().any(is_managed_copilot_entry))
        .unwrap_or(false)
}

fn is_managed_copilot_entry(entry: &Value) -> bool {
    let Some(object) = entry.as_object() else {
        return false;
    };

    object
        .get("bash")
        .and_then(Value::as_str)
        .map(is_managed_command)
        .unwrap_or(false)
        || object
            .get("powershell")
            .and_then(Value::as_str)
            .map(is_managed_command)
            .unwrap_or(false)
}

fn build_exec_command(platform: HookPlatform, scope: HookScope, env: &HookEnvironment) -> String {
    BinaryReference::for_scope(env, scope).build_command(&exec_args(platform, scope))
}

fn exec_args(platform: HookPlatform, scope: HookScope) -> Vec<String> {
    vec![
        "hooks".to_string(),
        "exec".to_string(),
        "--platform".to_string(),
        platform.cli_name().to_string(),
        "--scope".to_string(),
        scope.cli_name().to_string(),
        "--managed-id".to_string(),
        MANAGED_ID_DEFAULT_SHELL_V1.to_string(),
    ]
}

fn ensure_root_object(document: &mut Value) -> Result<&mut Map<String, Value>> {
    if document.is_null() {
        *document = Value::Object(Map::new());
    }
    document
        .as_object_mut()
        .ok_or_else(|| anyhow!("Hook config must be a JSON object"))
}

fn ensure_child_object<'a>(
    parent: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Map<String, Value>> {
    if !parent.contains_key(key) {
        parent.insert(key.to_string(), Value::Object(Map::new()));
    }

    parent
        .get_mut(key)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow!("Hook config field '{}' must be a JSON object", key))
}

fn ensure_child_array<'a>(
    parent: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Vec<Value>> {
    if !parent.contains_key(key) {
        parent.insert(key.to_string(), Value::Array(Vec::new()));
    }

    parent
        .get_mut(key)
        .and_then(Value::as_array_mut)
        .ok_or_else(|| anyhow!("Hook config field '{}' must be an array", key))
}

fn cleanup_empty_hook_tree(root: &mut Map<String, Value>, event_key: &str) {
    let remove_hooks = match root.get_mut("hooks") {
        Some(Value::Object(hooks)) => {
            let remove_event = hooks
                .get(event_key)
                .and_then(Value::as_array)
                .map(|entries| entries.is_empty())
                .unwrap_or(false);
            if remove_event {
                hooks.remove(event_key);
            }
            hooks.is_empty()
        }
        _ => false,
    };

    if remove_hooks {
        root.remove("hooks");
    }
}

fn is_managed_cursor_entry(entry: &Value) -> bool {
    entry
        .as_object()
        .and_then(|object| object.get("command"))
        .and_then(Value::as_str)
        .map(is_managed_command)
        .unwrap_or(false)
}

fn is_managed_group_entry(entry: &Value) -> bool {
    entry
        .as_object()
        .and_then(|object| object.get("hooks"))
        .and_then(Value::as_array)
        .map(|handlers| handlers.iter().any(is_managed_handler))
        .unwrap_or(false)
}

fn is_managed_handler(handler: &Value) -> bool {
    let Some(object) = handler.as_object() else {
        return false;
    };

    if let Some(args) = object.get("args").and_then(Value::as_array)
        && args
            .iter()
            .any(|value| value.as_str() == Some(MANAGED_ID_DEFAULT_SHELL_V1))
    {
        return true;
    }

    object
        .get("command")
        .and_then(Value::as_str)
        .map(is_managed_command)
        .unwrap_or(false)
        || object
            .get("commandWindows")
            .and_then(Value::as_str)
            .map(is_managed_command)
            .unwrap_or(false)
}

fn is_managed_command(command: &str) -> bool {
    command.contains(MANAGED_ID_DEFAULT_SHELL_V1)
}

fn load_hook_document(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Object(Map::new()));
    }

    let contents = fs::read_to_string(path)
        .with_context(|| format!("Failed to read hook config {}", path.display()))?;
    if contents.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }

    serde_json::from_str(&contents)
        .with_context(|| format!("Failed to parse JSON hook config {}", path.display()))
}

fn write_hook_document(
    path: &Path,
    before: &Value,
    after: &Value,
    dry_run: bool,
    existed: bool,
) -> Result<FileAction> {
    if before == after {
        return Ok(FileAction::Unchanged);
    }

    let action = if existed {
        FileAction::Updated
    } else {
        FileAction::Created
    };

    if dry_run {
        return Ok(action);
    }

    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Invalid hook config path"))?;
    fs::create_dir_all(parent).with_context(|| format!("Failed to create {}", parent.display()))?;

    if existed {
        backup_existing_hook(path)?;
    }

    persist_json_file(path, parent, after)?;

    Ok(action)
}

fn backup_existing_hook(path: &Path) -> Result<()> {
    let backup = backup_path(path)?;
    fs::copy(path, &backup).with_context(|| {
        format!(
            "Failed to create backup {} before updating hook config",
            backup.display()
        )
    })?;
    Ok(())
}

fn persist_json_file(path: &Path, parent: &Path, value: &Value) -> Result<()> {
    let mut temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("Failed to create temp file in {}", parent.display()))?;
    let formatted = serde_json::to_string_pretty(value)?;
    temp.write_all(formatted.as_bytes())?;
    temp.write_all(b"\n")?;
    temp.flush()?;
    temp.persist(path).map_err(|error| {
        anyhow!(
            "Failed to persist hook config {}: {}",
            path.display(),
            error.error
        )
    })?;
    Ok(())
}

fn backup_path(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("Invalid hook config path {}", path.display()))?;
    Ok(path.with_file_name(format!("{}.bak", file_name.to_string_lossy())))
}

fn read_stdin_json() -> Result<Value> {
    let mut buffer = String::new();
    io::stdin()
        .read_to_string(&mut buffer)
        .context("Failed to read hook input from stdin")?;
    if buffer.trim().is_empty() {
        bail!("Hook input on stdin was empty");
    }

    serde_json::from_str(&buffer).context("Failed to parse hook input JSON")
}

fn build_absolute_shell_command(path: &Path, args: &[String]) -> String {
    #[cfg(target_os = "windows")]
    {
        build_windows_absolute_command(path, args)
    }

    #[cfg(not(target_os = "windows"))]
    {
        std::iter::once(shell_quote_posix(&path.to_string_lossy()))
            .chain(args.iter().map(|arg| shell_quote_posix(arg)))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn build_windows_absolute_command(path: &Path, args: &[String]) -> String {
    let command = std::iter::once(powershell_quote(&path.to_string_lossy()))
        .chain(args.iter().map(|arg| powershell_quote(arg)))
        .collect::<Vec<_>>()
        .join(" ");
    let script = format!("& {{ & {} }}", command);
    format!(r#"powershell -NoProfile -Command "{}""#, script)
}

fn shell_quote_posix(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'\''"#))
}

fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_env() -> HookEnvironment {
        let temp = tempdir().expect("tempdir");
        let root = temp.keep();
        HookEnvironment {
            home_dir: root.join("home"),
            project_root: root.join("repo"),
            current_exe: root.join("bin space").join(if cfg!(windows) {
                "ahma.exe"
            } else {
                "ahma"
            }),
        }
    }

    #[test]
    fn test_cursor_install_writes_managed_command() {
        let env = test_env();
        let path = env.config_path(HookPlatform::Cursor, HookScope::User);
        let mut document = Value::Object(Map::new());

        install_platform_hook(&mut document, HookPlatform::Cursor, HookScope::User, &env).unwrap();
        write_hook_document(&path, &Value::Object(Map::new()), &document, false, false).unwrap();

        let installed = load_hook_document(&path).unwrap();
        assert!(platform_hook_installed(&installed, HookPlatform::Cursor));
        let entry = &installed["hooks"]["preToolUse"][0];
        let command = entry["command"].as_str().unwrap();
        assert!(command.contains(MANAGED_ID_DEFAULT_SHELL_V1));
        assert!(command.contains("bin space"));
        assert!(
            !entry["failClosed"].as_bool().unwrap_or(true),
            "Cursor hook must have failClosed:false so a binary crash/timeout fails OPEN (runs unsandboxed) rather than blocking the user's terminal"
        );
    }

    #[test]
    fn test_cursor_install_writes_observe_hook() {
        let env = test_env();
        let mut document = Value::Object(Map::new());

        install_cursor_hook(&mut document, HookScope::User, &env).unwrap();

        let observe = &document["hooks"][CURSOR_OBSERVE_EVENT_KEY][0];
        let command = observe["command"].as_str().unwrap();
        // Tokens are shell-quoted individually, so assert on the distinctive
        // `observe` subcommand token and the managed id rather than a contiguous
        // `hooks observe` (which quoting splits into `'hooks' 'observe'`).
        assert!(
            command.contains("observe"),
            "observe entry must invoke the `observe` subcommand: {command}"
        );
        assert!(command.contains(MANAGED_ID_DEFAULT_SHELL_V1));
        assert_eq!(observe["matcher"].as_str().unwrap(), "Shell");
        assert!(
            !observe["failClosed"].as_bool().unwrap_or(true),
            "observe hook must fail open — it is purely informational and must never block"
        );
    }

    #[test]
    fn test_cursor_install_is_idempotent_for_observe() {
        let env = test_env();
        let mut document = Value::Object(Map::new());

        install_cursor_hook(&mut document, HookScope::User, &env).unwrap();
        install_cursor_hook(&mut document, HookScope::User, &env).unwrap();

        let pre = document["hooks"]["preToolUse"].as_array().unwrap();
        let observe = document["hooks"][CURSOR_OBSERVE_EVENT_KEY]
            .as_array()
            .unwrap();
        assert_eq!(
            pre.len(),
            1,
            "re-install must not duplicate the preToolUse entry"
        );
        assert_eq!(
            observe.len(),
            1,
            "re-install must not duplicate the observe entry"
        );
    }

    #[test]
    fn test_cursor_uninstall_removes_both_pre_and_observe() {
        let env = test_env();
        let mut document = Value::Object(Map::new());
        install_cursor_hook(&mut document, HookScope::User, &env).unwrap();

        let changed = uninstall_cursor_hook(&mut document).unwrap();
        assert!(changed, "uninstall must report it removed managed entries");
        assert!(
            !cursor_hook_installed(&document),
            "no managed preToolUse entry should remain"
        );
        // The whole hooks tree should be cleaned up since nothing else was there.
        assert!(
            document.get("hooks").is_none()
                || document["hooks"].get(CURSOR_OBSERVE_EVENT_KEY).is_none(),
            "observe entry must be removed on uninstall: {document}"
        );
    }

    #[test]
    fn test_extract_command_output_concatenates_known_keys() {
        let input = json!({
            "command": "cargo build",
            "stdout": "Compiling foo",
            "stderr": "Operation not permitted",
        });
        let out = extract_command_output(&input);
        assert!(out.contains("Compiling foo"));
        assert!(out.contains("Operation not permitted"));
    }

    #[test]
    fn test_extract_command_output_reads_nested_tool_output() {
        let input = json!({ "tool_output": { "output": "nested denial text" } });
        assert_eq!(extract_command_output(&input), "nested denial text");
    }

    #[test]
    fn test_extract_command_output_empty_when_no_known_keys() {
        let input = json!({ "command": "ls", "unrelated": 5 });
        assert!(extract_command_output(&input).is_empty());
    }

    #[test]
    fn test_command_failed_reads_exit_code() {
        assert!(command_failed(&json!({ "exit_code": 1 })));
        assert!(!command_failed(&json!({ "exit_code": 0 })));
        assert!(command_failed(&json!({ "exitCode": 137 })));
        assert!(command_failed(&json!({ "aborted": true })));
        assert!(!command_failed(&json!({ "aborted": false })));
        // Unknown status → treated as failed so a denial signature is not suppressed.
        assert!(command_failed(&json!({ "command": "x" })));
    }

    #[test]
    fn test_build_cursor_observe_output_carries_remediation() {
        let out = build_cursor_observe_output("do the fix");
        assert_eq!(out["additional_context"].as_str().unwrap(), "do the fix");
        assert_eq!(out["agent_message"].as_str().unwrap(), "do the fix");
        assert_eq!(out["user_message"].as_str().unwrap(), "do the fix");
    }

    #[test]
    fn test_observe_args_shape() {
        let args = observe_args(HookScope::User);
        assert_eq!(args[0], "hooks");
        assert_eq!(args[1], "observe");
        assert!(args.iter().any(|a| a == "--platform"));
        assert!(args.iter().any(|a| a == MANAGED_ID_DEFAULT_SHELL_V1));
    }

    #[test]
    fn test_codex_project_install_uses_path_lookup() {
        let env = test_env();
        let mut document = Value::Object(Map::new());

        install_platform_hook(&mut document, HookPlatform::Codex, HookScope::Project, &env)
            .unwrap();

        let command = document["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(command.starts_with("ahma hooks exec"));
        assert!(!command.contains("bin space"));
    }

    #[test]
    fn test_uninstall_preserves_non_managed_groups() {
        let mut document = json!({
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "^Bash$",
                        "hooks": [
                            {
                                "type": "command",
                                "command": "echo keep me"
                            }
                        ]
                    },
                    {
                        "matcher": "^Bash$",
                        "hooks": [
                            {
                                "type": "command",
                                "command": "ahma hooks exec --platform codex --scope user --managed-id ahma-default-shell-v1"
                            }
                        ]
                    }
                ]
            }
        });

        let changed = uninstall_platform_hook(&mut document, HookPlatform::Codex).unwrap();
        assert!(changed);
        assert_eq!(document["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert_eq!(
            document["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap(),
            "echo keep me"
        );
    }

    #[test]
    fn test_exec_response_rewrites_command_and_preserves_fields() {
        let env = test_env();
        let input = json!({
            "cwd": "/tmp/project",
            "tool_input": {
                "command": "cargo test",
                "description": "Run tests"
            }
        });

        let decision =
            compute_exec_decision_internal(&input, HookScope::Project, &env, true, false, None);
        let output = build_exec_output(decision, HookPlatform::Claude);
        let updated = &output["hookSpecificOutput"]["updatedInput"];
        let command = updated["command"].as_str().unwrap();
        assert!(command.starts_with("ahma hooks run-shell"));
        assert!(command.contains(WRAPPED_BY_MARKER));
        assert_eq!(updated["description"].as_str(), Some("Run tests"));
    }

    #[test]
    fn test_exec_response_rewrites_command_line_and_preserves_fields() {
        let env = test_env();
        let input = json!({
            "cwd": "/tmp/project",
            "tool_input": {
                "CommandLine": "cargo test --bin ahma",
                "description": "Run specific test binary"
            }
        });

        let decision =
            compute_exec_decision_internal(&input, HookScope::Project, &env, true, false, None);
        let output = build_exec_output(decision, HookPlatform::Antigravity);
        let updated = &output["hookSpecificOutput"]["updatedInput"];
        let command = updated["CommandLine"].as_str().unwrap();
        assert!(command.starts_with("ahma hooks run-shell"));
        assert!(command.contains(WRAPPED_BY_MARKER));
        assert_eq!(
            updated["description"].as_str(),
            Some("Run specific test binary")
        );
    }

    #[test]
    fn test_exec_response_allows_non_shell_tools_without_warning() {
        // Non-shell tools (editFiles, createFile, etc.) have no `command` field.
        // The hook must exit 0 and return an allow response so VS Code does not
        // show "warning from pre tool use hook".
        let env = test_env();

        for (tool_name, tool_input) in [
            ("editFiles", json!({"files": [{"path": "src/main.rs"}]})),
            ("createFile", json!({"path": "src/new.rs", "content": ""})),
            ("readFile", json!({"path": "src/main.rs"})),
        ] {
            let input = json!({
                "tool_name": tool_name,
                "tool_input": tool_input,
                "cwd": "/tmp/project",
            });
            // active=true: even when ahma is on, non-shell tools must pass through
            let decision =
                compute_exec_decision_internal(&input, HookScope::User, &env, true, false, None);
            let output = build_exec_output(decision, HookPlatform::Copilot);
            // Must return allow with no input modification
            assert_eq!(
                output["hookSpecificOutput"]["permissionDecision"].as_str(),
                Some("allow"),
                "{tool_name} should be allowed"
            );
            assert!(
                output["hookSpecificOutput"]["updatedInput"].is_null(),
                "{tool_name} should not have updatedInput"
            );
        }

        // Also check: completely missing tool_input field
        let input_no_args = json!({"tool_name": "unknown", "cwd": "/tmp"});
        let decision = compute_exec_decision_internal(
            &input_no_args,
            HookScope::User,
            &env,
            true,
            false,
            None,
        );
        let output = build_exec_output(decision, HookPlatform::Copilot);
        assert_eq!(
            output["hookSpecificOutput"]["permissionDecision"].as_str(),
            Some("allow")
        );
    }

    #[test]
    fn test_wrapped_payload_round_trips() {
        let payload = WrappedShellPayload {
            cwd: "/tmp/work".to_string(),
            command: "echo hello && cargo test".to_string(),
        };
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        let decoded = decode_wrapped_shell_payload(&encoded).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn test_windows_absolute_command_uses_plain_command_quotes() {
        let command = build_windows_absolute_command(
            Path::new(r"C:\Users\Test User\ahma.exe"),
            &["hooks".to_string(), "exec".to_string()],
        );
        assert!(command.contains("powershell -NoProfile -Command \"& { & "));
        assert!(!command.contains(r#"\"& { &"#));
    }

    #[test]
    fn test_any_managed_hooks_installed_in_env_detects_user_hook() {
        let env = test_env();
        fs::create_dir_all(env.home_dir.join(".cursor")).unwrap();

        let path = env.config_path(HookPlatform::Cursor, HookScope::User);
        let mut document = Value::Object(Map::new());
        install_platform_hook(&mut document, HookPlatform::Cursor, HookScope::User, &env).unwrap();
        write_hook_document(&path, &Value::Object(Map::new()), &document, false, false).unwrap();

        assert!(any_managed_hooks_installed_in_env(HookScope::User, &env).unwrap());
        assert!(!any_managed_hooks_installed_in_env(HookScope::Project, &env).unwrap());
    }

    #[test]
    fn test_copilot_install_writes_bash_and_powershell() {
        let env = test_env();
        let path = env.config_path(HookPlatform::Copilot, HookScope::User);
        let mut document = Value::Object(Map::new());

        install_platform_hook(&mut document, HookPlatform::Copilot, HookScope::User, &env).unwrap();
        write_hook_document(&path, &Value::Object(Map::new()), &document, false, false).unwrap();

        let installed = load_hook_document(&path).unwrap();
        assert!(platform_hook_installed(&installed, HookPlatform::Copilot));

        let entry = &installed["hooks"]["preToolUse"][0];
        let bash = entry["bash"].as_str().unwrap();
        let powershell = entry["powershell"].as_str().unwrap();
        assert!(bash.contains(MANAGED_ID_DEFAULT_SHELL_V1));
        assert!(powershell.contains(MANAGED_ID_DEFAULT_SHELL_V1));
    }

    #[test]
    fn test_copilot_uninstall_removes_managed_entries() {
        let _env = test_env();
        let mut document = json!({
            "version": 1,
            "hooks": {
                "preToolUse": [
                    {
                        "type": "command",
                        "bash": "echo non-managed",
                        "powershell": "echo non-managed",
                        "timeoutSec": 30
                    },
                    {
                        "type": "command",
                        "bash": "ahma hooks exec --platform copilot --scope user --managed-id ahma-default-shell-v1",
                        "powershell": "powershell -Command ... ahma-default-shell-v1",
                        "timeoutSec": 30
                    }
                ]
            }
        });

        let changed = uninstall_platform_hook(&mut document, HookPlatform::Copilot).unwrap();
        assert!(changed);
        let entries = document["hooks"]["preToolUse"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["bash"].as_str().unwrap(), "echo non-managed");
    }

    #[test]
    fn test_antigravity_config_path() {
        let env = test_env();
        let user_path = HookPlatform::Antigravity.config_path(&env.home_dir, HookScope::User);
        assert_eq!(
            user_path,
            env.home_dir
                .join(".gemini")
                .join("config")
                .join("hooks.json")
        );

        let project_path =
            HookPlatform::Antigravity.config_path(&env.project_root, HookScope::Project);
        assert_eq!(
            project_path,
            env.project_root.join(".agents").join("hooks.json")
        );
    }

    #[test]
    fn test_antigravity_install_writes_grouped_hook() {
        let env = test_env();
        let path = env.config_path(HookPlatform::Antigravity, HookScope::User);
        let mut document = Value::Object(Map::new());

        install_platform_hook(
            &mut document,
            HookPlatform::Antigravity,
            HookScope::User,
            &env,
        )
        .unwrap();
        write_hook_document(&path, &Value::Object(Map::new()), &document, false, false).unwrap();

        let installed = load_hook_document(&path).unwrap();
        assert!(platform_hook_installed(
            &installed,
            HookPlatform::Antigravity
        ));
        let matcher = installed["hooks"]["PreToolUse"][0]["matcher"]
            .as_str()
            .unwrap();
        assert_eq!(matcher, "run_command");

        let handler = &installed["hooks"]["PreToolUse"][0]["hooks"][0];
        let command = handler["command"].as_str().unwrap();
        assert!(command.contains("bin space"));
        // The command must be fully self-contained (binary + `hooks exec` args
        // in one string) so a consumer that runs only `command` — e.g. Cursor
        // importing Claude/Antigravity hooks — still gets a complete invocation
        // rather than bare `ahma` (which dumps the usage banner and exits 2).
        assert!(command.contains("hooks"));
        assert!(command.contains("exec"));
        assert!(command.contains(MANAGED_ID_DEFAULT_SHELL_V1));
        assert!(
            handler.get("args").is_none(),
            "grouped hook must NOT use a separate `args` array: {handler:?}",
        );
    }

    #[test]
    fn test_exec_passthrough_when_ahma_hooks_off() {
        let env = test_env();
        let input = json!({
            "cwd": "/tmp/project",
            "tool_input": { "command": "rm -rf /" }
        });
        // active=false → must return allow-unchanged regardless of command
        let decision =
            compute_exec_decision_internal(&input, HookScope::User, &env, false, false, None);
        let output = build_exec_output(decision, HookPlatform::Cursor);
        assert_eq!(output["permission"].as_str(), Some("allow"));
        assert!(
            output.get("updated_input").is_none(),
            "passthrough must not rewrite the command"
        );
    }

    #[test]
    fn test_exec_deny_when_active_but_already_wrapped_passes_through() {
        // Already-wrapped commands must never be double-wrapped, even when active
        let env = test_env();
        let already_wrapped =
            format!("ahma hooks run-shell --payload-base64 abc --wrapped-by {WRAPPED_BY_MARKER}");
        let input = json!({
            "cwd": "/tmp/project",
            "tool_input": { "command": already_wrapped }
        });
        let decision =
            compute_exec_decision_internal(&input, HookScope::User, &env, true, false, None);
        let output = build_exec_output(decision, HookPlatform::Cursor);
        assert_eq!(output["permission"].as_str(), Some("allow"));
        assert!(output.get("updated_input").is_none());
    }

    #[test]
    fn test_exec_cursor_rewrite_when_active() {
        let env = test_env();
        let input = json!({
            "cwd": "/tmp/project",
            "tool_input": { "command": "cargo build" }
        });
        let decision =
            compute_exec_decision_internal(&input, HookScope::User, &env, true, false, None);
        let output = build_exec_output(decision, HookPlatform::Cursor);
        assert_eq!(output["permission"].as_str(), Some("allow"));
        let updated = output["updated_input"]["command"].as_str().unwrap();
        assert!(updated.contains(WRAPPED_BY_MARKER));
    }

    #[test]
    fn test_unsandboxable_without_consent_denies() {
        // SPEC R5.5.3: with no consent, an un-sandboxable command FAILS CLOSED.
        match unsandboxable_decision("test reason", false) {
            HooksDecision::DenyPendingConsent {
                user_message,
                agent_message,
            } => {
                assert!(user_message.contains("test reason"));
                assert!(user_message.contains("BLOCKED"));
                assert!(user_message.contains("approve-unsandboxed"));
                assert!(agent_message.contains("did NOT run"));
            }
            other => panic!("expected DenyPendingConsent, got {other:?}"),
        }
    }

    #[test]
    fn test_unsandboxable_with_consent_allows_with_warning() {
        // SPEC R5.5.3: after consent, it runs unsandboxed with a loud warning.
        match unsandboxable_decision("test reason", true) {
            HooksDecision::AllowWithWarning {
                user_message,
                agent_message,
            } => {
                assert!(user_message.contains("test reason"));
                assert!(user_message.contains("UNSANDBOXED"));
                assert!(agent_message.contains("WITHOUT"));
            }
            other => panic!("expected AllowWithWarning, got {other:?}"),
        }
    }

    #[test]
    fn test_exec_cursor_fail_closed_denies() {
        // Cursor: with no consent, permission is "deny" (fail closed, R5.5.3).
        let output = build_exec_output(unsandboxable_decision("boom", false), HookPlatform::Cursor);
        assert_eq!(
            output["permission"].as_str(),
            Some("deny"),
            "no-consent fall-open must deny the command (R5.5.3)"
        );
        assert!(output["user_message"].as_str().unwrap().contains("boom"));
    }

    #[test]
    fn test_exec_cursor_consented_allows_with_warning() {
        let output = build_exec_output(unsandboxable_decision("boom", true), HookPlatform::Cursor);
        assert_eq!(output["permission"].as_str(), Some("allow"));
        assert!(output["user_message"].as_str().unwrap().contains("boom"));
    }

    #[test]
    fn test_exec_structured_fail_closed_denies() {
        let output = build_exec_output(unsandboxable_decision("boom", false), HookPlatform::Claude);
        let hs = &output["hookSpecificOutput"];
        assert_eq!(
            hs["permissionDecision"].as_str(),
            Some("deny"),
            "no-consent fall-open must deny the command (R5.5.3)"
        );
        assert!(hs["agentMessage"].as_str().unwrap().contains("did NOT run"));
        assert!(hs["systemMessage"].as_str().unwrap().contains("boom"));
    }

    #[test]
    fn test_is_ahma_hooks_active_with_configs_empty_returns_false() {
        // Neutralise ambient AHMA_HOOKS (e.g. from a developer shell running
        // the suite with AHMA_HOOKS=off) — nextest gives each test its own
        // process, so env mutation here cannot race other tests.
        unsafe { std::env::remove_var("AHMA_HOOKS") };
        // auto mode with no MCP configs → inactive (passthrough)
        assert!(!is_ahma_hooks_active_with_configs(&[]));
    }

    #[test]
    fn test_is_ahma_hooks_active_with_configs_nonempty_returns_true() {
        unsafe { std::env::remove_var("AHMA_HOOKS") };
        let fake_path = std::path::PathBuf::from("/fake/mcp.json");
        assert!(is_ahma_hooks_active_with_configs(&[fake_path]));
    }

    #[test]
    fn test_detect_active_mcp_configs_case_insensitive() {
        // Write a temp mcp.json using capital-A "Ahma" (matching the user's real config)
        let temp = tempdir().unwrap();
        let home = temp.path();
        let cursor_dir = home.join(".cursor");
        fs::create_dir_all(&cursor_dir).unwrap();
        fs::write(
            cursor_dir.join("mcp.json"),
            r#"{"mcpServers":{"Ahma":{"command":"ahma","args":["serve","stdio"]}}}"#,
        )
        .unwrap();

        let found = detect_active_mcp_configs_in(home, None);
        assert_eq!(found, vec![cursor_dir.join("mcp.json")]);
    }

    #[test]
    fn test_detect_active_mcp_configs_finds_claude_code_dotfile() {
        // Regression: `ahma setup` writes the Claude Code MCP server to
        // `~/.claude.json`. The previous detector only inspected the Claude
        // *Desktop* config, so an installed Claude Code hook was silently inert
        // in `auto` mode. Detection must cover `~/.claude.json`.
        unsafe { std::env::remove_var("AHMA_HOOKS") };
        let temp = tempdir().unwrap();
        let home = temp.path();
        fs::write(
            home.join(".claude.json"),
            r#"{"mcpServers":{"Ahma":{"command":"ahma"}}}"#,
        )
        .unwrap();

        let found = detect_active_mcp_configs_in(home, None);
        assert_eq!(found, vec![home.join(".claude.json")]);
        assert!(
            is_ahma_hooks_active_with_configs(&found),
            "a Claude Code ahma MCP server must make auto-mode hooks active"
        );
    }

    #[test]
    fn test_detect_active_mcp_configs_finds_codex_toml() {
        // Codex stores the server in TOML as `[mcp_servers.Ahma]` (no quotes), so a
        // quoted `"ahma"` match would miss it. The lenient substring match must find it.
        let temp = tempdir().unwrap();
        let home = temp.path();
        let codex_dir = home.join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(
            codex_dir.join("config.toml"),
            "[mcp_servers.Ahma]\ncommand = \"ahma\"\n",
        )
        .unwrap();

        let found = detect_active_mcp_configs_in(home, None);
        assert_eq!(found, vec![codex_dir.join("config.toml")]);
    }

    #[test]
    fn test_detect_active_mcp_configs_finds_antigravity() {
        let temp = tempdir().unwrap();
        let home = temp.path();
        let cfg_dir = home.join(".gemini").join("config");
        fs::create_dir_all(&cfg_dir).unwrap();
        fs::write(
            cfg_dir.join("mcp_config.json"),
            r#"{"mcpServers":{"Ahma":{}}}"#,
        )
        .unwrap();

        let found = detect_active_mcp_configs_in(home, None);
        assert_eq!(found, vec![cfg_dir.join("mcp_config.json")]);
    }

    #[test]
    fn test_detect_active_mcp_configs_finds_lmstudio() {
        // `ahma setup` writes the LM Studio MCP server to `~/.lmstudio/mcp.json`.
        // It must be in the candidate list so auto-mode hooks activate when LM
        // Studio is the only configured client.
        let temp = tempdir().unwrap();
        let home = temp.path();
        let cfg_dir = home.join(".lmstudio");
        fs::create_dir_all(&cfg_dir).unwrap();
        fs::write(cfg_dir.join("mcp.json"), r#"{"mcpServers":{"Ahma":{}}}"#).unwrap();

        let found = detect_active_mcp_configs_in(home, None);
        assert_eq!(found, vec![cfg_dir.join("mcp.json")]);
    }

    #[test]
    fn test_detect_active_mcp_configs_empty_when_no_ahma() {
        let temp = tempdir().unwrap();
        let home = temp.path();
        fs::write(home.join(".claude.json"), r#"{"mcpServers":{"other":{}}}"#).unwrap();
        assert!(detect_active_mcp_configs_in(home, None).is_empty());
    }

    #[test]
    fn test_describe_activation_reports_inactive_reason() {
        unsafe { std::env::remove_var("AHMA_HOOKS") };
        unsafe { std::env::remove_var("AHMA_DISABLE_HOOKS") };
        let (active, reason) = describe_activation(&[]);
        assert!(!active);
        assert!(reason.contains("auto"));

        let (active, reason) = describe_activation(&[PathBuf::from("/x/.claude.json")]);
        assert!(active);
        assert!(reason.contains("auto"));
    }

    #[test]
    fn test_run_install_now_includes_cursor() {
        // Verify that the default platform list includes Cursor
        let all = HookPlatform::all();
        assert!(
            all.contains(&HookPlatform::Cursor),
            "Cursor must be in the default install set"
        );
    }

    // ----------------------------------------------------------------------
    // Env-var serialization guard. `AHMA_HOOKS` / `AHMA_DISABLE_HOOKS` are
    // process-global; serialize the tests that mutate them and restore after.
    // ----------------------------------------------------------------------
    static ENV_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

    struct EnvGuard {
        hooks: Option<String>,
        disable: Option<String>,
    }

    impl EnvGuard {
        fn capture() -> Self {
            Self {
                hooks: std::env::var("AHMA_HOOKS").ok(),
                disable: std::env::var("AHMA_DISABLE_HOOKS").ok(),
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.hooks {
                    Some(v) => std::env::set_var("AHMA_HOOKS", v),
                    None => std::env::remove_var("AHMA_HOOKS"),
                }
                match &self.disable {
                    Some(v) => std::env::set_var("AHMA_DISABLE_HOOKS", v),
                    None => std::env::remove_var("AHMA_DISABLE_HOOKS"),
                }
            }
        }
    }

    // ----------------------------------------------------------------------
    // HookPlatform / HookScope metadata
    // ----------------------------------------------------------------------
    #[test]
    fn test_hook_platform_labels_and_cli_names() {
        for (platform, label, cli) in [
            (HookPlatform::Cursor, "Cursor", "cursor"),
            (HookPlatform::Claude, "Claude Code", "claude"),
            (HookPlatform::Codex, "Codex", "codex"),
            (HookPlatform::Copilot, "GitHub Copilot CLI", "copilot"),
            (HookPlatform::Antigravity, "Antigravity", "antigravity"),
        ] {
            assert_eq!(platform.label(), label);
            assert_eq!(platform.cli_name(), cli);
        }
    }

    #[test]
    fn test_hook_platform_event_keys() {
        assert_eq!(HookPlatform::Cursor.event_key(), "preToolUse");
        assert_eq!(HookPlatform::Copilot.event_key(), "preToolUse");
        assert_eq!(HookPlatform::Claude.event_key(), "PreToolUse");
        assert_eq!(HookPlatform::Codex.event_key(), "PreToolUse");
        assert_eq!(HookPlatform::Antigravity.event_key(), "PreToolUse");
    }

    #[test]
    fn test_hook_platform_config_relative_paths() {
        assert_eq!(
            HookPlatform::Cursor.config_relative_path(HookScope::User),
            PathBuf::from(".cursor/hooks.json")
        );
        assert_eq!(
            HookPlatform::Claude.config_relative_path(HookScope::Project),
            PathBuf::from(".claude/settings.json")
        );
        assert_eq!(
            HookPlatform::Codex.config_relative_path(HookScope::User),
            PathBuf::from(".codex/hooks.json")
        );
        // Copilot differs by scope
        assert_eq!(
            HookPlatform::Copilot.config_relative_path(HookScope::User),
            PathBuf::from(".copilot/hooks/ahma.json")
        );
        assert_eq!(
            HookPlatform::Copilot.config_relative_path(HookScope::Project),
            PathBuf::from(".github/hooks/ahma.json")
        );
        // Antigravity differs by scope
        assert_eq!(
            HookPlatform::Antigravity.config_relative_path(HookScope::User),
            PathBuf::from(".gemini/config/hooks.json")
        );
        assert_eq!(
            HookPlatform::Antigravity.config_relative_path(HookScope::Project),
            PathBuf::from(".agents/hooks.json")
        );
    }

    #[test]
    fn test_hook_scope_labels_and_cli_names() {
        assert_eq!(HookScope::User.label(), "user");
        assert_eq!(HookScope::Project.label(), "project");
        assert_eq!(HookScope::User.cli_name(), "user");
        assert_eq!(HookScope::Project.cli_name(), "project");
    }

    #[test]
    fn test_hook_environment_scope_root_and_config_path() {
        let env = test_env();
        assert_eq!(env.scope_root(HookScope::User), env.home_dir.as_path());
        assert_eq!(
            env.scope_root(HookScope::Project),
            env.project_root.as_path()
        );
        assert_eq!(
            env.config_path(HookPlatform::Cursor, HookScope::User),
            env.home_dir.join(".cursor").join("hooks.json")
        );
        assert_eq!(
            env.config_path(HookPlatform::Claude, HookScope::Project),
            env.project_root.join(".claude").join("settings.json")
        );
    }

    // ----------------------------------------------------------------------
    // BinaryReference
    // ----------------------------------------------------------------------
    #[test]
    fn test_binary_reference_for_scope_user_is_absolute() {
        let env = test_env();
        match BinaryReference::for_scope(&env, HookScope::User) {
            BinaryReference::Absolute(p) => assert_eq!(p, env.current_exe),
            other => panic!("expected Absolute, got {other:?}"),
        }
    }

    #[test]
    fn test_binary_reference_for_scope_project_is_path_lookup() {
        let env = test_env();
        assert!(matches!(
            BinaryReference::for_scope(&env, HookScope::Project),
            BinaryReference::PathLookup
        ));
    }

    #[test]
    fn test_binary_reference_build_command_path_lookup() {
        let cmd =
            BinaryReference::PathLookup.build_command(&["hooks".to_string(), "exec".to_string()]);
        assert_eq!(cmd, "ahma hooks exec");
    }

    #[test]
    fn test_binary_reference_build_command_absolute_quotes_spaces() {
        let env = test_env();
        let cmd =
            BinaryReference::for_scope(&env, HookScope::User).build_command(&["hooks".to_string()]);
        // Contains the binary path with the "bin space" directory, quoted.
        assert!(cmd.contains("bin space"));
        assert!(cmd.contains("hooks"));
    }

    // ----------------------------------------------------------------------
    // action_message & selected_platforms
    // ----------------------------------------------------------------------
    #[test]
    fn test_action_message_all_combinations() {
        assert_eq!(
            action_message(FileAction::Created, true),
            "would be created"
        );
        assert_eq!(
            action_message(FileAction::Updated, true),
            "would be updated"
        );
        assert_eq!(
            action_message(FileAction::Unchanged, true),
            "already matches"
        );
        assert_eq!(action_message(FileAction::Created, false), "installed");
        assert_eq!(action_message(FileAction::Updated, false), "updated");
        assert_eq!(
            action_message(FileAction::Unchanged, false),
            "already matches"
        );
    }

    #[test]
    fn test_selected_platforms_empty_returns_all() {
        assert_eq!(selected_platforms(&[]), HookPlatform::all());
    }

    #[test]
    fn test_selected_platforms_nonempty_returns_requested() {
        let requested = vec![HookPlatform::Claude, HookPlatform::Codex];
        assert_eq!(selected_platforms(&requested), requested);
    }

    // ----------------------------------------------------------------------
    // extract_tool_args
    // ----------------------------------------------------------------------
    #[test]
    fn test_extract_tool_args_no_field_returns_none() {
        let input = json!({"tool_name": "foo"});
        assert!(extract_tool_args(&input).unwrap().is_none());
    }

    #[test]
    fn test_extract_tool_args_object_command() {
        let input = json!({"tool_input": {"command": "ls -la"}});
        let extracted = extract_tool_args(&input).unwrap().unwrap();
        assert_eq!(extracted.command, "ls -la");
        assert_eq!(extracted.arg_key, "command");
        assert_eq!(
            extracted.tool_input.get("command").unwrap().as_str(),
            Some("ls -la")
        );
    }

    #[test]
    fn test_extract_tool_args_command_line_key() {
        let input = json!({"tool_input": {"CommandLine": "dir"}});
        let extracted = extract_tool_args(&input).unwrap().unwrap();
        assert_eq!(extracted.command, "dir");
        assert_eq!(extracted.arg_key, "CommandLine");
    }

    #[test]
    fn test_extract_tool_args_tool_args_alias_object() {
        // `toolArgs` is the alternate top-level key.
        let input = json!({"toolArgs": {"command": "make"}});
        let extracted = extract_tool_args(&input).unwrap().unwrap();
        assert_eq!(extracted.command, "make");
    }

    #[test]
    fn test_extract_tool_args_string_payload_parsed() {
        // `toolArgs` provided as a JSON *string* must be parsed.
        let input = json!({"toolArgs": "{\"command\": \"echo hi\"}"});
        let extracted = extract_tool_args(&input).unwrap().unwrap();
        assert_eq!(extracted.command, "echo hi");
    }

    #[test]
    fn test_extract_tool_args_string_payload_invalid_json_errors() {
        let input = json!({"toolArgs": "{not valid json"});
        let err = extract_tool_args(&input).unwrap_err();
        assert!(err.to_string().contains("toolArgs"));
    }

    #[test]
    fn test_extract_tool_args_non_object_errors() {
        // tool_input that is an array (not an object) is rejected.
        let input = json!({"tool_input": [1, 2, 3]});
        let err = extract_tool_args(&input).unwrap_err();
        assert!(err.to_string().contains("must be a JSON object"));
    }

    #[test]
    fn test_extract_tool_args_no_command_field_returns_none() {
        // Object present but no command/CommandLine → not a shell tool.
        let input = json!({"tool_input": {"path": "src/main.rs"}});
        assert!(extract_tool_args(&input).unwrap().is_none());
    }

    // ----------------------------------------------------------------------
    // extract_command_cwd
    // ----------------------------------------------------------------------
    #[test]
    fn test_extract_command_cwd_prefers_working_directory() {
        let mut tool_input = Map::new();
        tool_input.insert(
            "working_directory".to_string(),
            Value::String("/work/here".to_string()),
        );
        let input = json!({"cwd": "/other"});
        assert_eq!(
            extract_command_cwd(&input, &tool_input).unwrap(),
            "/work/here"
        );
    }

    #[test]
    fn test_extract_command_cwd_falls_back_to_input_cwd() {
        let tool_input = Map::new();
        let input = json!({"cwd": "/top/level"});
        assert_eq!(
            extract_command_cwd(&input, &tool_input).unwrap(),
            "/top/level"
        );
    }

    #[test]
    fn test_extract_command_cwd_falls_back_to_current_dir() {
        let tool_input = Map::new();
        let input = json!({});
        let got = extract_command_cwd(&input, &tool_input).unwrap();
        let expected = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(got, expected);
    }

    // ----------------------------------------------------------------------
    // updated_tool_input
    // ----------------------------------------------------------------------
    #[test]
    fn test_updated_tool_input_replaces_key_preserves_others() {
        let mut tool_input = Map::new();
        tool_input.insert("command".to_string(), Value::String("old".to_string()));
        tool_input.insert("description".to_string(), Value::String("desc".to_string()));
        let updated = updated_tool_input(&tool_input, "new".to_string(), "command");
        assert_eq!(updated["command"].as_str(), Some("new"));
        assert_eq!(updated["description"].as_str(), Some("desc"));
    }

    // ----------------------------------------------------------------------
    // build_cursor_hook_output (all decision arms)
    // ----------------------------------------------------------------------
    #[test]
    fn test_build_cursor_hook_output_allow_unchanged() {
        let out = build_cursor_hook_output(HooksDecision::AllowUnchanged);
        assert_eq!(out["permission"].as_str(), Some("allow"));
        assert!(out.get("updated_input").is_none());
    }

    #[test]
    fn test_build_cursor_hook_output_allow_rewrite() {
        let updated = json!({"command": "wrapped"});
        let out = build_cursor_hook_output(HooksDecision::AllowRewrite(updated));
        assert_eq!(out["permission"].as_str(), Some("allow"));
        assert_eq!(out["updated_input"]["command"].as_str(), Some("wrapped"));
    }

    #[test]
    fn test_build_cursor_hook_output_allow_with_warning() {
        let out = build_cursor_hook_output(HooksDecision::AllowWithWarning {
            user_message: "u".to_string(),
            agent_message: "a".to_string(),
        });
        assert_eq!(out["permission"].as_str(), Some("allow"));
        assert_eq!(out["user_message"].as_str(), Some("u"));
        assert_eq!(out["agent_message"].as_str(), Some("a"));
    }

    // ----------------------------------------------------------------------
    // build_structured_hook_output (all decision arms)
    // ----------------------------------------------------------------------
    #[test]
    fn test_build_structured_hook_output_allow_unchanged() {
        let out = build_structured_hook_output(HooksDecision::AllowUnchanged);
        let hs = &out["hookSpecificOutput"];
        assert_eq!(hs["hookEventName"].as_str(), Some("PreToolUse"));
        assert_eq!(hs["permissionDecision"].as_str(), Some("allow"));
    }

    #[test]
    fn test_build_structured_hook_output_allow_rewrite_sets_both_keys() {
        let updated = json!({"command": "wrapped"});
        let out = build_structured_hook_output(HooksDecision::AllowRewrite(updated));
        let hs = &out["hookSpecificOutput"];
        assert_eq!(hs["updatedInput"]["command"].as_str(), Some("wrapped"));
        assert_eq!(hs["modifiedArgs"]["command"].as_str(), Some("wrapped"));
    }

    #[test]
    fn test_build_structured_hook_output_allow_with_warning() {
        let out = build_structured_hook_output(HooksDecision::AllowWithWarning {
            user_message: "sys".to_string(),
            agent_message: "agent".to_string(),
        });
        let hs = &out["hookSpecificOutput"];
        assert_eq!(hs["permissionDecision"].as_str(), Some("allow"));
        assert_eq!(hs["agentMessage"].as_str(), Some("agent"));
        assert_eq!(hs["systemMessage"].as_str(), Some("sys"));
    }

    // ----------------------------------------------------------------------
    // wrapped shell command encode/decode
    // ----------------------------------------------------------------------
    #[test]
    fn test_build_wrapped_shell_command_project_uses_path_lookup() {
        let env = test_env();
        let cmd =
            build_wrapped_shell_command(HookScope::Project, &env, "/work", "cargo build").unwrap();
        assert!(cmd.starts_with("ahma hooks run-shell"));
        assert!(cmd.contains("--payload-base64"));
        assert!(cmd.contains(WRAPPED_BY_MARKER));
    }

    #[test]
    fn test_build_wrapped_shell_command_roundtrip_payload() {
        let env = test_env();
        let cmd =
            build_wrapped_shell_command(HookScope::Project, &env, "/work/dir", "echo x").unwrap();
        // Pull the base64 token (3rd whitespace-separated field after run-shell).
        let token = cmd
            .split_whitespace()
            .skip_while(|t| *t != "--payload-base64")
            .nth(1)
            .unwrap();
        let payload = decode_wrapped_shell_payload(token).unwrap();
        assert_eq!(payload.cwd, "/work/dir");
        assert_eq!(payload.command, "echo x");
    }

    #[test]
    fn test_decode_wrapped_shell_payload_bad_base64_errors() {
        let err = decode_wrapped_shell_payload("!!!not base64!!!").unwrap_err();
        assert!(err.to_string().contains("decode"));
    }

    #[test]
    fn test_decode_wrapped_shell_payload_valid_base64_bad_json_errors() {
        let encoded = URL_SAFE_NO_PAD.encode(b"not json at all");
        let err = decode_wrapped_shell_payload(&encoded).unwrap_err();
        assert!(err.to_string().contains("parse"));
    }

    // ----------------------------------------------------------------------
    // is_wrapped_shell_command
    // ----------------------------------------------------------------------
    #[test]
    fn test_is_wrapped_shell_command_detects_markers() {
        assert!(is_wrapped_shell_command(&format!(
            "foo {WRAPPED_BY_MARKER}"
        )));
        assert!(is_wrapped_shell_command("ahma run_terminal_command --x"));
        assert!(!is_wrapped_shell_command("cargo build --release"));
    }

    // ----------------------------------------------------------------------
    // ensure_root_object / ensure_child_object / ensure_child_array
    // ----------------------------------------------------------------------
    #[test]
    fn test_ensure_root_object_initializes_null() {
        let mut doc = Value::Null;
        ensure_root_object(&mut doc).unwrap();
        assert!(doc.is_object());
    }

    #[test]
    fn test_ensure_root_object_rejects_non_object() {
        let mut doc = Value::String("nope".to_string());
        let err = ensure_root_object(&mut doc).unwrap_err();
        assert!(err.to_string().contains("must be a JSON object"));
    }

    #[test]
    fn test_ensure_child_object_creates_and_reuses() {
        let mut map = Map::new();
        ensure_child_object(&mut map, "hooks").unwrap();
        assert!(map["hooks"].is_object());
        // Reuse path: existing object is returned, not overwritten.
        map["hooks"]
            .as_object_mut()
            .unwrap()
            .insert("k".to_string(), Value::Bool(true));
        ensure_child_object(&mut map, "hooks").unwrap();
        assert_eq!(map["hooks"]["k"].as_bool(), Some(true));
    }

    #[test]
    fn test_ensure_child_object_rejects_non_object_value() {
        let mut map = Map::new();
        map.insert("hooks".to_string(), Value::String("bad".to_string()));
        let err = ensure_child_object(&mut map, "hooks").unwrap_err();
        assert!(err.to_string().contains("must be a JSON object"));
    }

    #[test]
    fn test_ensure_child_array_creates_and_rejects_non_array() {
        let mut map = Map::new();
        ensure_child_array(&mut map, "list").unwrap();
        assert!(map["list"].is_array());

        let mut bad = Map::new();
        bad.insert("list".to_string(), Value::Bool(false));
        let err = ensure_child_array(&mut bad, "list").unwrap_err();
        assert!(err.to_string().contains("must be an array"));
    }

    // ----------------------------------------------------------------------
    // cleanup_empty_hook_tree
    // ----------------------------------------------------------------------
    #[test]
    fn test_cleanup_empty_hook_tree_removes_empty_event_and_hooks() {
        let mut root = Map::new();
        let mut hooks = Map::new();
        hooks.insert("PreToolUse".to_string(), Value::Array(Vec::new()));
        root.insert("hooks".to_string(), Value::Object(hooks));
        cleanup_empty_hook_tree(&mut root, "PreToolUse");
        assert!(!root.contains_key("hooks"), "empty hooks tree pruned");
    }

    #[test]
    fn test_cleanup_empty_hook_tree_keeps_nonempty() {
        let mut root = Map::new();
        let mut hooks = Map::new();
        hooks.insert(
            "PreToolUse".to_string(),
            Value::Array(vec![json!({"x": 1})]),
        );
        root.insert("hooks".to_string(), Value::Object(hooks));
        cleanup_empty_hook_tree(&mut root, "PreToolUse");
        assert!(root.contains_key("hooks"));
        assert_eq!(root["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    }

    // ----------------------------------------------------------------------
    // is_managed_* predicates
    // ----------------------------------------------------------------------
    #[test]
    fn test_is_managed_command_matches_id() {
        assert!(is_managed_command(&format!(
            "ahma hooks exec --managed-id {MANAGED_ID_DEFAULT_SHELL_V1}"
        )));
        assert!(!is_managed_command("some other command"));
    }

    #[test]
    fn test_is_managed_cursor_entry_variants() {
        let managed = json!({"command": format!("x {MANAGED_ID_DEFAULT_SHELL_V1}")});
        assert!(is_managed_cursor_entry(&managed));
        assert!(!is_managed_cursor_entry(&json!({"command": "echo"})));
        assert!(!is_managed_cursor_entry(&json!("not an object")));
    }

    #[test]
    fn test_is_managed_handler_via_args() {
        let handler = json!({
            "type": "command",
            "command": "ahma",
            "args": ["hooks", "exec", "--managed-id", MANAGED_ID_DEFAULT_SHELL_V1],
        });
        assert!(is_managed_handler(&handler));
    }

    #[test]
    fn test_is_managed_handler_via_command_and_command_windows() {
        let by_command = json!({"command": format!("ahma {MANAGED_ID_DEFAULT_SHELL_V1}")});
        assert!(is_managed_handler(&by_command));
        let by_windows = json!({"commandWindows": format!("ps {MANAGED_ID_DEFAULT_SHELL_V1}")});
        assert!(is_managed_handler(&by_windows));
        assert!(!is_managed_handler(&json!({"command": "plain"})));
        assert!(!is_managed_handler(&json!("not object")));
    }

    #[test]
    fn test_is_managed_group_entry() {
        let entry = json!({
            "hooks": [
                {"command": format!("ahma {MANAGED_ID_DEFAULT_SHELL_V1}")}
            ]
        });
        assert!(is_managed_group_entry(&entry));
        assert!(!is_managed_group_entry(
            &json!({"hooks": [{"command": "x"}]})
        ));
        assert!(!is_managed_group_entry(&json!({"no_hooks": true})));
    }

    #[test]
    fn test_is_managed_copilot_entry_variants() {
        let by_bash = json!({"bash": format!("ahma {MANAGED_ID_DEFAULT_SHELL_V1}")});
        assert!(is_managed_copilot_entry(&by_bash));
        let by_ps = json!({"powershell": format!("ps {MANAGED_ID_DEFAULT_SHELL_V1}")});
        assert!(is_managed_copilot_entry(&by_ps));
        assert!(!is_managed_copilot_entry(&json!({"bash": "echo"})));
        assert!(!is_managed_copilot_entry(&json!(42)));
    }

    // ----------------------------------------------------------------------
    // load_hook_document
    // ----------------------------------------------------------------------
    #[test]
    fn test_load_hook_document_missing_returns_empty_object() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nope.json");
        let doc = load_hook_document(&path).unwrap();
        assert_eq!(doc, Value::Object(Map::new()));
    }

    #[test]
    fn test_load_hook_document_empty_file_returns_empty_object() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.json");
        fs::write(&path, "   \n").unwrap();
        let doc = load_hook_document(&path).unwrap();
        assert_eq!(doc, Value::Object(Map::new()));
    }

    #[test]
    fn test_load_hook_document_invalid_json_errors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.json");
        fs::write(&path, "{ this is not json").unwrap();
        let err = load_hook_document(&path).unwrap_err();
        assert!(err.to_string().contains("Failed to parse JSON hook config"));
    }

    #[test]
    fn test_load_hook_document_valid_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ok.json");
        fs::write(&path, r#"{"version":1}"#).unwrap();
        let doc = load_hook_document(&path).unwrap();
        assert_eq!(doc["version"].as_i64(), Some(1));
    }

    // ----------------------------------------------------------------------
    // write_hook_document & backup_path
    // ----------------------------------------------------------------------
    #[test]
    fn test_write_hook_document_unchanged() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("c.json");
        let same = json!({"a": 1});
        let action = write_hook_document(&path, &same, &same, false, true).unwrap();
        assert_eq!(action, FileAction::Unchanged);
        assert!(!path.exists(), "unchanged must not write a file");
    }

    #[test]
    fn test_write_hook_document_dry_run_created_does_not_write() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("d.json");
        let action = write_hook_document(
            &path,
            &Value::Object(Map::new()),
            &json!({"a": 1}),
            true,
            false,
        )
        .unwrap();
        assert_eq!(action, FileAction::Created);
        assert!(!path.exists(), "dry run must not write");
    }

    #[test]
    fn test_write_hook_document_dry_run_updated() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("e.json");
        let action =
            write_hook_document(&path, &json!({"a": 1}), &json!({"a": 2}), true, true).unwrap();
        assert_eq!(action, FileAction::Updated);
    }

    #[test]
    fn test_write_hook_document_created_writes_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("f.json");
        let action = write_hook_document(
            &path,
            &Value::Object(Map::new()),
            &json!({"hello": "world"}),
            false,
            false,
        )
        .unwrap();
        assert_eq!(action, FileAction::Created);
        let written = load_hook_document(&path).unwrap();
        assert_eq!(written["hello"].as_str(), Some("world"));
    }

    #[test]
    fn test_write_hook_document_updated_creates_backup() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("g.json");
        fs::write(&path, r#"{"a":1}"#).unwrap();
        let action =
            write_hook_document(&path, &json!({"a": 1}), &json!({"a": 2}), false, true).unwrap();
        assert_eq!(action, FileAction::Updated);
        let backup = backup_path(&path).unwrap();
        assert!(
            backup.exists(),
            "an existing file must be backed up on update"
        );
        let updated = load_hook_document(&path).unwrap();
        assert_eq!(updated["a"].as_i64(), Some(2));
    }

    #[test]
    fn test_backup_path_appends_bak() {
        let path = Path::new("/some/dir/settings.json");
        let backup = backup_path(path).unwrap();
        assert_eq!(backup, Path::new("/some/dir/settings.json.bak"));
    }

    // ----------------------------------------------------------------------
    // remove_managed_hook_entries error/early-return paths
    // ----------------------------------------------------------------------
    #[test]
    fn test_remove_managed_hook_entries_non_object_errors() {
        let mut doc = Value::String("nope".to_string());
        let err =
            remove_managed_hook_entries(&mut doc, "Lbl", "PreToolUse", is_managed_group_entry)
                .unwrap_err();
        assert!(err.to_string().contains("must be a JSON object"));
    }

    #[test]
    fn test_remove_managed_hook_entries_no_hooks_returns_false() {
        let mut doc = json!({"version": 1});
        let changed =
            remove_managed_hook_entries(&mut doc, "Lbl", "PreToolUse", is_managed_group_entry)
                .unwrap();
        assert!(!changed);
    }

    #[test]
    fn test_remove_managed_hook_entries_hooks_not_object_errors() {
        let mut doc = json!({"hooks": "bad"});
        let err =
            remove_managed_hook_entries(&mut doc, "Lbl", "PreToolUse", is_managed_group_entry)
                .unwrap_err();
        assert!(err.to_string().contains("'hooks' must be an object"));
    }

    #[test]
    fn test_remove_managed_hook_entries_no_event_key_returns_false() {
        let mut doc = json!({"hooks": {}});
        let changed =
            remove_managed_hook_entries(&mut doc, "Lbl", "PreToolUse", is_managed_group_entry)
                .unwrap();
        assert!(!changed);
    }

    #[test]
    fn test_remove_managed_hook_entries_event_not_array_errors() {
        let mut doc = json!({"hooks": {"PreToolUse": "bad"}});
        let err =
            remove_managed_hook_entries(&mut doc, "Lbl", "PreToolUse", is_managed_group_entry)
                .unwrap_err();
        assert!(err.to_string().contains("hook list must be an array"));
    }

    // ----------------------------------------------------------------------
    // hook_status_string
    // ----------------------------------------------------------------------
    #[test]
    fn test_hook_status_string_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("absent.json");
        assert_eq!(
            hook_status_string(&path, HookPlatform::Cursor).unwrap(),
            "missing"
        );
    }

    #[test]
    fn test_hook_status_string_installed_and_not_installed() {
        let env = test_env();
        let path = env.config_path(HookPlatform::Cursor, HookScope::User);
        let mut document = Value::Object(Map::new());
        install_platform_hook(&mut document, HookPlatform::Cursor, HookScope::User, &env).unwrap();
        write_hook_document(&path, &Value::Object(Map::new()), &document, false, false).unwrap();
        assert_eq!(
            hook_status_string(&path, HookPlatform::Cursor).unwrap(),
            "installed"
        );

        // A file that exists but has no managed entry → not installed.
        let dir = tempdir().unwrap();
        let other = dir.path().join("plain.json");
        fs::write(&other, r#"{"hooks":{"preToolUse":[]}}"#).unwrap();
        assert_eq!(
            hook_status_string(&other, HookPlatform::Cursor).unwrap(),
            "not installed"
        );
    }

    // ----------------------------------------------------------------------
    // exec_args / build_exec_command / self-contained grouped command
    // ----------------------------------------------------------------------
    #[test]
    fn test_exec_args_shape() {
        let args = exec_args(HookPlatform::Claude, HookScope::Project);
        assert_eq!(args[0], "hooks");
        assert_eq!(args[1], "exec");
        assert!(args.contains(&"--platform".to_string()));
        assert!(args.contains(&"claude".to_string()));
        assert!(args.contains(&"project".to_string()));
        assert!(args.contains(&MANAGED_ID_DEFAULT_SHELL_V1.to_string()));
    }

    /// The Claude grouped hook's `command` must be self-contained: a consumer
    /// that runs only `command` (e.g. Cursor importing Claude hooks) must still
    /// invoke `ahma hooks exec …`, never bare `ahma`. Regression for the
    /// "Hook blocked with message: <usage banner>" failure.
    #[test]
    fn test_claude_group_command_is_self_contained_user_absolute() {
        let env = test_env();
        let entry = managed_group_entry(HookPlatform::Claude, HookScope::User, &env);
        let handler = &entry["hooks"][0];
        let command = handler["command"].as_str().unwrap();
        assert!(command.contains("bin space"), "absolute path: {command}");
        assert!(command.contains("hooks"));
        assert!(command.contains("exec"));
        assert!(command.contains(MANAGED_ID_DEFAULT_SHELL_V1));
        assert!(
            handler.get("args").is_none(),
            "must not split into a separate `args` array: {handler:?}",
        );
    }

    #[test]
    fn test_claude_group_command_is_self_contained_project_path_lookup() {
        let env = test_env();
        let entry = managed_group_entry(HookPlatform::Claude, HookScope::Project, &env);
        let command = entry["hooks"][0]["command"].as_str().unwrap();
        assert!(
            command.starts_with("ahma hooks exec"),
            "project scope uses PATH lookup + self-contained command: {command}",
        );
        assert!(command.contains(MANAGED_ID_DEFAULT_SHELL_V1));
    }

    #[test]
    fn test_build_exec_command_project_is_ahma_prefixed() {
        let env = test_env();
        let cmd = build_exec_command(HookPlatform::Codex, HookScope::Project, &env);
        assert!(cmd.starts_with("ahma hooks exec"));
        assert!(cmd.contains("codex"));
    }

    // ----------------------------------------------------------------------
    // codex_group_entry
    // ----------------------------------------------------------------------
    #[test]
    fn test_codex_group_entry_user_scope_basic_handler() {
        let env = test_env();
        let entry = codex_group_entry(HookScope::User, &env);
        assert_eq!(entry["matcher"].as_str(), Some("^Bash$"));
        let handler = &entry["hooks"][0];
        assert_eq!(handler["type"].as_str(), Some("command"));
        assert!(handler["command"].as_str().unwrap().contains("bin space"));
        assert_eq!(
            handler["statusMessage"].as_str(),
            Some("Routing Bash through ahma")
        );
        // Non-windows user scope should not add commandWindows.
        #[cfg(not(target_os = "windows"))]
        assert!(handler.get("commandWindows").is_none());
    }

    #[test]
    fn test_codex_group_entry_project_scope_adds_command_windows() {
        let env = test_env();
        let entry = codex_group_entry(HookScope::Project, &env);
        let handler = &entry["hooks"][0];
        // Project scope always adds a Windows variant.
        assert!(handler["commandWindows"].as_str().is_some());
        assert!(handler["command"].as_str().unwrap().starts_with("ahma"));
    }

    #[test]
    fn test_managed_group_entry_claude_matcher_bash() {
        let env = test_env();
        let entry = managed_group_entry(HookPlatform::Claude, HookScope::User, &env);
        assert_eq!(entry["matcher"].as_str(), Some("Bash"));
        assert!(is_managed_group_entry(&entry));
    }

    #[test]
    fn test_managed_group_entry_antigravity_matcher_run_command() {
        let env = test_env();
        let entry = managed_group_entry(HookPlatform::Antigravity, HookScope::User, &env);
        assert_eq!(entry["matcher"].as_str(), Some("run_command"));
    }

    // ----------------------------------------------------------------------
    // shell quoting / absolute command builders
    // ----------------------------------------------------------------------
    #[test]
    fn test_shell_quote_posix_escapes_single_quotes() {
        assert_eq!(shell_quote_posix("plain"), "'plain'");
        assert_eq!(shell_quote_posix("a'b"), "'a'\\''b'");
    }

    #[test]
    fn test_powershell_quote_doubles_single_quotes() {
        assert_eq!(powershell_quote("plain"), "'plain'");
        assert_eq!(powershell_quote("a'b"), "'a''b'");
    }

    #[test]
    fn test_build_absolute_shell_command_quotes_path_and_args() {
        let cmd = build_absolute_shell_command(
            Path::new("/opt/bin space/ahma"),
            &["hooks".to_string(), "exec".to_string()],
        );
        assert!(cmd.contains("bin space"));
        assert!(cmd.contains("hooks"));
        assert!(cmd.contains("exec"));
    }

    // ----------------------------------------------------------------------
    // compute_exec_decision_internal: string-payload + non-shell + cwd fallback
    // ----------------------------------------------------------------------
    #[test]
    fn test_compute_exec_decision_internal_string_payload_rewrites() {
        let env = test_env();
        let input = json!({
            "cwd": "/proj",
            "toolArgs": "{\"command\": \"cargo test\"}"
        });
        let decision =
            compute_exec_decision_internal(&input, HookScope::Project, &env, true, false, None);
        assert!(matches!(decision, HooksDecision::AllowRewrite(_)));
    }

    #[test]
    fn test_compute_exec_decision_defers_to_host_when_detected() {
        let env = test_env();
        let input = json!({"tool_input": {"command": "cargo build"}});
        // With a host sandbox injected, ahma must defer (not rewrite to run-shell).
        let decision = compute_exec_decision_internal(
            &input,
            HookScope::User,
            &env,
            true,
            false,
            Some(crate::sandbox::HostSandbox::Cursor),
        );
        match decision {
            HooksDecision::DeferToHost { user_message, .. } => {
                assert!(
                    user_message.contains("Cursor"),
                    "names the host: {user_message}"
                );
                assert!(
                    user_message.contains("DEFERRING") && user_message.contains("UNSANDBOXED"),
                    "must disclose deferral + the host-off risk: {user_message}"
                );
            }
            other => panic!("expected DeferToHost, got {other:?}"),
        }
    }

    #[test]
    fn test_compute_exec_decision_rewrites_when_no_host() {
        let env = test_env();
        let input = json!({"tool_input": {"command": "cargo build"}});
        // No host → ahma stays authoritative and wraps the command in its sandbox.
        let decision =
            compute_exec_decision_internal(&input, HookScope::User, &env, true, false, None);
        assert!(matches!(decision, HooksDecision::AllowRewrite(_)));
    }

    #[test]
    fn test_defer_to_host_runs_command_unchanged_cursor() {
        let out = build_cursor_hook_output(HooksDecision::DeferToHost {
            user_message: "msg".to_string(),
            agent_message: "agent".to_string(),
        });
        assert_eq!(out["permission"].as_str(), Some("allow"));
        assert!(
            out.get("updated_input").is_none(),
            "deferral must run the original command unchanged (no rewrite)"
        );
    }

    #[test]
    fn test_defer_to_host_structured_output_allows() {
        let out = build_structured_hook_output(HooksDecision::DeferToHost {
            user_message: "msg".to_string(),
            agent_message: "agent".to_string(),
        });
        assert_eq!(
            out["hookSpecificOutput"]["permissionDecision"].as_str(),
            Some("allow")
        );
        assert!(out["hookSpecificOutput"].get("updatedInput").is_none());
    }

    #[test]
    fn test_compute_exec_decision_internal_malformed_args_allows_unchanged() {
        let env = test_env();
        // toolArgs string that is invalid JSON → extract_tool_args Err → allow.
        let input = json!({"toolArgs": "{bad"});
        let decision =
            compute_exec_decision_internal(&input, HookScope::User, &env, true, false, None);
        assert!(matches!(decision, HooksDecision::AllowUnchanged));
    }

    #[test]
    fn test_compute_exec_decision_internal_no_cwd_uses_current_dir_and_rewrites() {
        let env = test_env();
        // No `working_directory`/`cwd` → falls back to current_dir (succeeds) → rewrite.
        let input = json!({"tool_input": {"command": "ls"}});
        let decision =
            compute_exec_decision_internal(&input, HookScope::User, &env, true, false, None);
        assert!(matches!(decision, HooksDecision::AllowRewrite(_)));
    }

    // ----------------------------------------------------------------------
    // is_ahma_hooks_active_with_configs / describe_activation env precedence
    // ----------------------------------------------------------------------
    #[test]
    fn test_is_ahma_hooks_active_env_off_overrides_active_configs() {
        let _g = ENV_LOCK.lock().unwrap();
        let _restore = EnvGuard::capture();
        unsafe {
            std::env::set_var("AHMA_HOOKS", "off");
            std::env::remove_var("AHMA_DISABLE_HOOKS");
        }
        // Even with a configured MCP path, AHMA_HOOKS=off wins.
        assert!(!is_ahma_hooks_active_with_configs(&[PathBuf::from("/x")]));
    }

    #[test]
    fn test_is_ahma_hooks_active_env_on_overrides_no_configs() {
        let _g = ENV_LOCK.lock().unwrap();
        let _restore = EnvGuard::capture();
        unsafe {
            std::env::set_var("AHMA_HOOKS", "on");
            std::env::remove_var("AHMA_DISABLE_HOOKS");
        }
        assert!(is_ahma_hooks_active_with_configs(&[]));
    }

    #[test]
    fn test_is_ahma_hooks_active_disable_alias() {
        let _g = ENV_LOCK.lock().unwrap();
        let _restore = EnvGuard::capture();
        unsafe {
            std::env::remove_var("AHMA_HOOKS");
            std::env::set_var("AHMA_DISABLE_HOOKS", "1");
        }
        assert!(!is_ahma_hooks_active_with_configs(&[PathBuf::from("/x")]));
    }

    #[test]
    fn test_describe_activation_env_reasons() {
        let _g = ENV_LOCK.lock().unwrap();
        let _restore = EnvGuard::capture();

        unsafe {
            std::env::set_var("AHMA_HOOKS", "off");
            std::env::remove_var("AHMA_DISABLE_HOOKS");
        }
        let (active, reason) = describe_activation(&[PathBuf::from("/x")]);
        assert!(!active);
        assert_eq!(reason, "AHMA_HOOKS=off");

        unsafe { std::env::set_var("AHMA_HOOKS", "on") };
        let (active, reason) = describe_activation(&[]);
        assert!(active);
        assert_eq!(reason, "AHMA_HOOKS=on");

        unsafe {
            std::env::remove_var("AHMA_HOOKS");
            std::env::set_var("AHMA_DISABLE_HOOKS", "1");
        }
        let (active, reason) = describe_activation(&[PathBuf::from("/x")]);
        assert!(!active);
        assert_eq!(reason, "AHMA_DISABLE_HOOKS=1");
    }

    #[test]
    fn test_describe_activation_auto_active_reason() {
        let _g = ENV_LOCK.lock().unwrap();
        let _restore = EnvGuard::capture();
        unsafe {
            std::env::remove_var("AHMA_HOOKS");
            std::env::remove_var("AHMA_DISABLE_HOOKS");
        }
        let (active, reason) = describe_activation(&[PathBuf::from("/x/.claude.json")]);
        assert!(active);
        assert_eq!(reason, "auto: ahma MCP server detected in client config");
    }

    // ----------------------------------------------------------------------
    // mcp_config_candidates includes project .vscode/mcp.json
    // ----------------------------------------------------------------------
    #[test]
    fn test_mcp_config_candidates_includes_project_vscode() {
        let home = Path::new("/home/user");
        let project = Path::new("/proj");
        let with_project = mcp_config_candidates(home, Some(project));
        assert!(with_project.contains(&project.join(".vscode").join("mcp.json")));
        // Without a project root the vscode project path is absent.
        let without = mcp_config_candidates(home, None);
        assert!(!without.contains(&project.join(".vscode").join("mcp.json")));
        assert!(without.contains(&home.join(".cursor").join("mcp.json")));
    }

    // ----------------------------------------------------------------------
    // grouped/cursor/copilot install round-trips through uninstall
    // ----------------------------------------------------------------------
    #[test]
    fn test_cursor_install_then_uninstall_cleans_tree() {
        let env = test_env();
        let mut document = Value::Object(Map::new());
        install_platform_hook(&mut document, HookPlatform::Cursor, HookScope::User, &env).unwrap();
        assert!(cursor_hook_installed(&document));
        let changed = uninstall_platform_hook(&mut document, HookPlatform::Cursor).unwrap();
        assert!(changed);
        assert!(!cursor_hook_installed(&document));
    }

    #[test]
    fn test_install_grouped_hook_replaces_existing_managed_entry() {
        let env = test_env();
        let mut document = Value::Object(Map::new());
        install_platform_hook(&mut document, HookPlatform::Claude, HookScope::User, &env).unwrap();
        // Install again — must replace, not duplicate (retain drops old managed).
        install_platform_hook(&mut document, HookPlatform::Claude, HookScope::User, &env).unwrap();
        let entries = document["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(
            entries.len(),
            1,
            "re-install must not duplicate the managed entry"
        );
    }
}
