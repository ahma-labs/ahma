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

const MANAGED_ID_DEFAULT_SHELL_V1: &str = "ahma-default-shell-v1";
const WRAPPED_BY_MARKER: &str = "ahma-hooks-wrapper-v1";
const HOOK_TIMEOUT_SECS: u64 = 30;
const PATH_LOOKUP_BINARY: &str = "ahma";

/// Manage terminal hooks for external AI tools.
#[derive(Args, Debug)]
#[command(
    about = "Manage terminal hooks for Cursor, Claude Code, and Codex",
    after_help = "EXAMPLES:
  # Install user-scoped hooks for all supported tools
  ahma hooks install

  # Install project-scoped hooks for Claude Code and Codex
  ahma hooks install --platform claude,codex --scope project

  # Show both user and project hook status
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
    }
}

pub fn run_install(args: HooksInstallArgs) -> Result<()> {
    let env = HookEnvironment::detect()?;

    for platform in selected_platforms(&args.platforms) {
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

fn run_uninstall(args: HooksUninstallArgs) -> Result<()> {
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

fn detect_mcp_config_exists() -> bool {
    let home = match std::env::var("HOME").ok().map(PathBuf::from) {
        Some(h) => h,
        None => return false,
    };

    let mut paths = vec![
        home.join(".cursor").join("mcp.json"),
        home.join("Library/Application Support/Code/User/mcp.json"),
        home.join(".config/Code/User/mcp.json"),
        home.join("Library/Application Support/Claude/claude_desktop_config.json"),
        home.join(".config/Claude/claude_desktop_config.json"),
    ];

    if let Ok(project_root) = detect_project_root() {
        paths.push(project_root.join(".vscode").join("mcp.json"));
    }

    for path in paths {
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if content.contains("\"ahma\"") {
                    return true;
                }
            }
        }
    }
    false
}

fn run_status(args: HooksStatusArgs) -> Result<()> {
    let env = HookEnvironment::detect()?;
    let scopes = match args.scope {
        Some(scope) => vec![scope],
        None => vec![HookScope::User, HookScope::Project],
    };

    println!("{:<14} {:<8} {:<14} Config", "Platform", "Scope", "Status");
    println!("{:-<14} {:-<8} {:-<14} {:-<6}", "", "", "", "");

    let mut installed_count = 0;
    for scope in scopes {
        for platform in selected_platforms(&args.platforms) {
            let path = env.config_path(platform, scope);
            let status = hook_status_string(&path, platform)?;
            if status == "installed" {
                installed_count += 1;
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

    if installed_count > 0 && detect_mcp_config_exists() {
        println!("\n⚠️  WARNING: Redundant terminal hooks + MCP server configuration detected!");
        println!("Both terminal hooks and an MCP server are configured for \"ahma\".");
        println!("This can cause redundant wrapping and execution slowness.");
        println!("RECOMMENDED: Keep only the MCP server and uninstall terminal hooks via:");
        println!("    ahma hooks uninstall --scope user");
        println!("See AGENTS.md or the ahma skill documentation for setup guidelines.\n");
    }

    Ok(())
}

fn run_exec(args: HooksExecArgs) -> Result<()> {
    let env = HookEnvironment::detect()?;
    let stdin = read_stdin_json()?;
    let output = build_exec_response(&stdin, args.platform, args.scope, &env)?;

    let stdout = io::stdout();
    let mut handle = stdout.lock();
    serde_json::to_writer(&mut handle, &output)?;
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
        ..cfg
    };
    let sandbox = crate::shell::cli::initialize_sandbox(&cfg)?
        .ok_or_else(|| anyhow!("Sandbox scopes must be initialized for run-shell mode"))?;

    let monitor_config = crate::operation_monitor::MonitorConfig::with_timeout(std::time::Duration::from_secs(cfg.timeout_secs));
    let operation_monitor = std::sync::Arc::new(crate::operation_monitor::OperationMonitor::new(monitor_config));

    let shell_pool_config = crate::shell_pool::ShellPoolConfig {
        command_timeout: std::time::Duration::from_secs(cfg.timeout_secs),
        ..Default::default()
    };
    let shell_pool_manager = std::sync::Arc::new(crate::shell_pool::ShellPoolManager::new(shell_pool_config));

    let adapter = std::sync::Arc::new(crate::adapter::Adapter::new(
        operation_monitor,
        shell_pool_manager,
        sandbox,
    )?);

    let mut adapter_args = serde_json::Map::new();
    adapter_args.insert("command".to_string(), serde_json::Value::String(payload.command));
    adapter_args.insert("c_flag".to_string(), serde_json::Value::Bool(true));

    let timeout = Some(cfg.timeout_secs);
    let subcommand_config = crate::AhmaMcpService::build_shell_subcommand_config(timeout, &crate::adapter::ExecutionMode::Synchronous);

    let result = adapter.execute_sync_in_dir(
        crate::shell_pool::platform_shell_program(),
        Some(adapter_args),
        &payload.cwd,
        timeout,
        Some(&subcommand_config),
    ).await;

    match result {
        Ok(output) => {
            println!("{}", output);
            Ok(())
        }
        Err(e) => {
            eprintln!("Error executing tool: {}", e);
            Err(anyhow::anyhow!("Tool execution failed"))
        }
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

fn resolve_exec_updated_input(
    input: &Value,
    scope: HookScope,
    env: &HookEnvironment,
) -> Result<Option<Value>> {
    let Some(args) = extract_tool_args(input)? else {
        return Ok(None);
    };
    if is_wrapped_shell_command(&args.command) {
        return Ok(None);
    }
    let cwd = extract_command_cwd(input, &args.tool_input)?;
    let wrapped_command = build_wrapped_shell_command(scope, env, &cwd, &args.command)?;
    Ok(Some(updated_tool_input(
        &args.tool_input,
        wrapped_command,
        &args.arg_key,
    )))
}

fn build_exec_response(
    input: &Value,
    platform: HookPlatform,
    scope: HookScope,
    env: &HookEnvironment,
) -> Result<Value> {
    let updated_input = resolve_exec_updated_input(input, scope, env)?;
    Ok(match platform {
        HookPlatform::Cursor => build_cursor_hook_output(updated_input),
        HookPlatform::Claude
        | HookPlatform::Codex
        | HookPlatform::Copilot
        | HookPlatform::Antigravity => build_structured_hook_output(updated_input),
    })
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

fn build_cursor_hook_output(updated_input: Option<Value>) -> Value {
    let mut object = Map::new();
    object.insert("permission".to_string(), Value::String("allow".to_string()));
    if let Some(updated_input) = updated_input {
        object.insert("updated_input".to_string(), updated_input);
    }
    Value::Object(object)
}

fn build_structured_hook_output(updated_input: Option<Value>) -> Value {
    let mut hook_output = Map::new();
    hook_output.insert(
        "hookEventName".to_string(),
        Value::String("PreToolUse".to_string()),
    );
    hook_output.insert(
        "permissionDecision".to_string(),
        Value::String("allow".to_string()),
    );
    if let Some(updated_input) = updated_input {
        hook_output.insert("updatedInput".to_string(), updated_input.clone());
        hook_output.insert("modifiedArgs".to_string(), updated_input);
    }

    let mut root = Map::new();
    root.insert("hookSpecificOutput".to_string(), Value::Object(hook_output));
    Value::Object(root)
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
        let command = installed["hooks"]["preToolUse"][0]["command"]
            .as_str()
            .unwrap();
        assert!(command.contains(MANAGED_ID_DEFAULT_SHELL_V1));
        assert!(command.contains("bin space"));
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

        let output =
            build_exec_response(&input, HookPlatform::Claude, HookScope::Project, &env).unwrap();
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

        let output =
            build_exec_response(&input, HookPlatform::Antigravity, HookScope::Project, &env)
                .unwrap();
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
            let output =
                build_exec_response(&input, HookPlatform::Copilot, HookScope::User, &env).unwrap();
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
        let output =
            build_exec_response(&input_no_args, HookPlatform::Copilot, HookScope::User, &env)
                .unwrap();
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
}
