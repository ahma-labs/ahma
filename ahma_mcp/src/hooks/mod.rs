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
    /// `wrapped_command` is the literal rewritten command string (before it was
    /// merged into `updated_input`) — kept alongside so a platform's hook
    /// output builder can derive a stable match pattern from it (see
    /// `build_antigravity_permission_override`) without re-parsing JSON.
    AllowRewrite {
        updated_input: Value,
        wrapped_command: String,
    },
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

#[derive(Args, Debug, Clone)]
pub struct HooksRunShellArgs {
    /// Legacy or compact base64-encoded payload JSON (contains cwd, command, session_id).
    #[arg(long = "payload-base64")]
    pub payload_base64: Option<String>,

    /// Working directory for command execution.
    #[arg(long = "cwd")]
    pub cwd: Option<PathBuf>,

    /// Session ID for grouping hooked commands in the daemon / TUI.
    #[arg(long = "session-id")]
    pub session_id: Option<String>,

    /// The command to execute.
    #[arg(long = "command")]
    pub command: Option<String>,

    /// Wrapped-by marker for recursion prevention.
    #[arg(long, default_value = WRAPPED_BY_MARKER)]
    pub wrapped_by: String,

    /// Raw command and arguments (optional trailing args if not using `--command`).
    #[arg(last = true)]
    pub raw_command: Vec<String>,
}

impl HooksRunShellArgs {
    pub fn resolve_payload(self) -> Result<WrappedShellPayload> {
        if let Some(ref b64) = self.payload_base64 {
            return decode_wrapped_shell_payload(b64);
        }

        let cwd = self
            .cwd
            .map(|p| p.to_string_lossy().into_owned())
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| ".".to_string());

        let command = if let Some(cmd) = self.command {
            cmd
        } else if !self.raw_command.is_empty() {
            self.raw_command.join(" ")
        } else {
            anyhow::bail!(
                "hooks run-shell requires either --command, trailing command arguments, or --payload-base64"
            );
        };

        Ok(WrappedShellPayload {
            cwd,
            command,
            session_id: self.session_id,
        })
    }
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

    /// Where this client keeps its hook config, relative to the scope root.
    ///
    /// A flat `(platform, scope)` lookup table: the three clients that use one
    /// path for both scopes say so with `_`, and the two that differ per scope
    /// have both rows visible side by side rather than in a nested `match`.
    fn config_relative_path(self, scope: HookScope) -> PathBuf {
        let relative = match (self, scope) {
            (Self::Cursor, _) => ".cursor/hooks.json",
            (Self::Claude, _) => ".claude/settings.json",
            (Self::Codex, _) => ".codex/hooks.json",
            (Self::Copilot, HookScope::User) => ".copilot/hooks/ahma.json",
            (Self::Copilot, HookScope::Project) => ".github/hooks/ahma.json",
            (Self::Antigravity, HookScope::User) => ".gemini/config/hooks.json",
            (Self::Antigravity, HookScope::Project) => ".agents/hooks.json",
        };
        PathBuf::from(relative)
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

    /// Whether `ahma setup` installs terminal hooks for this client **by default**
    /// (SPEC R-PERM.6 / R5.5.4).
    ///
    /// Hooks were disabled globally not because their sandbox classification was
    /// wrong, but because a denial had nowhere to go: the user could not be asked,
    /// so the only outcomes were "blocked, with no way forward" or "let it
    /// through". The question ladder (R-PERM.3) fixes that — but only for clients
    /// where the loop demonstrably closes:
    ///
    ///   1. a denial round-trips deny → question → grant → the **next** command
    ///      succeeds (hooks re-derive their sandbox per command, so a grant needs
    ///      no restart — that is the hooks path's genuine advantage);
    ///   2. the fail-closed message is legible in that client, not a bare
    ///      `Operation not permitted` buried in a build log;
    ///   3. inside a detected host sandbox, R7.2 defer-to-host still applies —
    ///      which is what removes most of the surface where hooks "got in the way".
    ///
    /// Clients are added here as each is exercised end-to-end. A client that is not
    /// listed is not broken — it simply has not been proven, and `ahma setup` says
    /// so rather than silently omitting it.
    fn hooks_ready(self) -> bool {
        match self {
            // Exercised end-to-end: the denial → ask → grant → next-command-succeeds
            // loop closes, and the fail-closed message lands where the user reads it.
            Self::Cursor | Self::Claude => true,
            // Not yet proven. Opt in explicitly with `--hooks` if you want them.
            Self::Codex | Self::Copilot | Self::Antigravity => false,
        }
    }

    /// Why hooks are not installed by default for this client — shown to the user
    /// instead of leaving the omission unexplained (R5.5.4).
    fn hooks_not_ready_reason(self) -> Option<&'static str> {
        if self.hooks_ready() {
            return None;
        }
        Some(
            "the deny → ask → grant loop has not been verified end-to-end in this client yet;              install them explicitly with `ahma setup --hooks` if you want to try",
        )
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
pub struct WrappedShellPayload {
    pub cwd: String,
    pub command: String,
    /// The editor session this command belongs to, when the hook input named
    /// one. Lets a TUI group hooked work with the session that caused it
    /// (SPEC R-DAEMON.6). `#[serde(default)]` so a command wrapped by an older
    /// ahma — the payload is base64 in someone's shell history, with no version
    /// to negotiate — still decodes.
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileAction {
    Created,
    Updated,
    Unchanged,
}

/// The clients for which `ahma setup` installs terminal hooks by default, and the
/// reason for each that it skips (SPEC R-PERM.6 / R5.5.4).
///
/// Returned as `(label, ready, reason)` so the caller can *explain* an omission
/// rather than leave it as an unexplained gap. An unexplained default is how the
/// old blanket "hooks are not ready" ended up looking arbitrary.
pub fn hooks_readiness() -> Vec<(&'static str, bool, Option<&'static str>)> {
    HookPlatform::all()
        .into_iter()
        .map(|p| (p.label(), p.hooks_ready(), p.hooks_not_ready_reason()))
        .collect()
}

/// Whether *any* client is ready for hooks to be installed by default.
///
/// This is the gate that replaced the global "hooks are not ready" filter. It is
/// no longer a property of ahma; it is a property of each client, and it became
/// true the moment a denial could reach a human there (R-PERM.3).
pub fn any_client_ready_for_hooks() -> bool {
    HookPlatform::all().into_iter().any(|p| p.hooks_ready())
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
/// own breakage (the same fail-open philosophy `run_exec` uses for malformed
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
    args.iter()
        .find_map(|a| a.strip_prefix("--platform="))
        .or_else(|| {
            args.windows(2)
                .find(|w| w[0] == "--platform")
                .map(|w| w[1].as_str())
        })
        .and_then(parse_platform_cli_name)
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
    print_doctor_binary_info();
    print_doctor_kernel_sandbox_info();
    print_doctor_consent_info();
    Ok(())
}

fn print_doctor_binary_info() {
    match std::env::current_exe() {
        Ok(p) => println!("  binary       : {}", p.display()),
        Err(e) => println!("  binary       : <unknown> ({e})"),
    }
    println!("  version      : {}", env!("CARGO_PKG_VERSION"));
}

fn print_doctor_kernel_sandbox_info() {
    match crate::sandbox::test_sandbox_exec_available() {
        Ok(()) => println!("  kernel sandbox: AVAILABLE"),
        Err(e) => println!(
            "  kernel sandbox: UNAVAILABLE — {e}\n\
             \n  Repair: reinstall/update ahma so the hook binary can enforce the sandbox,\n\
             then retry. On macOS ensure `sandbox-exec` is present; on Linux ensure a\n\
             Landlock-capable kernel (5.13+)."
        ),
    }
}

fn print_doctor_consent_info() {
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
    if platform == HookPlatform::Antigravity && scope == HookScope::User && !dry_run {
        let _ = remove_antigravity_permissions(env);
    }

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

/// Parse an on/off flag value shared by the `--hooks` CLI flag and the
/// `AHMA_HOOKS` env var, so the two entry points can never drift out of sync
/// on which synonyms they accept. Returns `None` for `"auto"` and anything
/// else unrecognised.
fn parse_on_off_flag(value: &str) -> Option<bool> {
    match value.to_lowercase().as_str() {
        "off" | "0" | "false" | "no" => Some(false),
        "on" | "1" | "true" | "yes" => Some(true),
        _ => None,
    }
}

/// Set hooks behaviour from the `--hooks on|off|auto` CLI flag.
/// Call once, early in startup. Takes precedence over `AHMA_HOOKS` /
/// `AHMA_DISABLE_HOOKS` (which remain supported because hook subprocesses
/// can only be configured through the environment).
pub fn set_hooks_mode_override(mode: &str) {
    let _ = HOOKS_MODE_OVERRIDE.set(parse_on_off_flag(mode));
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
    if let Some(res) = check_explicit_hooks_override() {
        return res;
    }
    if let Some(res) = check_env_hooks_override() {
        return res;
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

fn check_explicit_hooks_override() -> Option<(bool, String)> {
    let forced = (*HOOKS_MODE_OVERRIDE.get()?)?;
    Some((
        forced,
        format!("--hooks {} flag", if forced { "on" } else { "off" }),
    ))
}

fn check_env_hooks_override() -> Option<(bool, String)> {
    if let Ok(val) = std::env::var("AHMA_HOOKS")
        && let Some(forced) = parse_on_off_flag(&val)
    {
        return Some((
            forced,
            format!("AHMA_HOOKS={}", if forced { "on" } else { "off" }),
        ));
    }
    if std::env::var("AHMA_DISABLE_HOOKS")
        .ok()
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return Some((false, "AHMA_DISABLE_HOOKS=1".to_string()));
    }
    None
}

fn detect_active_mcp_configs() -> Vec<PathBuf> {
    // Resolve through `ahma_home_dir` (not `$HOME`): on Windows `$HOME` is
    // usually unset, which previously made auto-detection silently report
    // "inactive" there. `ahma_home_dir` falls back to `dirs::home_dir`, so that
    // fix stands, and it additionally honours `AHMA_TEST_HOME` in debug builds
    // so this detection can be exercised without touching the real home.
    let Some(home) = ahma_common::config::ahma_home_dir() else {
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
    // Derived from `harness_target`, not listed again here. The per-harness copy
    // this replaced had already drifted — it omitted Claude Code, Codex and
    // Antigravity, exactly the clients that get both hooks and an MCP server —
    // and nothing could catch that, because a list of literals cannot disagree
    // with anything. Going through `PLATFORMS` means a new harness arrives here
    // the moment it is added there.
    let mut paths: Vec<PathBuf> = crate::harness_target::PLATFORMS
        .iter()
        .filter_map(|p| p.mcp_config(home).map(|(path, _format)| path))
        .collect();

    // `mcp_config` gives the running OS's path, which is all setup and uninstall
    // need. Detection also probes the other OSes' — see
    // `foreign_os_mcp_config_paths` for why erring wide is the safe direction.
    paths.extend(crate::harness_target::foreign_os_mcp_config_paths(home));

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
            && content
                .as_bytes()
                .windows(4)
                .any(|w| w.eq_ignore_ascii_case(b"ahma"))
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

    if args.platform == HookPlatform::Antigravity && args.scope == HookScope::User {
        let _ = ensure_antigravity_global_permission_grants(&env);
    }

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

fn collect_command_output_fields(object: &Map<String, Value>, into: &mut String) {
    for key in COMMAND_OUTPUT_KEYS {
        if let Some(s) = object.get(*key).and_then(Value::as_str) {
            append_nonempty_output_chunk(into, s);
        }
    }
}

fn append_nonempty_output_chunk(into: &mut String, s: &str) {
    if !s.is_empty() {
        if !into.is_empty() {
            into.push('\n');
        }
        into.push_str(s);
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

/// How long a hooked command waits for its report to reach the daemon.
const HOOK_REPORT_FLUSH: std::time::Duration = std::time::Duration::from_millis(300);

/// Wait, briefly, for this command's terminal event to be written to the hub.
///
/// The operation id is not known to this function, so it waits for *any*
/// terminal event: a hook process runs exactly one command, so the first one is
/// this one.
async fn flush_hook_report(mut reporter: crate::daemon_reporter::ReporterHandle) {
    let flushed = reporter.wait_for_any_finished(HOOK_REPORT_FLUSH).await;
    if !flushed {
        tracing::debug!(
            "hook: no ahma daemon took this command's report within {HOOK_REPORT_FLUSH:?}; \
             the command itself is unaffected"
        );
    }
}

/// Walk up from `start` looking for a `.git` entry (a directory for a normal
/// clone, a file for a worktree or submodule), returning the repo/worktree root when
/// found.
fn find_git_root_or_worktree(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// If `git_root` is a worktree whose `.git` file points to a parent git directory,
/// resolve the main repository root containing that git directory.
fn resolve_worktree_main_repo(git_root: &Path) -> Option<PathBuf> {
    let git_entry = git_root.join(".git");
    if !git_entry.is_file() {
        return None;
    }
    let content = std::fs::read_to_string(&git_entry).ok()?;
    let line = content
        .lines()
        .find(|l| l.trim_start().starts_with("gitdir:"))?;
    let raw_gitdir = line.trim_start()["gitdir:".len()..].trim();
    let gitdir_path = Path::new(raw_gitdir);
    let resolved = if gitdir_path.is_relative() {
        git_root.join(gitdir_path)
    } else {
        gitdir_path.to_path_buf()
    };
    let canon_gitdir = dunce::canonicalize(&resolved).unwrap_or(resolved);
    let parent = canon_gitdir.parent()?;
    let grandparent = parent.parent()?;
    let great_grandparent = grandparent.parent()?;
    if grandparent.file_name().and_then(|n| n.to_str()) == Some(".git") {
        Some(great_grandparent.to_path_buf())
    } else {
        None
    }
}

/// Resolve the sandbox scopes for a hooked command starting from `cwd`.
///
/// In addition to `cwd`, if `cwd` is inside a git repository or git worktree,
/// this discovers the repository root (and any connected worktree / main repo
/// root) so intra-repo builds, target directories, and submodule writes do
/// not fail with unexpected sandbox denials (SPEC R5.2.1).
pub fn resolve_hook_sandbox_scopes(cwd: &Path) -> Vec<PathBuf> {
    let canon_cwd = dunce::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let home = ahma_common::config::ahma_home_dir();
    let canon_home = home
        .as_deref()
        .map(|h| dunce::canonicalize(h).unwrap_or_else(|_| h.to_path_buf()));

    let mut raw_scopes = Vec::new();
    if let Some(git_root) = find_git_root_or_worktree(&canon_cwd) {
        let canon_git_root = dunce::canonicalize(&git_root).unwrap_or(git_root);
        let is_too_broad = canon_git_root.parent().is_none()
            || canon_home.as_ref().is_some_and(|h| h == &canon_git_root);

        if !is_too_broad {
            if let Some(main_repo) = resolve_worktree_main_repo(&canon_git_root) {
                let canon_main = dunce::canonicalize(&main_repo).unwrap_or(main_repo);
                let main_too_broad = canon_main.parent().is_none()
                    || canon_home.as_ref().is_some_and(|h| h == &canon_main);
                if !main_too_broad {
                    raw_scopes.push(canon_main);
                }
            }
            raw_scopes.push(canon_git_root);
        }
    }
    raw_scopes.push(canon_cwd);

    let mut deduped: Vec<PathBuf> = Vec::new();
    for scope in raw_scopes {
        if !deduped.contains(&scope) {
            deduped.push(scope);
        }
    }

    let mut final_scopes = Vec::new();
    for i in 0..deduped.len() {
        let is_sub = deduped
            .iter()
            .enumerate()
            .any(|(j, other)| i != j && deduped[i] != *other && deduped[i].starts_with(other));
        if !is_sub {
            final_scopes.push(deduped[i].clone());
        }
    }

    if final_scopes.is_empty() {
        vec![cwd.to_path_buf()]
    } else {
        final_scopes
    }
}

async fn run_shell(args: HooksRunShellArgs, cfg: AppConfig) -> Result<()> {
    let payload = args.resolve_payload()?;
    let hook_scopes = resolve_hook_sandbox_scopes(Path::new(&payload.cwd));
    std::env::set_current_dir(&payload.cwd)
        .with_context(|| format!("Failed to change directory to {}", payload.cwd))?;

    let cfg = AppConfig {
        run_tool: Some("run_terminal_command".to_string()),
        run_tool_args: vec![payload.command.clone()],
        skip_availability_probes: true,
        // Sandbox the command in the discovered hook scopes (enclosing git repo
        // or worktree root, plus cwd). This ensures intra-repo builds, target dirs,
        // and shared worktree dependencies do not hit false sandbox denials (SPEC R5.2.1).
        sandbox_scopes: hook_scopes,
        use_scratch_dir: false,
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
    };
    let shell_pool_manager =
        std::sync::Arc::new(crate::shell_pool::ShellPoolManager::new(shell_pool_config));

    let mutex_registry = std::sync::Arc::new(crate::adapter::CommandMutexRegistry::from_config(
        &cfg.mutex_groups,
    ));

    // Report this command to the daemon, so hooked work is visible in the TUI
    // alongside everything else (SPEC R-DAEMON.8). A hook is an instance for the
    // length of one command, which is exactly why it was invisible before: it
    // was always already gone by the time anyone looked.
    //
    // Detached and non-blocking: if no daemon is reachable, the command runs
    // exactly as it would have. This never spawns a daemon — a hook is a
    // latency-sensitive path, and starting one here would put a process launch
    // in front of the user's command.
    crate::daemon_reporter::set_initial_identity(payload.session_id.clone(), None);
    let reporter = crate::daemon_reporter::spawn_reporter(
        operation_monitor.clone(),
        "hook",
        payload.cwd.clone(),
        "hook",
        None,
        None,
    );

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

    // Cancelling the hook must take the whole command tree with it. Dropping the
    // `execute_sync_in_dir` future drops the `ProcessGroupGuard` inside
    // `run_sync_prepared`, whose `Drop` issues `kill(-pgid)` — that is what reaps
    // `sandbox-exec → sh → cargo → cargo-nextest` instead of orphaning it.
    //
    // Then die the way we were asked to. `tokio::signal` replaces the process-wide
    // disposition and never restores it, so simply returning here would leave this
    // process permanently deaf to SIGTERM — a harness trying to clean the hook up
    // would have to escalate to SIGKILL, and a `Ctrl-C` while we were still writing
    // output would do nothing. Restoring `SIG_DFL` and re-raising exits with the
    // conventional `128 + signo` status instead.
    #[cfg(unix)]
    let result = {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint = signal(SignalKind::interrupt()).ok();
        let mut sigterm = signal(SignalKind::terminate()).ok();

        tokio::select! {
            res = adapter.execute_sync_in_dir(
                crate::shell_pool::platform_shell_program(),
                Some(adapter_args),
                &payload.cwd,
                timeout,
                Some(&subcommand_config),
            ) => res,
            _ = async {
                match &mut sigint {
                    Some(s) => s.recv().await,
                    None => std::future::pending().await,
                }
            } => die_by_signal(libc::SIGINT),
            _ = async {
                match &mut sigterm {
                    Some(s) => s.recv().await,
                    None => std::future::pending().await,
                }
            } => die_by_signal(libc::SIGTERM),
        }
    };
    #[cfg(not(unix))]
    let result = adapter
        .execute_sync_in_dir(
            crate::shell_pool::platform_shell_program(),
            Some(adapter_args),
            &payload.cwd,
            timeout,
            Some(&subcommand_config),
        )
        .await;

    // The command is done; give its terminal event a brief moment to reach the
    // daemon before this process exits. Bounded and short (SPEC R-DAEMON.8): a
    // user's command must never wait on observability, so a daemon that is
    // absent or slow costs this much and no more.
    flush_hook_report(reporter).await;

    match result {
        Ok(output) => {
            // Not `println!` (SPEC R5.6.1): the hook's stdout is a pipe to the
            // editor's hook engine, not a terminal, and `println!` panics
            // unconditionally on a write error — a broken pipe here (Windows OS
            // error 232, EPIPE on Unix) would kill the hook with a stack trace
            // instead of a log line. This is the same reasoning the file already
            // applies to `write_exec_output` a few hundred lines up.
            crate::utils::stdio::emit_stdout_text(&format!("{output}\n"))?;
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
        Err(e) => report_shell_execution_error(e),
    }
}

/// Terminate this process with `signo` the way it would have terminated had the
/// signal never been intercepted.
///
/// `tokio::signal` installs a process-wide handler that it never uninstalls, so
/// a hook that merely *returns* after catching SIGINT/SIGTERM stays deaf to them
/// for the rest of its life. Restoring `SIG_DFL` and re-raising gives the parent
/// the conventional "died from signal N" status and keeps `Ctrl-C` working for
/// whatever the hook does next.
///
/// Diverges — the return type only exists so this can sit in a `select!` arm
/// alongside the command's own `Result`.
#[cfg(unix)]
fn die_by_signal(signo: i32) -> Result<String> {
    // SAFETY: both calls are async-signal-safe libc primitives operating on this
    // process's own disposition; `raise` does not return for a default-fatal
    // signal, and the `unreachable` below covers the case where it somehow does.
    unsafe {
        libc::signal(signo, libc::SIG_DFL);
        libc::raise(signo);
    }
    unreachable!("raising signal {signo} with SIG_DFL restored must terminate the process")
}

/// Turn a failed hooked-shell-command execution into its final `Result`.
///
/// Rung 3 of the question ladder, in a terminal (SPEC R-PERM.3 / R-PERM.6.1). A
/// hooked command has no MCP session of its own, so when the sandbox blocks it
/// the user's only surface is the terminal they are already looking at. It must
/// therefore say what was denied and exactly how to allow it — never a bare
/// `Operation not permitted` buried in a build log, which is the failure mode
/// that made hooks feel like a wall rather than a boundary.
///
/// Both denial shapes are covered: a *runtime* denial (the kernel blocked the
/// write mid-command) and a *pre-exec* denial (the path was rejected before the
/// command ran). The second used to fall through as a raw error — a denial the
/// user could see but not act on.
fn report_shell_execution_error(e: anyhow::Error) -> Result<()> {
    let Some(sandbox_err) = e.downcast_ref::<crate::sandbox::SandboxError>() else {
        return Err(e);
    };
    let remediation = match sandbox_err {
        crate::sandbox::SandboxError::RuntimeDenial { path, access, .. } => {
            Some(crate::sandbox::grant_channel::runtime_denial_remediation_cli(path, *access))
        }
        crate::sandbox::SandboxError::PathOutsideSandbox { path, .. } => Some(
            crate::sandbox::grant_channel::runtime_denial_remediation_cli(
                path,
                ahma_common::config::ScopeAccess::Rw,
            ),
        ),
        _ => None,
    };
    let Some(remediation) = remediation else {
        return Err(e);
    };
    // The grant applies to the **next command**: each hooked command spawns a
    // fresh `ahma hooks run-shell` that re-reads the ledger, so there is no
    // server to restart. Say so — it is the difference between "fix this
    // later" and "fix this now".
    let msg = format!(
        "{e}\n\n{remediation}\n\nThe grant takes effect on your next command \
         — terminal hooks re-read it each time, so nothing needs restarting."
    );
    eprintln!("{msg}");
    Err(anyhow!("{msg}"))
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
    let cwd = std::env::current_dir().context("Failed to get current dir")?;

    for ancestor in cwd.ancestors() {
        if ancestor.join(".git").exists()
            || ancestor.join(".cursor").exists()
            || ancestor.join(".vscode").exists()
        {
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
    let Some(raw_args) = input
        .get("tool_input")
        .or_else(|| input.get("toolArgs"))
        .or_else(|| input.get("toolCall").and_then(|tc| tc.get("args")))
        .or_else(|| input.get("tool_call").and_then(|tc| tc.get("args")))
    else {
        // Not a shell tool invocation — allow through without modification
        return Ok(None);
    };

    let args_val = parse_raw_tool_args(raw_args)?;
    let args_obj = args_val
        .as_object()
        .ok_or_else(|| anyhow!("Tool arguments must be a JSON object"))?;

    let Some((command, key)) = extract_command_and_key(args_obj) else {
        return Ok(None);
    };

    Ok(Some(ExtractedToolArgs {
        tool_input: args_obj.clone(),
        command: command.to_string(),
        arg_key: key.to_string(),
    }))
}

fn parse_raw_tool_args(raw_args: &Value) -> Result<Value> {
    if let Some(s) = raw_args.as_str() {
        serde_json::from_str(s).context("Failed to parse toolArgs JSON string")
    } else {
        Ok(raw_args.clone())
    }
}

fn extract_command_and_key(args_obj: &Map<String, Value>) -> Option<(&str, &str)> {
    if let Some(c) = args_obj.get("command").and_then(Value::as_str) {
        Some((c, "command"))
    } else if let Some(c) = args_obj.get("CommandLine").and_then(Value::as_str) {
        Some((c, "CommandLine"))
    } else {
        None
    }
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

    // Editors that carry a session id in the hook input (Claude Code does) let
    // the hooked command be grouped with the session that caused it.
    let session_id = input
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    match build_wrapped_shell_command(scope, env, &cwd, &args.command, session_id) {
        Ok(wrapped) => {
            let updated = updated_tool_input(&args.tool_input, wrapped.clone(), &args.arg_key);
            HooksDecision::AllowRewrite {
                updated_input: updated,
                wrapped_command: wrapped,
            }
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
        HookPlatform::Antigravity => build_antigravity_hook_output(decision),
        HookPlatform::Claude | HookPlatform::Codex | HookPlatform::Copilot => {
            build_structured_hook_output(decision)
        }
    }
}

fn extract_command_cwd(input: &Value, tool_input: &Map<String, Value>) -> Result<String> {
    if let Some(cwd) = tool_input
        .get("working_directory")
        .and_then(Value::as_str)
        .or_else(|| tool_input.get("Cwd").and_then(Value::as_str))
        .or_else(|| tool_input.get("cwd").and_then(Value::as_str))
        .or_else(|| input.get("cwd").and_then(Value::as_str))
        .or_else(|| {
            input
                .get("workspacePaths")
                .and_then(|p| p.get(0))
                .and_then(Value::as_str)
        })
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

// NOTE: unlike `build_structured_hook_output` (Claude/Codex/Copilot), this
// function still returns `"permission": "allow"` on every non-deny branch,
// including the two (`AllowWithWarning`, `DeferToHost`) where ahma is not
// actually the control for the call. That mirrors a real over-approval bug
// found in Claude Code's `permissionDecision` field, but fixing it here is
// deliberately deferred: this codebase has no verified evidence of Cursor's
// own `preToolUse` contract supporting an "ask"/undecided outcome the way
// Claude Code's optional `permissionDecision` does (only the Antigravity
// `command(<regex>)` allow-cache behavior is verified against the shipped
// binary, per SPEC.md R5.4.2) — omitting `permission` here could as easily
// default-deny or error as fall through to a prompt. Fix once that contract
// is confirmed from Cursor's own documentation or a captured session.
fn build_cursor_hook_output(decision: HooksDecision) -> Value {
    match decision {
        HooksDecision::AllowUnchanged => json!({"permission": "allow"}),
        HooksDecision::AllowRewrite { updated_input, .. } => json!({
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

// NOTE: same deferral as `build_cursor_hook_output` above — agy's own
// `PreToolUse` contract has only been reverse-engineered for the
// `permissionOverrides`/allow-cache behavior (SPEC.md R5.4.2), not for
// whether an undecided `decision` falls through to agy's own prompt rather
// than defaulting to deny. Don't drop `"decision": "allow"` here without
// confirming that first.
fn build_antigravity_hook_output(decision: HooksDecision) -> Value {
    match decision {
        HooksDecision::AllowUnchanged => json!({
            "decision": "allow",
        }),
        HooksDecision::AllowRewrite {
            updated_input,
            wrapped_command,
        } => json!({
            "decision": "allow",
            "overwrite": updated_input,
            // R5.4.2 correction of record: agy's `command(...)` allow-cache keys
            // on the exact literal command, but every ahma-wrapped invocation
            // carries a unique `--payload-base64` blob. Without this, agy
            // re-prompts for every distinct underlying command, forever — a
            // rewriting hook is expected to self-register a pattern covering
            // its own rewrites via `permissionOverrides` (agy's own PreToolUse
            // contract), rather than relying on the user's "always allow" cache
            // (which keys on the same unique-per-call string and never matches
            // twice).
            "permissionOverrides": [
                build_antigravity_permission_override(&wrapped_command),
                "command(ahma hooks run-shell)".to_string(),
                "command(regex:.*ahma.* hooks run-shell)".to_string(),
            ],
        }),
        HooksDecision::AllowWithWarning { user_message, .. } => json!({
            "decision": "allow",
            "reason": user_message,
        }),
        HooksDecision::DeferToHost { user_message, .. } => json!({
            "decision": "allow",
            "reason": user_message,
        }),
        HooksDecision::DenyPendingConsent { user_message, .. } => json!({
            "decision": "deny",
            "reason": user_message,
        }),
    }
}

/// Builds an agy `permissionOverrides` grant (`command(<prefix>)`) that
/// matches ANY ahma-wrapped shell command.
///
/// In agy, `commandutils.MatchesConfig` decomposes commands into words (`argv`)
/// and performs prefix token matching. A grant of `command(/path/to/ahma hooks run-shell)`
/// matches any command whose first three words match, covering all underlying subcommands
/// and flags without requiring regex escapes or trailing `.*`.
fn build_antigravity_permission_override(wrapped_command: &str) -> String {
    let trimmed = wrapped_command.trim();
    let (exe, rest) = if let Some(stripped) = trimmed.strip_prefix('\'') {
        if let Some(end) = stripped.find('\'') {
            (&stripped[..end], stripped[end + 1..].trim_start())
        } else {
            (trimmed, "")
        }
    } else if let Some(stripped) = trimmed.strip_prefix('"') {
        if let Some(end) = stripped.find('"') {
            (&stripped[..end], stripped[end + 1..].trim_start())
        } else {
            (trimmed, "")
        }
    } else if let Some((first, remainder)) = trimmed.split_once(char::is_whitespace) {
        (first, remainder.trim_start())
    } else {
        (trimmed, "")
    };

    let rest_parts: Vec<&str> = rest.split_whitespace().collect();
    if rest_parts.len() >= 2
        && rest_parts[0].trim_matches('\'').trim_matches('"') == "hooks"
        && rest_parts[1].trim_matches('\'').trim_matches('"') == "run-shell"
    {
        return format!("command({exe} hooks run-shell)");
    }
    format!("command({trimmed})")
}

fn build_structured_hook_output(decision: HooksDecision) -> Value {
    // `permissionDecision: "allow"` is an explicit grant that bypasses Claude
    // Code's own permission system (settings.json rules, then a prompt) for
    // this call. That is correct ONLY when ahma itself is the substitute
    // control — `AllowRewrite`, where the command now runs inside ahma's own
    // sandbox and the wrapped form is opaque enough that re-prompting on it
    // every time would be pure friction — and for `DenyPendingConsent`, an
    // explicit refusal. Every other branch means ahma is NOT the control for
    // this call (hooks inactive or already-wrapped, a fail-open with no
    // sandbox at all under R5.5.3 session consent, or a host sandbox doing the
    // enforcing instead) and must omit the field so Claude Code's normal
    // permission flow decides, rather than being silently force-approved.
    let hook_specific = match decision {
        HooksDecision::AllowUnchanged => json!({
            "hookEventName": "PreToolUse",
        }),
        HooksDecision::AllowRewrite { updated_input, .. } => json!({
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "updatedInput": updated_input.clone(),
            "modifiedArgs": updated_input,
        }),
        // Fail open (R5.5.3): the command is running with NO ahma sandbox at
        // all, under one-time session consent — surface the warning, but let
        // Claude Code's own permission system still have its say.
        HooksDecision::AllowWithWarning {
            user_message,
            agent_message,
        } => json!({
            "hookEventName": "PreToolUse",
            "agentMessage": agent_message,
            "systemMessage": user_message,
        }),
        // Deferred to the host sandbox (R7): the host, not ahma, is the
        // control here — disclose loudly, but don't also force-approve.
        HooksDecision::DeferToHost {
            user_message,
            agent_message,
        } => json!({
            "hookEventName": "PreToolUse",
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
    session_id: Option<String>,
) -> Result<String> {
    let mut args = vec![
        "hooks".to_string(),
        "run-shell".to_string(),
        "--wrapped-by".to_string(),
        WRAPPED_BY_MARKER.to_string(),
        "--cwd".to_string(),
        cwd.to_string(),
    ];
    if let Some(ref sid) = session_id {
        args.push("--session-id".to_string());
        args.push(sid.clone());
    }
    args.push("--command".to_string());
    args.push(command.to_string());

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
        HookPlatform::Antigravity => install_antigravity_hook(document, platform, scope, env),
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
    let Some(entry) = managed_group_entry(platform, scope, env) else {
        anyhow::bail!(
            "{} does not use the grouped hook format; it has its own installer. This is a \
             wiring bug in ahma, not a problem with your configuration — please report it.",
            platform.label()
        );
    };
    let entries = ensure_child_array(hooks, platform.event_key())?;
    entries.retain(|entry| !is_managed_group_entry(entry));
    entries.push(entry);
    Ok(())
}

fn install_antigravity_hook(
    document: &mut Value,
    platform: HookPlatform,
    scope: HookScope,
    env: &HookEnvironment,
) -> Result<()> {
    install_grouped_hook(document, platform, scope, env)?;
    if scope == HookScope::User {
        let _ = ensure_antigravity_global_permission_grants(env);
    }
    Ok(())
}

fn ensure_antigravity_permissions_in_file(
    file_path: &Path,
    env: &HookEnvironment,
    use_user_settings_wrapper: bool,
) -> Result<()> {
    let mut doc: Value = if file_path.exists() {
        let content = std::fs::read_to_string(file_path)?;
        serde_json::from_str(&content).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };

    let root = ensure_root_object(&mut doc)?;
    let raw_exe = env.current_exe.display().to_string();

    let clean_grants = vec![
        format!("command({raw_exe} hooks run-shell)"),
        "command(ahma hooks run-shell)".to_string(),
        "command(regex:.*ahma.* hooks run-shell)".to_string(),
        format!("command({raw_exe} hooks run-shell --wrapped-by {WRAPPED_BY_MARKER})"),
        format!("command(ahma hooks run-shell --wrapped-by {WRAPPED_BY_MARKER})"),
        format!("command(regex:.*ahma.* hooks run-shell --wrapped-by {WRAPPED_BY_MARKER})"),
    ];

    let mut modified = false;

    let filter_and_add = |entries: &mut Vec<Value>| -> bool {
        let mut changed = false;
        let len_before = entries.len();
        entries.retain(|v| {
            if let Some(s) = v.as_str() {
                if s.contains("regex:") {
                    return true;
                }
                let is_bloated = s.contains("hooks")
                    && s.contains("run-shell")
                    && (s.contains("--command")
                        || s.contains("--payload-base64")
                        || s.contains("--cwd"));
                let is_malformed = s.contains("ahma")
                    && (s.contains(".*") || s.contains(r"\.") || s.contains('\n'));
                if is_bloated || is_malformed {
                    return false;
                }
            }
            true
        });
        if entries.len() != len_before {
            changed = true;
        }

        for grant in &clean_grants {
            let val = Value::String(grant.clone());
            if !entries.contains(&val) {
                entries.push(val);
                changed = true;
            }
        }
        changed
    };

    if use_user_settings_wrapper {
        let user_settings = ensure_child_object(root, "userSettings")?;
        let global_grants = ensure_child_object(user_settings, "globalPermissionGrants")?;
        let allow_entries = ensure_child_array(global_grants, "allow")?;
        if filter_and_add(allow_entries) {
            modified = true;
        }
    }

    let permissions = ensure_child_object(root, "permissions")?;
    let allow_entries = ensure_child_array(permissions, "allow")?;
    if filter_and_add(allow_entries) {
        modified = true;
    }

    if modified {
        if let Some(parent) = file_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let formatted = serde_json::to_string_pretty(&doc)?;
        std::fs::write(file_path, formatted)?;
    }

    Ok(())
}

fn remove_antigravity_permissions(env: &HookEnvironment) -> Result<()> {
    let cli_path = env
        .home_dir
        .join(".gemini")
        .join("antigravity-cli")
        .join("settings.json");
    let config_path = env
        .home_dir
        .join(".gemini")
        .join("config")
        .join("config.json");
    for path in [&cli_path, &config_path] {
        if !path.exists() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(mut doc) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        let mut modified = false;
        if let Some(obj) = doc.as_object_mut() {
            if let Some(perms) = obj.get_mut("permissions").and_then(|p| p.as_object_mut())
                && let Some(allow) = perms.get_mut("allow").and_then(|a| a.as_array_mut())
            {
                let len_before = allow.len();
                allow.retain(|v| {
                    if let Some(s) = v.as_str()
                        && s.starts_with("command(")
                        && s.contains("ahma")
                    {
                        return false;
                    }
                    true
                });
                if allow.len() != len_before {
                    modified = true;
                }
            }
            if let Some(user_settings) = obj.get_mut("userSettings").and_then(|u| u.as_object_mut())
                && let Some(gpg) = user_settings
                    .get_mut("globalPermissionGrants")
                    .and_then(|g| g.as_object_mut())
                && let Some(allow) = gpg.get_mut("allow").and_then(|a| a.as_array_mut())
            {
                let len_before = allow.len();
                allow.retain(|v| {
                    if let Some(s) = v.as_str()
                        && s.starts_with("command(")
                        && s.contains("ahma")
                    {
                        return false;
                    }
                    true
                });
                if allow.len() != len_before {
                    modified = true;
                }
            }
        }
        if modified && let Ok(formatted) = serde_json::to_string_pretty(&doc) {
            let _ = std::fs::write(path, formatted);
        }
    }
    Ok(())
}

fn ensure_antigravity_global_permission_grants(env: &HookEnvironment) -> Result<()> {
    // 1. Antigravity CLI: ~/.gemini/antigravity-cli/settings.json
    let cli_path = env
        .home_dir
        .join(".gemini")
        .join("antigravity-cli")
        .join("settings.json");
    let _ = ensure_antigravity_permissions_in_file(&cli_path, env, false);

    // 2. Global Gemini config: ~/.gemini/config/config.json
    let config_path = env
        .home_dir
        .join(".gemini")
        .join("config")
        .join("config.json");
    let _ = ensure_antigravity_permissions_in_file(&config_path, env, true);

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
/// `<banner>`" on every shell command. Codex, Claude and Antigravity share this
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

/// The managed hook entry for a platform that uses the *grouped* hook format.
///
/// `None` for a platform that does not, which today is Cursor and Copilot — they
/// have their own installers (`install_cursor_hook`, `install_copilot_hook`).
///
/// A `Result`-shaped answer rather than `unreachable!`, because the premise is a
/// claim about *external* tools' file formats. `unreachable!` is right for an
/// invariant this code enforces; here the invariant belongs to editors that ship
/// on their own schedule, and a new `HookPlatform` routed through the grouped
/// installer would have panicked the CLI on a wrong guess about somebody else's
/// JSON.
fn managed_group_entry(
    platform: HookPlatform,
    scope: HookScope,
    env: &HookEnvironment,
) -> Option<Value> {
    match platform {
        HookPlatform::Claude => Some(single_command_group_entry(
            platform,
            scope,
            env,
            "Bash",
            "Routing Bash through ahma",
        )),
        HookPlatform::Antigravity => Some(single_command_group_entry(
            platform,
            scope,
            env,
            "run_command",
            "Routing run_command through ahma",
        )),
        HookPlatform::Codex => Some(codex_group_entry(scope, env)),
        HookPlatform::Cursor | HookPlatform::Copilot => None,
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
        assert_eq!(output["decision"].as_str(), Some("allow"));
        let updated = &output["overwrite"];
        let command = updated["CommandLine"].as_str().unwrap();
        assert!(command.starts_with("ahma hooks run-shell"));
        assert!(command.contains(WRAPPED_BY_MARKER));
        assert_eq!(
            updated["description"].as_str(),
            Some("Run specific test binary")
        );
    }

    #[test]
    fn test_exec_response_supports_antigravity_toolcall_input() {
        let env = test_env();
        let input = json!({
            "toolCall": {
                "name": "run_command",
                "args": {
                    "CommandLine": "echo hello",
                    "Cwd": "/tmp/project"
                }
            },
            "stepIdx": 1,
            "workspacePaths": ["/tmp/project"]
        });

        let decision =
            compute_exec_decision_internal(&input, HookScope::Project, &env, true, false, None);
        let output = build_exec_output(decision, HookPlatform::Antigravity);
        assert_eq!(output["decision"].as_str(), Some("allow"));
        let updated = &output["overwrite"];
        let command = updated["CommandLine"].as_str().unwrap();
        assert!(command.starts_with("ahma hooks run-shell"));
        assert!(command.contains(WRAPPED_BY_MARKER));
    }

    /// R5.4.2 correction of record: agy's `PreToolUse` `command(...)` allow-cache
    /// keys on the exact literal command, but every ahma-wrapped invocation
    /// carries a unique `--payload-base64` blob (it encodes the underlying
    /// command). Without a wildcard `permissionOverrides` entry, the user is
    /// re-prompted for every distinct command they run, forever. Verified via
    /// agy's own embedded `PreToolUse` contract docs and `regexp.QuoteMeta`/
    /// `regexp.Compile`/`MatchString` symbols in the shipped `agy` binary, plus
    /// a real `command(\./generate-swift-bindings\.sh)` (regex-escaped) entry
    /// found in a live `~/.gemini/antigravity-cli/settings.json`.
    #[test]
    fn test_antigravity_permission_override_matches_any_payload() {
        let env = test_env();
        let make_output = |command: &str| {
            let input = json!({
                "cwd": "/tmp/project",
                "tool_input": { "CommandLine": command }
            });
            let decision =
                compute_exec_decision_internal(&input, HookScope::Project, &env, true, false, None);
            build_exec_output(decision, HookPlatform::Antigravity)
        };

        let output_a = make_output("cargo test");
        let output_b = make_output("cargo build --release");

        let overrides = output_a["permissionOverrides"]
            .as_array()
            .expect("Antigravity rewrite must self-register a permission override");
        assert!(!overrides.is_empty());
        let pattern = overrides[0].as_str().unwrap();
        assert_eq!(pattern, "command(ahma hooks run-shell)");

        // Canonical grants are present:
        assert!(
            overrides
                .iter()
                .any(|v| v.as_str() == Some("command(ahma hooks run-shell)"))
        );
        assert!(
            overrides
                .iter()
                .any(|v| v.as_str() == Some("command(regex:.*ahma.* hooks run-shell)"))
        );

        let wrapped_a = output_a["overwrite"]["CommandLine"].as_str().unwrap();
        let wrapped_b = output_b["overwrite"]["CommandLine"].as_str().unwrap();
        assert_ne!(
            wrapped_a, wrapped_b,
            "the two commands must actually produce distinct payloads"
        );
        assert!(wrapped_a.starts_with("ahma hooks run-shell"));
        assert!(wrapped_b.starts_with("ahma hooks run-shell"));

        // The pattern itself must be identical across calls — a shifting pattern
        // would never accumulate into a durable allow rule.
        assert_eq!(output_b["permissionOverrides"][0].as_str(), Some(pattern));
    }

    #[test]
    fn test_antigravity_permission_override_covers_absolute_binary_scope() {
        let env = test_env();
        let input = json!({
            "cwd": "/tmp/project",
            "tool_input": { "CommandLine": "cargo test" }
        });
        let decision =
            compute_exec_decision_internal(&input, HookScope::User, &env, true, false, None);
        let output = build_exec_output(decision, HookPlatform::Antigravity);

        let overrides = output["permissionOverrides"].as_array().unwrap();
        let pattern = overrides[0].as_str().unwrap();
        let raw_exe = env.current_exe.display().to_string();
        assert_eq!(pattern, &format!("command({raw_exe} hooks run-shell)"));

        let wrapped = output["overwrite"]["CommandLine"].as_str().unwrap();
        assert!(wrapped.contains(&env.current_exe.to_string_lossy().to_string()));
    }

    #[test]
    fn test_exec_response_allows_non_shell_tools_without_warning() {
        // Non-shell tools (editFiles, createFile, etc.) have no `command` field,
        // so they resolve to `AllowUnchanged` — ahma isn't controlling this
        // call at all. The hook must exit 0 and never deny; it must NOT also
        // force-approve on VS Code's behalf (`permissionDecision` omitted), so
        // VS Code's own permission flow decides.
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
            assert!(
                output["hookSpecificOutput"]
                    .get("permissionDecision")
                    .is_none(),
                "{tool_name} should not be force-approved by ahma"
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
        assert!(
            output["hookSpecificOutput"]
                .get("permissionDecision")
                .is_none()
        );
    }

    #[test]
    fn test_wrapped_payload_round_trips() {
        let payload = WrappedShellPayload {
            cwd: "/tmp/work".to_string(),
            command: "echo hello && cargo test".to_string(),
            session_id: Some("cc-session-1".to_string()),
        };
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        let decoded = decode_wrapped_shell_payload(&encoded).unwrap();
        assert_eq!(decoded, payload);
    }

    /// A command wrapped by an older ahma is base64 in someone's shell history
    /// or editor config, with no version to negotiate: it must still decode.
    #[test]
    fn a_payload_without_the_session_field_still_decodes() {
        let legacy = URL_SAFE_NO_PAD.encode(
            serde_json::json!({"cwd": "/tmp/work", "command": "echo hi"})
                .to_string()
                .as_bytes(),
        );
        let decoded = decode_wrapped_shell_payload(&legacy).expect("older payloads still decode");
        assert_eq!(decoded.cwd, "/tmp/work");
        assert_eq!(decoded.command, "echo hi");
        assert_eq!(decoded.session_id, None);
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
    fn test_any_managed_hooks_installed() {
        // Just verify calling the public entry point returns a valid Result
        let _ = any_managed_hooks_installed(HookScope::Project);
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
    static ENV_LOCK: std::sync::LazyLock<parking_lot::Mutex<()>> =
        std::sync::LazyLock::new(|| parking_lot::Mutex::new(()));

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
        let out = build_cursor_hook_output(HooksDecision::AllowRewrite {
            updated_input: updated,
            wrapped_command: "wrapped".to_string(),
        });
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
    //
    // Only `AllowRewrite` (ahma is substituting its own sandbox for the call)
    // and `DenyPendingConsent` carry an explicit `permissionDecision`. Claude
    // Code treats an explicit `"allow"` as bypassing its own permission system
    // for that call (settings.json rules, then prompt) — appropriate only when
    // ahma is actually providing the control. `AllowUnchanged` (hooks inactive,
    // or the command is already wrapped), `AllowWithWarning` (the fail-open
    // path after session consent — genuinely running with NO sandbox at all,
    // R5.5.3), and `DeferToHost` (a host sandbox is the control, not ahma) must
    // omit it so Claude Code's own permission flow runs instead of being
    // silently force-approved.
    // ----------------------------------------------------------------------
    #[test]
    fn test_build_structured_hook_output_allow_unchanged() {
        let out = build_structured_hook_output(HooksDecision::AllowUnchanged);
        let hs = &out["hookSpecificOutput"];
        assert_eq!(hs["hookEventName"].as_str(), Some("PreToolUse"));
        assert!(
            hs.get("permissionDecision").is_none(),
            "AllowUnchanged must not force-approve: ahma isn't controlling this call \
             (hooks inactive, or the command is already wrapped) — got {hs:?}"
        );
    }

    #[test]
    fn test_build_structured_hook_output_allow_rewrite_sets_both_keys() {
        let updated = json!({"command": "wrapped"});
        let out = build_structured_hook_output(HooksDecision::AllowRewrite {
            updated_input: updated,
            wrapped_command: "wrapped".to_string(),
        });
        let hs = &out["hookSpecificOutput"];
        assert_eq!(hs["updatedInput"]["command"].as_str(), Some("wrapped"));
        assert_eq!(hs["modifiedArgs"]["command"].as_str(), Some("wrapped"));
        // The one branch where explicit allow is earning its keep: ahma IS the
        // substitute sandbox here, and the wrapped command is opaque enough
        // that re-prompting on it every time would be pure friction.
        assert_eq!(hs["permissionDecision"].as_str(), Some("allow"));
    }

    #[test]
    fn test_build_structured_hook_output_allow_with_warning() {
        let out = build_structured_hook_output(HooksDecision::AllowWithWarning {
            user_message: "sys".to_string(),
            agent_message: "agent".to_string(),
        });
        let hs = &out["hookSpecificOutput"];
        assert!(
            hs.get("permissionDecision").is_none(),
            "AllowWithWarning is the fail-open path — the command is running with \
             NO ahma sandbox at all, so Claude Code's own permission system must \
             still run rather than being force-approved — got {hs:?}"
        );
        assert_eq!(hs["agentMessage"].as_str(), Some("agent"));
        assert_eq!(hs["systemMessage"].as_str(), Some("sys"));
    }

    #[test]
    fn test_build_structured_hook_output_defer_to_host() {
        let out = build_structured_hook_output(HooksDecision::DeferToHost {
            user_message: "sys".to_string(),
            agent_message: "agent".to_string(),
        });
        let hs = &out["hookSpecificOutput"];
        assert!(
            hs.get("permissionDecision").is_none(),
            "DeferToHost means the HOST sandbox is the control, not ahma — ahma \
             must not also force-approve on top of it — got {hs:?}"
        );
        assert_eq!(hs["agentMessage"].as_str(), Some("agent"));
        assert_eq!(hs["systemMessage"].as_str(), Some("sys"));
    }

    #[test]
    fn test_build_structured_hook_output_deny_pending_consent_still_denies() {
        let out = build_structured_hook_output(HooksDecision::DenyPendingConsent {
            user_message: "sys".to_string(),
            agent_message: "agent".to_string(),
        });
        let hs = &out["hookSpecificOutput"];
        assert_eq!(hs["permissionDecision"].as_str(), Some("deny"));
        assert_eq!(hs["permissionDecisionReason"].as_str(), Some("sys"));
    }

    // ----------------------------------------------------------------------
    // wrapped shell command encode/decode
    // ----------------------------------------------------------------------
    #[test]
    fn test_build_wrapped_shell_command_project_uses_path_lookup() {
        let env = test_env();
        let cmd =
            build_wrapped_shell_command(HookScope::Project, &env, "/work", "cargo build", None)
                .unwrap();
        assert!(cmd.starts_with("ahma hooks run-shell"));
        assert!(cmd.contains("--wrapped-by"));
        assert!(cmd.contains(WRAPPED_BY_MARKER));
        assert!(cmd.contains("--cwd"));
        assert!(cmd.contains("--command"));
    }

    #[test]
    fn test_build_wrapped_shell_command_is_human_readable() {
        let env = test_env();
        let cmd =
            build_wrapped_shell_command(HookScope::Project, &env, "/work/dir", "echo x", None)
                .unwrap();
        assert!(cmd.contains("/work/dir"));
        assert!(cmd.contains("echo x"));
    }

    #[test]
    fn test_hooks_run_shell_args_resolves_human_readable() {
        let args = HooksRunShellArgs {
            payload_base64: None,
            cwd: Some(PathBuf::from("/my/cwd")),
            session_id: Some("session-42".to_string()),
            command: Some("cargo test".to_string()),
            wrapped_by: WRAPPED_BY_MARKER.to_string(),
            raw_command: Vec::new(),
        };
        let payload = args.resolve_payload().unwrap();
        assert_eq!(payload.cwd, "/my/cwd");
        assert_eq!(payload.command, "cargo test");
        assert_eq!(payload.session_id, Some("session-42".to_string()));
    }

    #[test]
    fn test_hooks_run_shell_args_resolves_raw_command_trailing() {
        let args = HooksRunShellArgs {
            payload_base64: None,
            cwd: Some(PathBuf::from("/my/cwd")),
            session_id: None,
            command: None,
            wrapped_by: WRAPPED_BY_MARKER.to_string(),
            raw_command: vec![
                "cargo".to_string(),
                "check".to_string(),
                "-p".to_string(),
                "stat3".to_string(),
            ],
        };
        let payload = args.resolve_payload().unwrap();
        assert_eq!(payload.cwd, "/my/cwd");
        assert_eq!(payload.command, "cargo check -p stat3");
    }

    #[test]
    fn test_hooks_run_shell_args_resolves_base64_backward_compatible() {
        let original = WrappedShellPayload {
            cwd: "/work/old".to_string(),
            command: "echo old".to_string(),
            session_id: None,
        };
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&original).unwrap());
        let args = HooksRunShellArgs {
            payload_base64: Some(encoded),
            cwd: None,
            session_id: None,
            command: None,
            wrapped_by: WRAPPED_BY_MARKER.to_string(),
            raw_command: Vec::new(),
        };
        let payload = args.resolve_payload().unwrap();
        assert_eq!(payload.cwd, "/work/old");
        assert_eq!(payload.command, "echo old");
    }

    #[test]
    fn test_resolve_hook_sandbox_scopes_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("my-repo");
        let sub = repo.join("sub").join("nested");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        let scopes = resolve_hook_sandbox_scopes(&sub);
        let canon_repo = dunce::canonicalize(&repo).unwrap();
        assert_eq!(scopes, vec![canon_repo]);
    }

    #[test]
    fn test_resolve_hook_sandbox_scopes_git_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = tmp.path().join("main-repo");
        let gitdir_worktree = main_repo.join(".git").join("worktrees").join("branch-pass");
        std::fs::create_dir_all(&gitdir_worktree).unwrap();

        let worktree = main_repo
            .join(".claude")
            .join("worktrees")
            .join("branch-pass");
        let worktree_sub = worktree.join("rust");
        std::fs::create_dir_all(&worktree_sub).unwrap();

        // Write worktree .git file pointing to main repo gitdir
        let git_file = worktree.join(".git");
        std::fs::write(
            &git_file,
            format!("gitdir: {}\n", gitdir_worktree.display()),
        )
        .unwrap();

        let scopes = resolve_hook_sandbox_scopes(&worktree_sub);
        let canon_main = dunce::canonicalize(&main_repo).unwrap();
        // Since worktree is inside main_repo, the broadest enclosing scope is main_repo
        assert_eq!(scopes, vec![canon_main]);
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
        let entry = managed_group_entry(HookPlatform::Claude, HookScope::User, &env)
            .expect("Claude uses the grouped hook format");
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
        let entry = managed_group_entry(HookPlatform::Claude, HookScope::Project, &env)
            .expect("Claude uses the grouped hook format");
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
        let entry = managed_group_entry(HookPlatform::Claude, HookScope::User, &env)
            .expect("Claude uses the grouped hook format");
        assert_eq!(entry["matcher"].as_str(), Some("Bash"));
        assert!(is_managed_group_entry(&entry));
    }

    #[test]
    fn test_managed_group_entry_antigravity_matcher_run_command() {
        let env = test_env();
        let entry = managed_group_entry(HookPlatform::Antigravity, HookScope::User, &env)
            .expect("Antigravity uses the grouped hook format");
        assert_eq!(entry["matcher"].as_str(), Some("run_command"));
    }

    /// The two platforms with their own installers return `None` rather than
    /// panicking. Asserted so a future platform added to the grouped path has to
    /// decide which format it uses, instead of finding out from a CLI panic.
    #[test]
    fn platforms_with_their_own_installers_have_no_grouped_entry() {
        let env = test_env();
        for platform in [HookPlatform::Cursor, HookPlatform::Copilot] {
            assert!(
                managed_group_entry(platform, HookScope::User, &env).is_none(),
                "{platform:?} installs its hook through its own writer, not the grouped one"
            );
        }
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
        assert!(matches!(decision, HooksDecision::AllowRewrite { .. }));
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
        assert!(matches!(decision, HooksDecision::AllowRewrite { .. }));
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
    fn test_defer_to_host_structured_output_runs_command_unchanged() {
        // Deferral semantics for the `permissionDecision` field itself are
        // covered by `test_build_structured_hook_output_defer_to_host` above
        // (must be omitted — the host sandbox is the control, not ahma). This
        // test covers the other half: no rewrite happens.
        let out = build_structured_hook_output(HooksDecision::DeferToHost {
            user_message: "msg".to_string(),
            agent_message: "agent".to_string(),
        });
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
        assert!(matches!(decision, HooksDecision::AllowRewrite { .. }));
    }

    // ----------------------------------------------------------------------
    // is_ahma_hooks_active_with_configs / describe_activation env precedence
    // ----------------------------------------------------------------------
    #[test]
    fn test_is_ahma_hooks_active_env_off_overrides_active_configs() {
        let _g = ENV_LOCK.lock();
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
        let _g = ENV_LOCK.lock();
        let _restore = EnvGuard::capture();
        unsafe {
            std::env::set_var("AHMA_HOOKS", "on");
            std::env::remove_var("AHMA_DISABLE_HOOKS");
        }
        assert!(is_ahma_hooks_active_with_configs(&[]));
    }

    #[test]
    fn test_is_ahma_hooks_active_disable_alias() {
        let _g = ENV_LOCK.lock();
        let _restore = EnvGuard::capture();
        unsafe {
            std::env::remove_var("AHMA_HOOKS");
            std::env::set_var("AHMA_DISABLE_HOOKS", "1");
        }
        assert!(!is_ahma_hooks_active_with_configs(&[PathBuf::from("/x")]));
    }

    #[test]
    fn test_describe_activation_env_reasons() {
        let _g = ENV_LOCK.lock();
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
        let _g = ENV_LOCK.lock();
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

    #[test]
    fn test_ensure_antigravity_global_permission_grants() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let gemini_config = home.join(".gemini").join("config");
        std::fs::create_dir_all(&gemini_config).unwrap();
        let config_file = gemini_config.join("config.json");
        std::fs::write(
            &config_file,
            r#"{"userSettings":{"globalPermissionGrants":{"allow":["command(git status)"]}}}"#,
        )
        .unwrap();

        let cli_dir = home.join(".gemini").join("antigravity-cli");
        std::fs::create_dir_all(&cli_dir).unwrap();
        let cli_settings = cli_dir.join("settings.json");
        std::fs::write(
            &cli_settings,
            r#"{"permissions":{"allow":["command(\\./generate-swift-bindings\\.sh)"]}}"#,
        )
        .unwrap();

        let env = HookEnvironment {
            home_dir: home.clone(),
            project_root: tmp.path().join("proj"),
            current_exe: PathBuf::from("/usr/local/bin/ahma"),
        };

        ensure_antigravity_global_permission_grants(&env).unwrap();

        // 1. Check ~/.gemini/config/config.json
        let content = std::fs::read_to_string(&config_file).unwrap();
        let doc: Value = serde_json::from_str(&content).unwrap();
        let allows = doc["userSettings"]["globalPermissionGrants"]["allow"]
            .as_array()
            .unwrap();
        assert!(
            allows
                .iter()
                .any(|v| v.as_str() == Some("command(ahma hooks run-shell)"))
        );
        assert!(
            allows
                .iter()
                .any(|v| v.as_str() == Some("command(/usr/local/bin/ahma hooks run-shell)"))
        );

        // 2. Check ~/.gemini/antigravity-cli/settings.json
        let cli_content = std::fs::read_to_string(&cli_settings).unwrap();
        let cli_doc: Value = serde_json::from_str(&cli_content).unwrap();
        let cli_allows = cli_doc["permissions"]["allow"].as_array().unwrap();
        assert!(
            cli_allows
                .iter()
                .any(|v| v.as_str() == Some("command(\\./generate-swift-bindings\\.sh)"))
        );
        assert!(cli_allows.iter().any(|v| {
            v.as_str()
                .is_some_and(|s| s.contains("ahma-hooks-wrapper-v1"))
        }));

        // 3. Test removal on uninstall
        remove_antigravity_permissions(&env).unwrap();

        let content_after = std::fs::read_to_string(&config_file).unwrap();
        let doc_after: Value = serde_json::from_str(&content_after).unwrap();
        let allows_after = doc_after["userSettings"]["globalPermissionGrants"]["allow"]
            .as_array()
            .unwrap();
        assert!(
            !allows_after
                .iter()
                .any(|v| v.as_str().is_some_and(|s| s.contains("ahma")))
        );
        assert!(
            allows_after
                .iter()
                .any(|v| v.as_str() == Some("command(git status)"))
        );

        let cli_content_after = std::fs::read_to_string(&cli_settings).unwrap();
        let cli_doc_after: Value = serde_json::from_str(&cli_content_after).unwrap();
        let cli_allows_after = cli_doc_after["permissions"]["allow"].as_array().unwrap();
        assert!(
            !cli_allows_after
                .iter()
                .any(|v| v.as_str().is_some_and(|s| s.contains("ahma")))
        );
        assert!(
            cli_allows_after
                .iter()
                .any(|v| v.as_str() == Some("command(\\./generate-swift-bindings\\.sh)"))
        );
    }
}

/// The candidate list is now derived; these tests are what stop the derivation
/// from quietly covering less than the hand-written list it replaced.
#[cfg(test)]
mod mcp_config_candidate_coverage {
    use super::mcp_config_candidates;
    use std::path::{Path, PathBuf};

    /// Every path the previous hand-maintained list probed, transcribed.
    ///
    /// A miss here is not cosmetic: `auto` hook mode decides ahma is inactive
    /// and passes commands through **unsandboxed**. So the replacement is held
    /// to covering the original literally, rather than to looking equivalent.
    const PREVIOUS_LIST: &[&str] = &[
        ".cursor/mcp.json",
        ".claude.json",
        ".codex/config.toml",
        ".gemini/config/mcp_config.json",
        ".lmstudio/mcp.json",
        "Library/Application Support/Code/User/mcp.json",
        ".config/Code/User/mcp.json",
        "AppData/Roaming/Code/User/mcp.json",
        "Library/Application Support/Claude/claude_desktop_config.json",
        ".config/Claude/claude_desktop_config.json",
        "AppData/Roaming/Claude/claude_desktop_config.json",
    ];

    #[test]
    fn every_previously_probed_path_is_still_probed() {
        let home = Path::new("/home/u");
        let got = mcp_config_candidates(home, None);
        for relative in PREVIOUS_LIST {
            let want: PathBuf = relative
                .split('/')
                .fold(home.to_path_buf(), |p, s| p.join(s));
            assert!(
                got.contains(&want),
                "{} is no longer probed; auto hook mode would call ahma inactive there \
                 and pass commands through unsandboxed.\nprobed: {got:#?}",
                want.display()
            );
        }
    }

    #[test]
    fn the_project_local_vscode_config_is_probed_when_a_project_root_is_known() {
        let home = Path::new("/home/u");
        let root = Path::new("/work/repo");
        let without = mcp_config_candidates(home, None);
        let with = mcp_config_candidates(home, Some(root));
        let project = root.join(".vscode").join("mcp.json");
        assert!(!without.contains(&project));
        assert!(with.contains(&project));
    }

    /// The derivation must not probe a path twice — a duplicate would double the
    /// `stat` calls on every hook invocation and make the "active configs"
    /// report list the same file twice.
    #[test]
    fn no_candidate_is_probed_twice() {
        let mut got = mcp_config_candidates(Path::new("/home/u"), None);
        let before = got.len();
        got.sort();
        got.dedup();
        assert_eq!(before, got.len(), "duplicate candidate paths: {got:#?}");
    }
}
