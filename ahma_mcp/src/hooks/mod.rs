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

const MANAGED_ID_DEFAULT_SHELL_V1: &str = "ahma-default-shell-v1";
const WRAPPED_BY_MARKER: &str = "ahma-hooks-wrapper-v1";
const HOOK_TIMEOUT_SECS: u64 = 30;
const PATH_LOOKUP_BINARY: &str = "ahma";

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
        HooksCommand::RunShell(args) => run_shell(args, cfg).await,
        HooksCommand::ApproveUnsandboxed => run_approve_unsandboxed(),
        HooksCommand::Revoke => run_revoke_consent(),
        HooksCommand::Doctor => run_doctor(),
    }
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
fn is_ahma_hooks_active_with_configs(active_mcps: &[PathBuf]) -> bool {
    if let Some(Some(forced)) = HOOKS_MODE_OVERRIDE.get() {
        return *forced;
    }
    if let Ok(val) = std::env::var("AHMA_HOOKS") {
        match val.to_lowercase().as_str() {
            "off" | "0" | "false" | "no" => return false,
            "on" | "1" | "true" | "yes" => return true,
            _ => {}
        }
    }
    if std::env::var("AHMA_DISABLE_HOOKS")
        .ok()
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return false;
    }
    // Auto: active if any editor config has an ahma MCP server configured.
    !active_mcps.is_empty()
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
    if active {
        println!(
            "\x1b[1mEffective: \x1b[32mACTIVE\x1b[0m\x1b[1m\x1b[0m ({reason}) — installed hooks route shell commands through ahma's sandbox.\n"
        );
    } else {
        println!(
            "\x1b[1mEffective: \x1b[33mINACTIVE\x1b[0m ({reason}) — installed hooks pass commands through to the default terminal \x1b[1mUNSANDBOXED\x1b[0m.\n      Force on with AHMA_HOOKS=on, or fix ahma's MCP config so `auto` detects it.\n"
        );
    }

    println!("{:<14} {:<8} {:<14} Config", "Platform", "Scope", "Status");
    println!("{:-<14} {:-<8} {:-<14} {:-<6}", "", "", "", "");

    let mut installed_hooks = Vec::new();
    let mut installed_count = 0;
    for scope in scopes {
        for platform in selected_platforms(&args.platforms) {
            let path = env.config_path(platform, scope);
            let status = hook_status_string(&path, platform)?;
            if status == "installed" {
                installed_count += 1;
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

    if installed_count > 0 && !active_mcps.is_empty() {
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
        println!(
            "                         runs through its NATIVE terminal/Bash tool, which never"
        );
        println!("                         pass through MCP. Without hooks those run unsandboxed.");
        println!("      Together they give full sandbox coverage. A command is only ever wrapped");
        println!("      once (already-wrapped and MCP tool calls are passed through untouched).");
        println!();
        println!("  \x1b[1mactive terminal hooks:\x1b[0m");
        for (platform, scope, path) in &installed_hooks {
            println!(
                "    - {} ({} scope) at {}",
                platform.label(),
                scope.label(),
                path.display()
            );
        }
        println!();
        println!("  \x1b[1mactive MCP configurations:\x1b[0m");
        for path in &active_mcps {
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

    Ok(())
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

/// Write the decision to stdout for the IDE, and mirror any fail-open warning to
/// stderr so it is visible even when the IDE does not surface allow-time messages.
fn emit_decision(decision: HooksDecision, platform: HookPlatform) -> Result<()> {
    match &decision {
        HooksDecision::AllowWithWarning { user_message, .. }
        | HooksDecision::DenyPendingConsent { user_message, .. } => {
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
        Err(e) => Err(e),
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
    let decision =
        compute_exec_decision_internal(input, scope, env, is_ahma_hooks_active(), consented);
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

    Ok(())
}

fn uninstall_cursor_hook(document: &mut Value) -> Result<bool> {
    remove_managed_hook_entries(
        document,
        "Cursor",
        HookPlatform::Cursor.event_key(),
        is_managed_cursor_entry,
    )
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

fn codex_group_entry(scope: HookScope, env: &HookEnvironment) -> Value {
    let args = exec_args(HookPlatform::Codex, scope);
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
        Value::String("Routing Bash through ahma".to_string()),
    );
    json!({
        "matcher": "^Bash$",
        "hooks": [Value::Object(handler)],
    })
}

fn managed_group_entry(platform: HookPlatform, scope: HookScope, env: &HookEnvironment) -> Value {
    match platform {
        HookPlatform::Claude | HookPlatform::Antigravity => {
            let (command, args) = exec_command_and_args(platform, scope, env);
            let matcher = match platform {
                HookPlatform::Claude => "Bash",
                HookPlatform::Antigravity => "run_command",
                _ => unreachable!(),
            };
            json!({
                "matcher": matcher,
                "hooks": [
                    {
                        "type": "command",
                        "command": command,
                        "args": args,
                        "timeout": HOOK_TIMEOUT_SECS,
                    }
                ]
            })
        }
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

fn exec_command_and_args(
    platform: HookPlatform,
    scope: HookScope,
    env: &HookEnvironment,
) -> (String, Vec<String>) {
    let binary = match BinaryReference::for_scope(env, scope) {
        BinaryReference::Absolute(path) => path.to_string_lossy().into_owned(),
        BinaryReference::PathLookup => PATH_LOOKUP_BINARY.to_string(),
    };
    (binary, exec_args(platform, scope))
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
            compute_exec_decision_internal(&input, HookScope::Project, &env, true, false);
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
            compute_exec_decision_internal(&input, HookScope::Project, &env, true, false);
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
                compute_exec_decision_internal(&input, HookScope::User, &env, true, false);
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
        let decision =
            compute_exec_decision_internal(&input_no_args, HookScope::User, &env, true, false);
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

        let command = installed["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(command.contains("bin space"));

        let args = installed["hooks"]["PreToolUse"][0]["hooks"][0]["args"]
            .as_array()
            .unwrap();
        assert!(
            args.iter()
                .any(|arg| arg.as_str() == Some(MANAGED_ID_DEFAULT_SHELL_V1))
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
        let decision = compute_exec_decision_internal(&input, HookScope::User, &env, false, false);
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
        let decision = compute_exec_decision_internal(&input, HookScope::User, &env, true, false);
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
        let decision = compute_exec_decision_internal(&input, HookScope::User, &env, true, false);
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
}
