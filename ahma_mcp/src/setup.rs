//! Native Rust setup wizard for ahma.
//!
//! Configures global MCP servers, terminal hooks, agent skills, and TLS.

use anyhow::{Context, Result, anyhow};
use serde_json::json;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::hooks::{HookPlatform, HookScope, HooksInstallArgs};
use crate::shell::cli::SetupArgs;

/// The embedded skill content to install globally.
const SKILL_CONTENT: &str = include_str!("../../skills/ahma/SKILL.md");

fn prompt_transport() -> &'static str {
    println!("\nSelect how MCP clients like your IDE or TUI connect to ahma:");
    println!("  1) stdio  (recommended - private ahma instance per project)");
    println!("  2) http   (one shared server over localhost TCP)");
    println!("  3) unix   (one shared server over localhost Unix socket)");
    print!("  Mode [default 1]: ");
    let _ = io::stdout().flush();
    let mut input = String::new();
    let _ = io::stdin().read_line(&mut input);
    match input.trim() {
        "2" => "http",
        "3" => "unix",
        _ => "stdio",
    }
}

/// A thing the wizard can set up. Listed in alphabetical order (by label) for
/// uniform, simple presentation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SetupAction {
    Skills,
    Mcp,
    Hooks,
    Tls,
}

const SETUP_ACTIONS: &[SetupAction] = &[
    SetupAction::Skills, // "Agent skills"
    SetupAction::Mcp,    // "MCP servers"
    SetupAction::Hooks,  // "Terminal hooks"
    SetupAction::Tls,    // "TLS certificates"
];

impl SetupAction {
    fn label(self) -> &'static str {
        match self {
            SetupAction::Skills => "Agent skills",
            SetupAction::Mcp => "MCP servers",
            SetupAction::Hooks => "Terminal hooks",
            SetupAction::Tls => "TLS certificates",
        }
    }

    /// Whether this action is applied per-platform (and therefore needs the
    /// "which platforms?" question). TLS and skills are global.
    fn is_platform_specific(self) -> bool {
        matches!(self, SetupAction::Mcp | SetupAction::Hooks)
    }
}

/// An AI tool the wizard can target. Listed in alphabetical order (by label)
/// for uniform, simple presentation. Not every platform supports every action:
/// GitHub Copilot has no MCP config target here; VS Code, Claude Desktop, and
/// LM Studio are configured via MCP only (no terminal hook wrapper).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Platform {
    Antigravity,
    ClaudeCode,
    ClaudeDesktop,
    Codex,
    Cursor,
    Copilot,
    LmStudio,
    VsCode,
}

const PLATFORMS: &[Platform] = &[
    Platform::Antigravity,
    Platform::ClaudeCode,
    Platform::ClaudeDesktop,
    Platform::Codex,
    Platform::Cursor,
    Platform::Copilot,
    Platform::LmStudio,
    Platform::VsCode,
];

impl Platform {
    fn label(self) -> &'static str {
        match self {
            Platform::Antigravity => "Antigravity",
            Platform::ClaudeCode => "Claude Code",
            Platform::ClaudeDesktop => "Claude Desktop",
            Platform::Codex => "Codex",
            Platform::Cursor => "Cursor",
            Platform::Copilot => "GitHub Copilot CLI",
            Platform::LmStudio => "LM Studio",
            Platform::VsCode => "VS Code (GitHub Copilot Chat)",
        }
    }

    fn supports_mcp(self) -> bool {
        !matches!(self, Platform::Copilot)
    }

    fn supports_hooks(self) -> bool {
        !matches!(
            self,
            Platform::VsCode | Platform::ClaudeDesktop | Platform::LmStudio
        )
    }

    fn hook_platform(self) -> Option<HookPlatform> {
        match self {
            Platform::Antigravity => Some(HookPlatform::Antigravity),
            Platform::ClaudeCode => Some(HookPlatform::Claude),
            Platform::ClaudeDesktop => None,
            Platform::Codex => Some(HookPlatform::Codex),
            Platform::Cursor => Some(HookPlatform::Cursor),
            Platform::Copilot => Some(HookPlatform::Copilot),
            Platform::LmStudio => None,
            Platform::VsCode => None,
        }
    }

    /// Apply MCP server configuration for this platform. Returns the display
    /// name on success, or `None` if there was nothing to configure.
    fn configure_mcp(
        self,
        transport: &str,
        servers_entry: &serde_json::Value,
        scoped_servers_entry: &serde_json::Value,
        home: &Path,
    ) -> Result<Option<&'static str>> {
        match self {
            Platform::VsCode => {
                if let Some(path) = vscode_mcp_path() {
                    merge_mcp_json(&path, "servers", servers_entry.clone())?;
                    return Ok(Some("VS Code (GitHub Copilot Chat)"));
                }
            }
            Platform::ClaudeCode => {
                let path = home.join(".claude.json");
                merge_mcp_json(&path, "mcpServers", servers_entry.clone())?;
                return Ok(Some("Claude Code"));
            }
            Platform::ClaudeDesktop => {
                if let Some(path) = claude_desktop_config_path() {
                    let entry = build_claude_desktop_mcp_entry(transport, home);
                    merge_mcp_json(&path, "mcpServers", entry)?;
                    return Ok(Some("Claude Desktop"));
                }
            }
            Platform::Cursor => {
                let path = home.join(".cursor").join("mcp.json");
                merge_mcp_json(&path, "mcpServers", servers_entry.clone())?;
                return Ok(Some("Cursor"));
            }
            Platform::Antigravity => {
                let path = home.join(".gemini").join("config").join("mcp_config.json");
                merge_mcp_json(&path, "mcpServers", scoped_servers_entry.clone())?;
                return Ok(Some("Antigravity"));
            }
            Platform::LmStudio => {
                // LM Studio reads MCP servers from ~/.lmstudio/mcp.json. Like
                // Antigravity it does not send roots/list, so use the scoped entry.
                let path = home.join(".lmstudio").join("mcp.json");
                merge_mcp_json(&path, "mcpServers", scoped_servers_entry.clone())?;
                return Ok(Some("LM Studio"));
            }
            Platform::Codex => {
                let path = home.join(".codex").join("config.toml");
                let toml_val = build_codex_toml_value(transport);
                merge_codex_toml(&path, toml_val)?;
                return Ok(Some("Codex CLI"));
            }
            Platform::Copilot => {}
        }
        Ok(None)
    }
}

async fn execute_actions(
    actions: &[SetupAction],
    platforms: &[Platform],
    transport: &str,
    interactive: bool,
) -> Result<()> {
    if actions.contains(&SetupAction::Mcp) {
        setup_mcp_config(platforms, transport, interactive).await?;
    }
    if actions.contains(&SetupAction::Hooks) {
        setup_terminal_hooks(platforms, interactive).await?;
    }
    if actions.contains(&SetupAction::Skills) {
        setup_agent_skills(interactive).await?;
    }
    if actions.contains(&SetupAction::Tls) {
        setup_tls()?;
    }
    Ok(())
}

/// Runs the setup wizard.
///
/// Interactively this asks just two questions: **what** to set up (actions),
/// then **where** to apply it (platforms). Only per-platform actions (MCP
/// servers, terminal hooks) trigger the platform question; TLS and skills are
/// global. When MCP is selected, the connection transport is requested as a
/// follow-up detail of that action.
pub async fn run(args: SetupArgs) -> Result<()> {
    let interactive = !args.auto && io::stdin().is_terminal() && io::stdout().is_terminal();

    if interactive {
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("  Ahma Setup Wizard");
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!();
    }

    // Question 1: which actions to perform.
    let actions = select_actions(&args, interactive);
    if actions.is_empty() {
        if interactive {
            println!("Nothing selected — exiting without changes.\n");
        }
        return Ok(());
    }

    // Question 2: which platforms to apply the per-platform actions to.
    let platforms = if actions.iter().any(|a| a.is_platform_specific()) {
        select_platforms(&actions, interactive)
    } else {
        Vec::new()
    };

    // The MCP transport is a required detail of the MCP action only.
    let transport = if actions.contains(&SetupAction::Mcp) && interactive {
        prompt_transport()
    } else {
        "stdio"
    };

    execute_actions(&actions, &platforms, transport, interactive).await?;

    if interactive {
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("  Setup Completed!");
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!();
    }

    Ok(())
}

/// Actions selected when the user accepts the default (presses Enter
/// interactively, or runs non-interactively/`--auto` without other flags).
///
/// Terminal hooks used to be excluded from this default unconditionally. The
/// stated reason was that a sandbox exception might be "classified incorrectly"
/// — but that was never quite the real problem. The real problem was that when
/// the sandbox blocked something the user genuinely wanted, **there was nowhere
/// to ask them**: a hooked command's denial reached no surface at all, so the only
/// available outcomes were "blocked, with no way forward" and "let it through".
/// Hooks looked like they were getting in the way because, lacking a question,
/// they were.
///
/// The question ladder (SPEC R-PERM.3) gives a denial somewhere to go, and the
/// hooks path re-derives its sandbox per command — so a grant applies on the very
/// next command with no restart. With that, readiness stops being a property of
/// *ahma* and becomes a property of each *client*: hooks are installed by default
/// for the clients where that loop is proven (R-PERM.6), and skipped — **with a
/// stated reason** — for the rest.
fn default_setup_actions() -> Vec<SetupAction> {
    SETUP_ACTIONS
        .iter()
        .copied()
        .filter(|a| *a != SetupAction::Hooks || crate::hooks::any_client_ready_for_hooks())
        .collect()
}

/// Renders `actions` as the comma-separated 1-based menu numbers a user would
/// type to select exactly them, e.g. `[Skills, Mcp, Tls]` -> `"1,2,4"`.
fn selection_string_for(actions: &[SetupAction]) -> String {
    SETUP_ACTIONS
        .iter()
        .enumerate()
        .filter(|(_, a)| actions.contains(a))
        .map(|(i, _)| (i + 1).to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Explains what each setup item does in plain language, for users who may
/// not know ahma's internals, before asking which ones to set up.
fn print_setup_action_guidance() {
    println!("What each of these does:");
    println!("  Agent skills     - installs the `/ahma` skill so your AI agent knows how");
    println!("                     to use ahma's tools well.");
    println!("  MCP servers      - registers ahma as a tool provider with your AI tools");
    println!("                     (Claude Code, Cursor, VS Code, etc.).");
    println!("  Terminal hooks   - also reroutes commands your AI tool runs directly in a");
    println!("                     terminal through ahma's sandbox, not just its MCP tool");
    println!("                     calls. NOT selected by default: this is an experimental");
    println!("                     project and sandbox-exception handling isn't fully");
    println!("                     hardened yet, so an automatic hook could interfere with");
    println!("                     your work. Include \"3\" below (or pass --hooks) once");
    println!("                     you want to opt in.");
    println!("  TLS certificates - generates a local TLS certificate for the HTTP bridge.");
    println!();
}

/// Question 1: determine which actions to run.
///
/// Explicit `--mcp`/`--hooks`/`--skills`/`--tls` flags select a fixed subset
/// (for scripting). Otherwise the user is asked interactively, defaulting to
/// every action except terminal hooks (see `default_setup_actions`); `--auto`
/// and non-interactive sessions get that same default without prompting.
fn select_actions(args: &SetupArgs, interactive: bool) -> Vec<SetupAction> {
    let mut flagged = Vec::new();
    if args.skills {
        flagged.push(SetupAction::Skills);
    }
    if args.mcp {
        flagged.push(SetupAction::Mcp);
    }
    if args.hooks {
        flagged.push(SetupAction::Hooks);
    }
    if args.tls {
        flagged.push(SetupAction::Tls);
    }
    if !flagged.is_empty() {
        return flagged;
    }

    let default_actions = default_setup_actions();
    if !interactive {
        return default_actions;
    }

    print_setup_action_guidance();
    let labels: Vec<&str> = SETUP_ACTIONS.iter().map(|a| a.label()).collect();
    let default = selection_string_for(&default_actions);
    let chosen = prompt_multi_select(
        "What would you like to set up? (comma-separated numbers):",
        &labels,
        &default,
    );
    chosen
        .into_iter()
        .filter_map(|i| SETUP_ACTIONS.get(i).copied())
        .collect()
}

/// Question 2: determine which platforms to apply per-platform actions to.
///
/// The offered list is the union of platforms relevant to the selected actions,
/// so unsupported combinations are never shown.
fn select_platforms(actions: &[SetupAction], interactive: bool) -> Vec<Platform> {
    let want_mcp = actions.contains(&SetupAction::Mcp);
    let want_hooks = actions.contains(&SetupAction::Hooks);

    let relevant: Vec<Platform> = PLATFORMS
        .iter()
        .copied()
        .filter(|p| (want_mcp && p.supports_mcp()) || (want_hooks && p.supports_hooks()))
        .collect();

    let labels: Vec<&str> = relevant.iter().map(|p| p.label()).collect();
    let chosen = prompt_multi_select_all(
        interactive,
        "On which platforms? (comma-separated numbers):",
        &labels,
    );
    chosen
        .into_iter()
        .filter_map(|i| relevant.get(i).copied())
        .collect()
}

fn prompt_multi_select(question: &str, options: &[&str], default: &str) -> Vec<usize> {
    println!("{}", question);
    for (i, opt) in options.iter().enumerate() {
        println!("  {}) {}", i + 1, opt);
    }
    print!("  Selection [default: {}]: ", default);
    let _ = io::stdout().flush();
    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() || input.trim().is_empty() {
        return parse_selection_string(default, options.len());
    }
    parse_selection_string(&input, options.len())
}

fn default_all_selection(_count: usize) -> String {
    "all".to_string()
}

fn parse_selection_string(input: &str, max_val: usize) -> Vec<usize> {
    let input_trimmed = input.trim();
    if input_trimmed.eq_ignore_ascii_case("all") {
        return (0..max_val).collect();
    }

    // Check if the input is purely numeric digits without any spaces or other separator characters
    let is_pure_digits =
        !input_trimmed.is_empty() && input_trimmed.chars().all(|c| c.is_ascii_digit());

    if is_pure_digits && max_val < 10 {
        parse_digit_sequence(input_trimmed, max_val)
    } else {
        parse_separated_list(input_trimmed, max_val)
    }
}

fn parse_digit_sequence(input: &str, max_val: usize) -> Vec<usize> {
    let mut selections = Vec::new();
    let valid_indices = input
        .chars()
        .filter_map(|c| c.to_digit(10))
        .map(|d| d as usize)
        .filter(|&n| n >= 1 && n <= max_val)
        .map(|n| n - 1);
    for idx in valid_indices {
        if !selections.contains(&idx) {
            selections.push(idx);
        }
    }
    selections
}

fn parse_separated_list(input: &str, max_val: usize) -> Vec<usize> {
    let mut selections = Vec::new();
    let normalized = input.replace([',', '.', ';'], " ");
    let valid_indices = normalized
        .split_whitespace()
        .filter_map(|part| part.parse::<usize>().ok())
        .filter(|&n| n >= 1 && n <= max_val)
        .map(|n| n - 1);
    for idx in valid_indices {
        if !selections.contains(&idx) {
            selections.push(idx);
        }
    }
    selections
}

/// Prompt a multi-select that defaults to "all" options. Non-interactive
/// sessions select everything without prompting.
fn prompt_multi_select_all(interactive: bool, question: &str, labels: &[&str]) -> Vec<usize> {
    if !interactive {
        return (0..labels.len()).collect();
    }
    let default = default_all_selection(labels.len());
    prompt_multi_select(question, labels, &default)
}

fn mcp_shared_transport_url(transport: &str) -> Option<&'static str> {
    match transport {
        "http" => Some("http://localhost:3000/mcp"),
        "unix" => Some("unix:///tmp/ahma.sock#/mcp"),
        _ => None,
    }
}

fn build_mcp_servers_entry(transport: &str) -> serde_json::Value {
    if let Some(url) = mcp_shared_transport_url(transport) {
        return json!({ "type": "http", "url": url });
    }
    json!({
        "type": "stdio",
        "command": "ahma",
        "args": [
            "serve",
            "stdio",
            "--tools",
            "simplify",
            "--sandbox",
            "--log-monitor"
        ]
    })
}

fn build_scoped_servers_entry(transport: &str, home: &Path) -> serde_json::Value {
    if let Some(url) = mcp_shared_transport_url(transport) {
        return json!({ "url": url });
    }
    // Some clients (Antigravity, LM Studio) don't send MCP roots/list, so we must
    // specify a sandbox scope explicitly.  Use the canonical path (not ~/sandbox)
    // because MCP clients launch processes without shell tilde expansion, and the
    // sandbox directory must exist for canonicalization.
    let sandbox_dir = home.join("sandbox");
    if let Err(e) = std::fs::create_dir_all(&sandbox_dir) {
        tracing::warn!(
            "Could not pre-create sandbox directory {}: {e}",
            sandbox_dir.display()
        );
    }
    let scope_str = sandbox_dir.to_string_lossy().to_string();
    json!({
        "command": "ahma",
        "args": [
            "serve",
            "stdio",
            "--tools",
            "simplify",
            "--sandbox",
            "--log-monitor",
            "--sandbox-scope",
            scope_str
        ]
    })
}

fn claude_desktop_config_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    #[cfg(target_os = "macos")]
    {
        Some(home.join("Library/Application Support/Claude/claude_desktop_config.json"))
    }
    #[cfg(target_os = "windows")]
    {
        Some(home.join("AppData/Roaming/Claude/claude_desktop_config.json"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Some(home.join(".config/Claude/claude_desktop_config.json"))
    }
}

/// Build the MCP entry for Claude Desktop.
///
/// Claude Desktop's `claude_desktop_config.json` uses `mcpServers` with the
/// same `command`/`args` shape as Claude Code but without a `"type"` wrapper
/// field — omitting it ensures compatibility with all Desktop versions.
/// HTTP and Unix transports are passed through as-is for users running a
/// shared ahma server.
fn build_claude_desktop_mcp_entry(transport: &str, _home: &Path) -> serde_json::Value {
    if let Some(url) = mcp_shared_transport_url(transport) {
        return json!({ "type": "http", "url": url });
    }
    json!({
        "command": "ahma",
        "args": [
            "serve",
            "stdio",
            "--tools",
            "simplify",
            "--sandbox",
            "--log-monitor"
        ]
    })
}

fn print_mcp_restart_hints(interactive: bool, configured: &[&str], transport: &str) {
    if !interactive || configured.is_empty() {
        return;
    }
    println!("\n✓ MCP setup complete! Restart these tools to apply changes:");
    for platform in configured {
        println!("    - {}", platform);
    }
    match transport {
        "http" => println!(
            "  Start the HTTP server before opening tools: ahma serve http --tools simplify"
        ),
        "unix" => println!(
            "  Start the Unix socket server before opening tools: ahma serve unix --socket-path /tmp/ahma.sock --tools simplify"
        ),
        _ => {}
    }
    println!();
}

fn vscode_mcp_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    #[cfg(target_os = "macos")]
    {
        Some(home.join("Library/Application Support/Code/User/mcp.json"))
    }
    #[cfg(target_os = "windows")]
    {
        Some(home.join("AppData/Roaming/Code/User/mcp.json"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Some(home.join(".config/Code/User/mcp.json"))
    }
}

async fn setup_mcp_config(
    platforms: &[Platform],
    transport: &str,
    interactive: bool,
) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("Could not resolve home directory"))?;

    let servers_entry = build_mcp_servers_entry(transport);
    let scoped_servers_entry = build_scoped_servers_entry(transport, &home);

    let mut configured = Vec::new();

    for platform in platforms.iter().copied().filter(|p| p.supports_mcp()) {
        if let Some(name) =
            platform.configure_mcp(transport, &servers_entry, &scoped_servers_entry, &home)?
        {
            configured.push(name);
        }
    }

    print_mcp_restart_hints(interactive, &configured, transport);

    Ok(())
}

pub(crate) fn merge_mcp_json(
    path: &Path,
    servers_key: &str,
    value: serde_json::Value,
) -> Result<()> {
    let mut config = if path.exists() {
        let content = std::fs::read_to_string(path)?;
        serde_json::from_str(&content)
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()))
    } else {
        serde_json::Value::Object(serde_json::Map::new())
    };

    if !config.is_object() {
        config = serde_json::Value::Object(serde_json::Map::new());
    }

    let obj = config.as_object_mut().unwrap();
    if !obj.contains_key(servers_key) || !obj.get(servers_key).unwrap().is_object() {
        obj.insert(
            servers_key.to_string(),
            serde_json::Value::Object(serde_json::Map::new()),
        );
    }

    let servers_obj = obj.get_mut(servers_key).unwrap().as_object_mut().unwrap();
    servers_obj.insert("Ahma".to_string(), value);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut file = std::fs::File::create(path)?;
    serde_json::to_writer_pretty(&mut file, &config)?;
    Ok(())
}

fn build_codex_toml_value(transport: &str) -> toml::Value {
    let mut table = toml::map::Map::new();
    match transport {
        "http" => {
            table.insert(
                "url".to_string(),
                toml::Value::String("http://localhost:3000/mcp".to_string()),
            );
        }
        "unix" => {
            table.insert(
                "url".to_string(),
                toml::Value::String("unix:///tmp/ahma.sock#/mcp".to_string()),
            );
        }
        _ => {
            table.insert(
                "command".to_string(),
                toml::Value::String("ahma".to_string()),
            );
            let args = vec![
                toml::Value::String("serve".to_string()),
                toml::Value::String("stdio".to_string()),
                toml::Value::String("--tools".to_string()),
                toml::Value::String("simplify".to_string()),
                toml::Value::String("--sandbox".to_string()),
                toml::Value::String("--log-monitor".to_string()),
            ];
            table.insert("args".to_string(), toml::Value::Array(args));
        }
    }
    toml::Value::Table(table)
}

fn merge_codex_toml(path: &Path, value: toml::Value) -> Result<()> {
    let mut config = if path.exists() {
        let content = std::fs::read_to_string(path)?;
        toml::from_str(&content).unwrap_or_else(|_| toml::Value::Table(toml::map::Map::new()))
    } else {
        toml::Value::Table(toml::map::Map::new())
    };

    if !config.is_table() {
        config = toml::Value::Table(toml::map::Map::new());
    }

    let table = config.as_table_mut().unwrap();
    if !table.contains_key("mcp_servers") || !table.get("mcp_servers").unwrap().is_table() {
        table.insert(
            "mcp_servers".to_string(),
            toml::Value::Table(toml::map::Map::new()),
        );
    }

    let mcp_servers = table
        .get_mut("mcp_servers")
        .unwrap()
        .as_table_mut()
        .unwrap();
    mcp_servers.insert("Ahma".to_string(), value);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let content = toml::to_string_pretty(&config)?;
    std::fs::write(path, content)?;
    Ok(())
}

async fn setup_terminal_hooks(platforms: &[Platform], interactive: bool) -> Result<()> {
    let mut hook_platforms = Vec::new();
    let mut names = Vec::new();

    for platform in platforms.iter().copied().filter(|p| p.supports_hooks()) {
        if let Some(hook_platform) = platform.hook_platform() {
            hook_platforms.push(hook_platform);
            names.push(platform.label());
        }
    }

    if hook_platforms.is_empty() {
        return Ok(());
    }

    let install_args = HooksInstallArgs {
        platforms: hook_platforms,
        scope: HookScope::User,
        dry_run: false,
    };

    println!("Installing terminal hooks...");
    crate::hooks::run_install(install_args)?;

    if interactive {
        println!("\n✓ Hook setup complete! Restart these tools to apply hook wrappers:");
        for n in names {
            println!("    - {}", n);
        }
        println!();
    }

    Ok(())
}

fn setup_tls() -> Result<()> {
    use ahma_common::local_tls::{LocalTlsConfig, generate_and_save};
    let config = LocalTlsConfig::from_env();
    if config.exists() {
        println!("TLS certificate already exists.");
        return Ok(());
    }
    println!("Generating local TLS certificate...");
    generate_and_save(&config).context("Failed to generate local TLS certificate")?;
    println!("TLS certificate initialized.");
    Ok(())
}

/// The skills directories ahma writes `SKILL.md` into. A skill is just one
/// `SKILL.md` placed in a directory the agent auto-discovers — no plugin
/// manifest, marketplace, or enable-toggle. The directory name (`ahma`) becomes
/// the `/ahma` command on every platform.
///
///   - `~/.agents/skills/ahma/`  → cross-agent convention (Cursor, …)
///   - `~/.claude/skills/ahma/`  → Claude Code native personal skill
///
/// Both are written unconditionally and idempotently; they are plain file
/// writes with no version stamping, so `ahma update` simply overwrites them.
pub(crate) fn skill_install_dirs(home: &Path) -> [PathBuf; 2] {
    [
        home.join(".agents").join("skills").join("ahma"),
        home.join(".claude").join("skills").join("ahma"),
    ]
}

async fn setup_agent_skills(interactive: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("Could not resolve home directory"))?;

    for skill_dir in skill_install_dirs(&home) {
        let skill_path = skill_dir.join("SKILL.md");
        std::fs::create_dir_all(&skill_dir)
            .with_context(|| format!("Failed to create directory {}", skill_dir.display()))?;
        std::fs::write(&skill_path, SKILL_CONTENT)
            .with_context(|| format!("Failed to write skill to {}", skill_path.display()))?;
        if interactive {
            println!("✓ Installed ahma skill to {}", skill_path.display());
        }
    }

    // Migrate away from the legacy Claude Code *plugin* install (version-stamped
    // `~/.claude/plugins/cache/local/ahma/<version>/` + `installed_plugins.json`
    // + `enabledPlugins`). That design was fragile: each version bump
    // re-registered a new directory and orphaned the one that held the file,
    // leaving Claude Code pointed at an empty dir. The native personal skill
    // written above replaces it entirely, so tear the old plugin down.
    if let Err(e) = crate::uninstall::remove_claude_plugin(&home, false, false)
        && interactive
    {
        println!("  Note: could not clean up legacy Claude Code plugin: {e}");
    }

    if interactive {
        println!();
    }

    setup_llm_prompts(interactive).await?;

    Ok(())
}

async fn prompt_and_backup_prompts_file(
    path: &std::path::Path,
    new_template: &str,
    interactive: bool,
) -> Result<()> {
    use std::fs;

    let current_content = fs::read_to_string(path)?;
    if current_content == new_template {
        return Ok(());
    }

    if !interactive {
        println!(
            "Notice: Your global prompts file (~/.ahma/prompts.toml) differs from compiled-in defaults. Run 'ahma prompts update' to overwrite with defaults."
        );
        return Ok(());
    }

    println!(
        "\nNotice: A new version of default prompts is available, or your global prompts file has been modified."
    );
    if !prompt_yes_no_setup("Would you like to replace ~/.ahma/prompts.toml with the latest default template? (A backup will be created) [y/N]: ").await? {
        println!("Keeping existing ~/.ahma/prompts.toml intact.");
        println!();
        return Ok(());
    }

    let backup_path = path.with_extension("toml.bak");
    if backup_path.exists() {
        let _ = fs::remove_file(&backup_path);
    }
    fs::rename(path, &backup_path)?;
    fs::write(path, new_template)?;
    println!(
        "✓ Updated ~/.ahma/prompts.toml. Old version backed up to {}",
        backup_path.display()
    );
    println!();
    Ok(())
}

async fn setup_llm_prompts(interactive: bool) -> Result<()> {
    use ahma_common::prompts::AhmaPrompts;
    use std::fs;

    let Some(path) = ahma_common::prompts::global_prompts_path() else {
        return Ok(());
    };

    let new_template = AhmaPrompts::generate_template();

    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, &new_template)?;
        if interactive {
            println!("✓ Created global LLM prompts file at {}", path.display());
            println!();
        }
    } else {
        prompt_and_backup_prompts_file(&path, &new_template, interactive).await?;
    }
    Ok(())
}

async fn prompt_yes_no_setup(prompt: &str) -> Result<bool> {
    let prompt = prompt.to_string();
    tokio::task::spawn_blocking(move || {
        print!("{prompt}");
        let _ = io::stdout().flush();
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let trimmed = input.trim().to_lowercase();
        Ok(trimmed == "y" || trimmed == "yes")
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_merge_mcp_json_new_file() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("mcp.json");
        let val = json!({
            "type": "stdio",
            "command": "ahma"
        });

        merge_mcp_json(&path, "mcpServers", val)?;

        assert!(path.exists());
        let content = std::fs::read_to_string(&path)?;
        let parsed: serde_json::Value = serde_json::from_str(&content)?;
        assert_eq!(parsed["mcpServers"]["Ahma"]["type"], "stdio");
        assert_eq!(parsed["mcpServers"]["Ahma"]["command"], "ahma");
        Ok(())
    }

    #[test]
    fn test_merge_mcp_json_existing_file() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"other": "data", "mcpServers": {"Other": {"type": "stdio"}}}"#,
        )?;

        let val = json!({
            "type": "stdio",
            "command": "ahma"
        });

        merge_mcp_json(&path, "mcpServers", val)?;

        let content = std::fs::read_to_string(&path)?;
        let parsed: serde_json::Value = serde_json::from_str(&content)?;
        assert_eq!(parsed["other"], "data");
        assert_eq!(parsed["mcpServers"]["Other"]["type"], "stdio");
        assert_eq!(parsed["mcpServers"]["Ahma"]["type"], "stdio");
        Ok(())
    }

    #[test]
    fn test_merge_codex_toml_new_file() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("config.toml");
        let val = build_codex_toml_value("stdio");

        merge_codex_toml(&path, val)?;

        assert!(path.exists());
        let content = std::fs::read_to_string(&path)?;
        let parsed: toml::Value = toml::from_str(&content)?;
        assert_eq!(
            parsed["mcp_servers"]["Ahma"]["command"].as_str(),
            Some("ahma")
        );
        Ok(())
    }

    #[test]
    fn test_merge_codex_toml_existing_file() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "[other]\nkey = \"val\"\n[mcp_servers.Other]\ncommand = \"other\"",
        )?;

        let val = build_codex_toml_value("http");

        merge_codex_toml(&path, val)?;

        let content = std::fs::read_to_string(&path)?;
        let parsed: toml::Value = toml::from_str(&content)?;
        assert_eq!(parsed["other"]["key"].as_str(), Some("val"));
        assert_eq!(
            parsed["mcp_servers"]["Other"]["command"].as_str(),
            Some("other")
        );
        assert_eq!(
            parsed["mcp_servers"]["Ahma"]["url"].as_str(),
            Some("http://localhost:3000/mcp")
        );
        Ok(())
    }

    #[test]
    fn test_antigravity_servers_entry_uses_canonical_path_and_creates_dir() {
        let tmp = tempdir().unwrap();
        let fake_home = tmp.path();
        let entry = build_scoped_servers_entry("stdio", fake_home);

        // The sandbox scope must be a canonical path, not ~/sandbox.
        let args = entry["args"].as_array().expect("args must be an array");
        let scope_idx = args
            .iter()
            .position(|a| a.as_str() == Some("--sandbox-scope"))
            .expect("must contain --sandbox-scope");
        let scope_value = args[scope_idx + 1].as_str().unwrap();

        // Must NOT start with ~
        assert!(
            !scope_value.starts_with('~'),
            "sandbox scope must be canonical, not tilde: {scope_value}"
        );
        // Must be under the fake home
        let home_str = fake_home.to_string_lossy().to_string();
        assert!(
            scope_value.starts_with(&home_str),
            "scope must be under home dir: {scope_value}"
        );
        // Must end with /sandbox
        assert!(
            scope_value.ends_with("/sandbox") || scope_value.ends_with("\\sandbox"),
            "scope must end with /sandbox: {scope_value}"
        );
        // The directory must have been created
        assert!(
            fake_home.join("sandbox").exists(),
            "sandbox directory must be pre-created by setup"
        );
    }

    #[test]
    fn test_skill_install_dirs_targets_both_conventions() {
        let home = Path::new("/home/tester");
        let dirs = skill_install_dirs(home);

        // Cross-agent convention (Cursor, …) and Claude Code native personal
        // skill — both end in `skills/ahma` so the directory name yields `/ahma`.
        assert_eq!(
            dirs[0],
            home.join(".agents").join("skills").join("ahma"),
            "first target must be the generic ~/.agents/skills path"
        );
        assert_eq!(
            dirs[1],
            home.join(".claude").join("skills").join("ahma"),
            "second target must be the Claude Code native ~/.claude/skills path"
        );
    }

    #[test]
    fn test_skill_install_dirs_directory_name_is_ahma() {
        // The command name is derived from the directory name, so every target
        // must be named `ahma` for the skill to surface as `/ahma`.
        for dir in skill_install_dirs(Path::new("/some/home")) {
            assert_eq!(dir.file_name().unwrap(), "ahma");
        }
    }

    #[test]
    fn test_parse_selection_string() {
        assert_eq!(parse_selection_string("all", 5), vec![0, 1, 2, 3, 4]);
        assert_eq!(parse_selection_string("ALL", 3), vec![0, 1, 2]);
        assert_eq!(parse_selection_string("135", 5), vec![0, 2, 4]);
        assert_eq!(parse_selection_string("1 3 5", 5), vec![0, 2, 4]);
        assert_eq!(parse_selection_string("1,3.5", 5), vec![0, 2, 4]);
        assert_eq!(parse_selection_string("1;3;5", 5), vec![0, 2, 4]);
        assert_eq!(parse_selection_string("1, 2 , 3", 3), vec![0, 1, 2]);
        assert_eq!(parse_selection_string("12", 2), vec![0, 1]);
        assert_eq!(parse_selection_string("0 1 6", 5), vec![0]);
    }

    #[test]
    fn test_default_all_selection() {
        assert_eq!(default_all_selection(5), "all");
    }

    // ─── Default install args use --sandbox, not --tmp ────────────────────────

    #[test]
    fn test_default_mcp_entry_uses_sandbox_not_tmp() {
        let entry = build_mcp_servers_entry("stdio");
        let args = entry["args"].as_array().expect("args must be array");
        let has_sandbox = args.iter().any(|a| a.as_str() == Some("--sandbox"));
        let has_tmp = args.iter().any(|a| a.as_str() == Some("--tmp"));
        assert!(
            has_sandbox,
            "default stdio entry must include --sandbox: {args:?}"
        );
        assert!(
            !has_tmp,
            "default stdio entry must NOT include --tmp: {args:?}"
        );
    }

    #[test]
    fn test_claude_desktop_entry_uses_sandbox_not_tmp() {
        let tmp = tempdir().unwrap();
        let entry = build_claude_desktop_mcp_entry("stdio", tmp.path());
        let args = entry["args"].as_array().expect("args must be array");
        let has_sandbox = args.iter().any(|a| a.as_str() == Some("--sandbox"));
        let has_tmp = args.iter().any(|a| a.as_str() == Some("--tmp"));
        assert!(
            has_sandbox,
            "Claude Desktop entry must include --sandbox: {args:?}"
        );
        assert!(
            !has_tmp,
            "Claude Desktop entry must NOT include --tmp: {args:?}"
        );
    }

    #[test]
    fn test_antigravity_entry_uses_sandbox_not_tmp() {
        let tmp = tempdir().unwrap();
        let entry = build_scoped_servers_entry("stdio", tmp.path());
        let args = entry["args"].as_array().expect("args must be array");
        let has_sandbox = args.iter().any(|a| a.as_str() == Some("--sandbox"));
        let has_tmp = args.iter().any(|a| a.as_str() == Some("--tmp"));
        assert!(
            has_sandbox,
            "Antigravity entry must include --sandbox: {args:?}"
        );
        assert!(
            !has_tmp,
            "Antigravity entry must NOT include --tmp: {args:?}"
        );
    }

    /// Reinstalling over a stale failClosed:true hook entry migrates it to false.
    /// This behavior is verified via the hooks module's own test at line 1614-1617.
    #[test]
    fn test_hook_fail_open_verified_in_hooks_module() {
        // The assertion that failClosed is false lives in hooks/mod.rs:
        //   test_cursor_hook_default_has_fail_closed_false
    }

    // ─── Test helpers ─────────────────────────────────────────────────────────

    fn setup_args(auto: bool, mcp: bool, hooks: bool, skills: bool, tls: bool) -> SetupArgs {
        SetupArgs {
            auto,
            mcp,
            hooks,
            skills,
            tls,
        }
    }

    // ─── mcp_shared_transport_url ─────────────────────────────────────────────

    #[test]
    fn test_mcp_shared_transport_url_variants() {
        assert_eq!(
            mcp_shared_transport_url("http"),
            Some("http://localhost:3000/mcp")
        );
        assert_eq!(
            mcp_shared_transport_url("unix"),
            Some("unix:///tmp/ahma.sock#/mcp")
        );
        assert_eq!(mcp_shared_transport_url("stdio"), None);
        assert_eq!(mcp_shared_transport_url("bogus"), None);
    }

    // ─── build_mcp_servers_entry ──────────────────────────────────────────────

    #[test]
    fn test_build_mcp_servers_entry_http() {
        let entry = build_mcp_servers_entry("http");
        assert_eq!(entry["type"], "http");
        assert_eq!(entry["url"], "http://localhost:3000/mcp");
        assert!(entry.get("args").is_none());
    }

    #[test]
    fn test_build_mcp_servers_entry_unix() {
        let entry = build_mcp_servers_entry("unix");
        assert_eq!(entry["type"], "http");
        assert_eq!(entry["url"], "unix:///tmp/ahma.sock#/mcp");
    }

    #[test]
    fn test_build_mcp_servers_entry_stdio() {
        let entry = build_mcp_servers_entry("stdio");
        assert_eq!(entry["type"], "stdio");
        assert_eq!(entry["command"], "ahma");
        let args = entry["args"].as_array().unwrap();
        assert_eq!(args[0], "serve");
        assert_eq!(args[1], "stdio");
        assert!(args.iter().any(|a| a == "--log-monitor"));
    }

    // ─── build_scoped_servers_entry ───────────────────────────────────────────

    #[test]
    fn test_build_scoped_servers_entry_http_returns_url_only() {
        let tmp = tempdir().unwrap();
        let entry = build_scoped_servers_entry("http", tmp.path());
        assert_eq!(entry["url"], "http://localhost:3000/mcp");
        assert!(entry.get("command").is_none());
        // Shared-transport path must NOT pre-create the sandbox directory.
        assert!(!tmp.path().join("sandbox").exists());
    }

    #[test]
    fn test_build_scoped_servers_entry_unix_returns_url_only() {
        let tmp = tempdir().unwrap();
        let entry = build_scoped_servers_entry("unix", tmp.path());
        assert_eq!(entry["url"], "unix:///tmp/ahma.sock#/mcp");
        assert!(entry.get("command").is_none());
    }

    // ─── build_claude_desktop_mcp_entry ───────────────────────────────────────

    #[test]
    fn test_build_claude_desktop_mcp_entry_http() {
        let tmp = tempdir().unwrap();
        let entry = build_claude_desktop_mcp_entry("http", tmp.path());
        assert_eq!(entry["type"], "http");
        assert_eq!(entry["url"], "http://localhost:3000/mcp");
    }

    #[test]
    fn test_build_claude_desktop_mcp_entry_unix() {
        let tmp = tempdir().unwrap();
        let entry = build_claude_desktop_mcp_entry("unix", tmp.path());
        assert_eq!(entry["type"], "http");
        assert_eq!(entry["url"], "unix:///tmp/ahma.sock#/mcp");
    }

    #[test]
    fn test_build_claude_desktop_mcp_entry_stdio_has_no_type() {
        let tmp = tempdir().unwrap();
        let entry = build_claude_desktop_mcp_entry("stdio", tmp.path());
        assert!(entry.get("type").is_none());
        assert_eq!(entry["command"], "ahma");
        assert_eq!(entry["args"][0], "serve");
    }

    // ─── build_codex_toml_value ───────────────────────────────────────────────

    #[test]
    fn test_build_codex_toml_value_http() {
        let v = build_codex_toml_value("http");
        let t = v.as_table().unwrap();
        assert_eq!(
            t.get("url").unwrap().as_str(),
            Some("http://localhost:3000/mcp")
        );
        assert!(t.get("command").is_none());
    }

    #[test]
    fn test_build_codex_toml_value_unix() {
        let v = build_codex_toml_value("unix");
        let t = v.as_table().unwrap();
        assert_eq!(
            t.get("url").unwrap().as_str(),
            Some("unix:///tmp/ahma.sock#/mcp")
        );
    }

    #[test]
    fn test_build_codex_toml_value_stdio() {
        let v = build_codex_toml_value("stdio");
        let t = v.as_table().unwrap();
        assert_eq!(t.get("command").unwrap().as_str(), Some("ahma"));
        let args = t.get("args").unwrap().as_array().unwrap();
        assert_eq!(args[0].as_str(), Some("serve"));
        assert!(args.iter().any(|a| a.as_str() == Some("--sandbox")));
    }

    // ─── merge_mcp_json edge cases ────────────────────────────────────────────

    #[test]
    fn test_merge_mcp_json_malformed_existing_resets_to_object() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("mcp.json");
        // Not valid JSON at all -> from_str fails -> falls back to empty object.
        std::fs::write(&path, "this is not json {{")?;
        merge_mcp_json(&path, "mcpServers", json!({"command": "ahma"}))?;
        let parsed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        assert_eq!(parsed["mcpServers"]["Ahma"]["command"], "ahma");
        Ok(())
    }

    #[test]
    fn test_merge_mcp_json_non_object_root_resets() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("mcp.json");
        // Valid JSON but a top-level array, not an object.
        std::fs::write(&path, "[1, 2, 3]")?;
        merge_mcp_json(&path, "mcpServers", json!({"command": "ahma"}))?;
        let parsed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        assert!(parsed.is_object());
        assert_eq!(parsed["mcpServers"]["Ahma"]["command"], "ahma");
        Ok(())
    }

    #[test]
    fn test_merge_mcp_json_servers_key_not_object_is_replaced() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("mcp.json");
        // mcpServers exists but is a string, not an object -> replaced.
        std::fs::write(&path, r#"{"mcpServers": "oops"}"#)?;
        merge_mcp_json(&path, "mcpServers", json!({"command": "ahma"}))?;
        let parsed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        assert!(parsed["mcpServers"].is_object());
        assert_eq!(parsed["mcpServers"]["Ahma"]["command"], "ahma");
        Ok(())
    }

    #[test]
    fn test_merge_mcp_json_creates_parent_dirs() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("nested").join("deeper").join("mcp.json");
        merge_mcp_json(&path, "mcpServers", json!({"command": "ahma"}))?;
        assert!(path.exists());
        Ok(())
    }

    // ─── merge_codex_toml edge cases ──────────────────────────────────────────

    #[test]
    fn test_merge_codex_toml_malformed_existing_resets() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "this = = = not valid toml [[[")?;
        merge_codex_toml(&path, build_codex_toml_value("stdio"))?;
        let parsed: toml::Value = toml::from_str(&std::fs::read_to_string(&path)?)?;
        assert_eq!(
            parsed["mcp_servers"]["Ahma"]["command"].as_str(),
            Some("ahma")
        );
        Ok(())
    }

    #[test]
    fn test_merge_codex_toml_servers_key_not_table_is_replaced() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("config.toml");
        // mcp_servers exists but is a string.
        std::fs::write(&path, "mcp_servers = \"oops\"")?;
        merge_codex_toml(&path, build_codex_toml_value("stdio"))?;
        let parsed: toml::Value = toml::from_str(&std::fs::read_to_string(&path)?)?;
        assert!(parsed["mcp_servers"].is_table());
        assert_eq!(
            parsed["mcp_servers"]["Ahma"]["command"].as_str(),
            Some("ahma")
        );
        Ok(())
    }

    // ─── select_actions ───────────────────────────────────────────────────────

    #[test]
    fn test_select_actions_explicit_flags() {
        let args = setup_args(false, true, false, true, false);
        let actions = select_actions(&args, false);
        // Order is skills, mcp, hooks, tls per the flag checks.
        assert_eq!(actions.len(), 2);
        assert!(actions[0] == SetupAction::Skills);
        assert!(actions[1] == SetupAction::Mcp);
    }

    #[test]
    fn test_select_actions_single_flag() {
        let args = setup_args(false, false, false, false, true);
        let actions = select_actions(&args, false);
        assert_eq!(actions.len(), 1);
        assert!(actions[0] == SetupAction::Tls);
    }

    #[test]
    fn test_select_actions_no_flags_noninteractive_selects_the_full_default() {
        let args = setup_args(true, false, false, false, false);
        let actions = select_actions(&args, false);
        // Terminal hooks are now part of the default, because a denied command can
        // finally reach the user (SPEC R-PERM.3) and a grant applies on the very
        // next command (R-PERM.6). The *per-client* gate decides which clients get
        // them; this is the action-level default.
        assert_eq!(actions.len(), SETUP_ACTIONS.len());
        assert!(actions.contains(&SetupAction::Skills));
        assert!(actions.contains(&SetupAction::Mcp));
        assert!(actions.contains(&SetupAction::Tls));
        assert!(actions.contains(&SetupAction::Hooks));
    }

    #[test]
    fn test_select_actions_explicit_hooks_flag_still_included() {
        let args = setup_args(false, false, true, false, false);
        let actions = select_actions(&args, false);
        assert_eq!(actions, vec![SetupAction::Hooks]);
    }

    #[test]
    fn test_default_setup_actions_includes_hooks_once_a_client_is_ready() {
        // Hooks used to be excluded from the default unconditionally, because a
        // denied command had nowhere to ask the user and so could only be a wall.
        // With the question ladder (SPEC R-PERM.3) a denial reaches a human, and a
        // grant applies on the very next command — so readiness became a property
        // of each *client* (R-PERM.6), not a blanket property of ahma.
        let actions = default_setup_actions();
        assert!(
            crate::hooks::any_client_ready_for_hooks(),
            "precondition: at least one client is proven"
        );
        assert!(
            actions.contains(&SetupAction::Hooks),
            "hooks are installed by default once any client can carry a denial to a decision"
        );
        assert_eq!(actions.len(), SETUP_ACTIONS.len());
    }

    #[test]
    fn test_selection_string_for_includes_hooks() {
        let actions = default_setup_actions();
        // Skills=1, Mcp=2, Hooks=3, Tls=4.
        assert_eq!(selection_string_for(&actions), "1,2,3,4");
    }

    // ─── select_platforms ─────────────────────────────────────────────────────

    #[test]
    fn test_select_platforms_mcp_excludes_copilot() {
        let platforms = select_platforms(&[SetupAction::Mcp], false);
        // Every platform supports MCP except Copilot.
        assert_eq!(platforms.len(), PLATFORMS.len() - 1);
        assert!(!platforms.contains(&Platform::Copilot));
        assert!(platforms.contains(&Platform::ClaudeCode));
    }

    #[test]
    fn test_select_platforms_hooks_only() {
        let platforms = select_platforms(&[SetupAction::Hooks], false);
        // Hooks unsupported by VsCode, ClaudeDesktop, LmStudio.
        assert!(!platforms.contains(&Platform::VsCode));
        assert!(!platforms.contains(&Platform::ClaudeDesktop));
        assert!(!platforms.contains(&Platform::LmStudio));
        assert!(platforms.contains(&Platform::Copilot));
        assert!(platforms.contains(&Platform::ClaudeCode));
    }

    #[test]
    fn test_select_platforms_union_of_mcp_and_hooks() {
        let platforms = select_platforms(&[SetupAction::Mcp, SetupAction::Hooks], false);
        // Union covers everything (Copilot via hooks, VsCode via mcp).
        assert_eq!(platforms.len(), PLATFORMS.len());
    }

    // ─── prompt_multi_select_all (non-interactive) ────────────────────────────

    #[test]
    fn test_prompt_multi_select_all_noninteractive_returns_full_range() {
        let labels = ["a", "b", "c"];
        let chosen = prompt_multi_select_all(false, "q?", &labels);
        assert_eq!(chosen, vec![0, 1, 2]);
    }

    #[test]
    fn test_prompt_multi_select_all_noninteractive_empty_labels() {
        let chosen = prompt_multi_select_all(false, "q?", &[]);
        assert!(chosen.is_empty());
    }

    // ─── parse helpers: extra boundary cases ──────────────────────────────────

    #[test]
    fn test_parse_selection_string_empty_input() {
        // Empty (after trim) is not "all", not pure digits -> separated list -> empty.
        assert!(parse_selection_string("", 5).is_empty());
        assert!(parse_selection_string("   ", 5).is_empty());
    }

    #[test]
    fn test_parse_selection_string_dedups() {
        // Repeated digits are deduplicated.
        assert_eq!(parse_selection_string("112233", 5), vec![0, 1, 2]);
        assert_eq!(parse_selection_string("1 1 2 2", 5), vec![0, 1]);
    }

    #[test]
    fn test_parse_selection_string_pure_digits_large_max_uses_separated() {
        // max_val >= 10 forces the separated-list parser even for pure digits,
        // so "12" is read as the single number twelve, not 1 and 2.
        assert_eq!(parse_selection_string("12", 15), vec![11]);
    }

    #[test]
    fn test_parse_digit_sequence_filters_out_of_range() {
        // Only digits within 1..=max survive.
        assert_eq!(parse_digit_sequence("0192", 5), vec![0, 1]);
    }

    #[test]
    fn test_parse_separated_list_ignores_nonnumeric() {
        assert_eq!(parse_separated_list("1 foo 3 bar", 5), vec![0, 2]);
        assert!(parse_separated_list("foo bar", 5).is_empty());
    }

    // ─── claude_desktop_config_path / vscode_mcp_path ─────────────────────────

    #[test]
    fn test_claude_desktop_config_path_points_at_claude() {
        let p = claude_desktop_config_path().expect("home dir resolvable in test env");
        let s = p.to_string_lossy();
        assert!(s.contains("Claude"), "path should mention Claude: {s}");
        assert!(s.ends_with("claude_desktop_config.json"));
    }

    #[test]
    fn test_vscode_mcp_path_points_at_code_mcp_json() {
        let p = vscode_mcp_path().expect("home dir resolvable in test env");
        let s = p.to_string_lossy();
        assert!(s.contains("Code"), "path should mention Code: {s}");
        assert!(s.ends_with("mcp.json"));
    }

    // ─── SetupAction methods ──────────────────────────────────────────────────

    #[test]
    fn test_setup_action_labels_and_platform_specificity() {
        assert_eq!(SetupAction::Skills.label(), "Agent skills");
        assert_eq!(SetupAction::Mcp.label(), "MCP servers");
        assert_eq!(SetupAction::Hooks.label(), "Terminal hooks");
        assert_eq!(SetupAction::Tls.label(), "TLS certificates");

        assert!(SetupAction::Mcp.is_platform_specific());
        assert!(SetupAction::Hooks.is_platform_specific());
        assert!(!SetupAction::Skills.is_platform_specific());
        assert!(!SetupAction::Tls.is_platform_specific());
    }

    // ─── Platform methods ─────────────────────────────────────────────────────

    #[test]
    fn test_platform_labels_unique_and_nonempty() {
        for p in PLATFORMS.iter().copied() {
            assert!(!p.label().is_empty());
        }
        assert_eq!(Platform::Copilot.label(), "GitHub Copilot CLI");
        assert_eq!(Platform::VsCode.label(), "VS Code (GitHub Copilot Chat)");
    }

    #[test]
    fn test_platform_supports_mcp() {
        assert!(!Platform::Copilot.supports_mcp());
        for p in PLATFORMS
            .iter()
            .copied()
            .filter(|p| *p != Platform::Copilot)
        {
            assert!(p.supports_mcp(), "{} should support MCP", p.label());
        }
    }

    #[test]
    fn test_platform_supports_hooks() {
        assert!(!Platform::VsCode.supports_hooks());
        assert!(!Platform::ClaudeDesktop.supports_hooks());
        assert!(!Platform::LmStudio.supports_hooks());
        assert!(Platform::ClaudeCode.supports_hooks());
        assert!(Platform::Codex.supports_hooks());
        assert!(Platform::Cursor.supports_hooks());
        assert!(Platform::Antigravity.supports_hooks());
        assert!(Platform::Copilot.supports_hooks());
    }

    #[test]
    fn test_platform_hook_platform_mapping() {
        // Platforms with no terminal-hook wrapper map to None.
        assert!(Platform::ClaudeDesktop.hook_platform().is_none());
        assert!(Platform::LmStudio.hook_platform().is_none());
        assert!(Platform::VsCode.hook_platform().is_none());
        // The rest map to Some.
        assert!(Platform::Antigravity.hook_platform().is_some());
        assert!(Platform::ClaudeCode.hook_platform().is_some());
        assert!(Platform::Codex.hook_platform().is_some());
        assert!(Platform::Cursor.hook_platform().is_some());
        assert!(Platform::Copilot.hook_platform().is_some());
    }

    // ─── configure_mcp (home-parameterized platforms; safe in tempdir) ────────

    #[test]
    fn test_configure_mcp_claude_code_writes_claude_json() -> Result<()> {
        let tmp = tempdir()?;
        let home = tmp.path();
        let servers = build_mcp_servers_entry("stdio");
        let scoped = build_scoped_servers_entry("stdio", home);
        let name = Platform::ClaudeCode.configure_mcp("stdio", &servers, &scoped, home)?;
        assert_eq!(name, Some("Claude Code"));
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json"))?)?;
        assert_eq!(parsed["mcpServers"]["Ahma"]["type"], "stdio");
        Ok(())
    }

    #[test]
    fn test_configure_mcp_cursor_writes_cursor_mcp_json() -> Result<()> {
        let tmp = tempdir()?;
        let home = tmp.path();
        let servers = build_mcp_servers_entry("stdio");
        let scoped = build_scoped_servers_entry("stdio", home);
        let name = Platform::Cursor.configure_mcp("stdio", &servers, &scoped, home)?;
        assert_eq!(name, Some("Cursor"));
        let path = home.join(".cursor").join("mcp.json");
        let parsed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
        assert_eq!(parsed["mcpServers"]["Ahma"]["command"], "ahma");
        Ok(())
    }

    #[test]
    fn test_configure_mcp_antigravity_uses_scoped_entry() -> Result<()> {
        let tmp = tempdir()?;
        let home = tmp.path();
        let servers = build_mcp_servers_entry("stdio");
        let scoped = build_scoped_servers_entry("stdio", home);
        let name = Platform::Antigravity.configure_mcp("stdio", &servers, &scoped, home)?;
        assert_eq!(name, Some("Antigravity"));
        let path = home.join(".gemini").join("config").join("mcp_config.json");
        let parsed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
        // Scoped entry has a --sandbox-scope arg (and no "type" field).
        let args = parsed["mcpServers"]["Ahma"]["args"].as_array().unwrap();
        assert!(args.iter().any(|a| a == "--sandbox-scope"));
        Ok(())
    }

    #[test]
    fn test_configure_mcp_lmstudio_uses_scoped_entry() -> Result<()> {
        let tmp = tempdir()?;
        let home = tmp.path();
        let servers = build_mcp_servers_entry("stdio");
        let scoped = build_scoped_servers_entry("stdio", home);
        let name = Platform::LmStudio.configure_mcp("stdio", &servers, &scoped, home)?;
        assert_eq!(name, Some("LM Studio"));
        let path = home.join(".lmstudio").join("mcp.json");
        assert!(path.exists());
        Ok(())
    }

    #[test]
    fn test_configure_mcp_codex_writes_toml() -> Result<()> {
        let tmp = tempdir()?;
        let home = tmp.path();
        let servers = build_mcp_servers_entry("http");
        let scoped = build_scoped_servers_entry("http", home);
        let name = Platform::Codex.configure_mcp("http", &servers, &scoped, home)?;
        assert_eq!(name, Some("Codex CLI"));
        let path = home.join(".codex").join("config.toml");
        let parsed: toml::Value = toml::from_str(&std::fs::read_to_string(path)?)?;
        assert_eq!(
            parsed["mcp_servers"]["Ahma"]["url"].as_str(),
            Some("http://localhost:3000/mcp")
        );
        Ok(())
    }

    #[test]
    fn test_configure_mcp_copilot_returns_none() -> Result<()> {
        let tmp = tempdir()?;
        let home = tmp.path();
        let servers = build_mcp_servers_entry("stdio");
        let scoped = build_scoped_servers_entry("stdio", home);
        let name = Platform::Copilot.configure_mcp("stdio", &servers, &scoped, home)?;
        assert_eq!(name, None);
        Ok(())
    }

    // ─── setup_terminal_hooks: early-return when no platform supports hooks ────

    #[tokio::test]
    async fn test_setup_terminal_hooks_no_hook_platforms_is_noop() -> Result<()> {
        // VsCode/ClaudeDesktop/LmStudio do not support hooks, so the install
        // path is skipped and the function returns Ok without touching the FS.
        setup_terminal_hooks(
            &[
                Platform::VsCode,
                Platform::ClaudeDesktop,
                Platform::LmStudio,
            ],
            false,
        )
        .await?;
        Ok(())
    }

    // ─── print_mcp_restart_hints (smoke; covers transport match arms) ─────────

    #[test]
    fn test_print_mcp_restart_hints_noninteractive_is_noop() {
        // Non-interactive returns early; just exercise the guard.
        print_mcp_restart_hints(false, &["Cursor"], "http");
    }

    #[test]
    fn test_print_mcp_restart_hints_interactive_transport_arms() {
        // Exercise each transport branch (http / unix / stdio default).
        print_mcp_restart_hints(true, &["Cursor"], "http");
        print_mcp_restart_hints(true, &["Cursor"], "unix");
        print_mcp_restart_hints(true, &["Cursor"], "stdio");
        // Empty configured list returns early even when interactive.
        print_mcp_restart_hints(true, &[], "http");
    }

    // ─── env seam helper (AHMA_TEST_HOME redirects ahma_home_dir in debug) ─────

    use std::sync::{LazyLock, Mutex};

    static SETUP_ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Run `f` with `AHMA_TEST_HOME` pointed at `home`, restoring the prior value
    /// afterwards. Serialized so concurrent tests don't clobber the env var.
    fn with_test_home<R>(home: &Path, f: impl FnOnce() -> R) -> R {
        let _guard = SETUP_ENV_MUTEX.lock().unwrap();
        let prev = std::env::var_os("AHMA_TEST_HOME");
        // SAFETY: test-only; SETUP_ENV_MUTEX serializes env access in this module.
        unsafe { std::env::set_var("AHMA_TEST_HOME", home) };
        let result = f();
        // SAFETY: test-only; mutex held.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("AHMA_TEST_HOME", v),
                None => std::env::remove_var("AHMA_TEST_HOME"),
            }
        }
        result
    }

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    // ─── execute_actions: empty set is a no-op ────────────────────────────────

    #[tokio::test]
    async fn test_execute_actions_empty_is_noop() -> Result<()> {
        // No action selected -> all four guards are false -> Ok with no side effects.
        execute_actions(&[], &[], "stdio", false).await?;
        Ok(())
    }

    // ─── setup_llm_prompts (via AHMA_TEST_HOME seam) ──────────────────────────

    #[test]
    fn test_setup_llm_prompts_creates_when_missing() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        with_test_home(home, || {
            block_on(setup_llm_prompts(false)).expect("should create prompts file");
        });
        let path = home.join(".ahma").join("prompts.toml");
        assert!(path.exists(), "prompts.toml must be created");
        let written = std::fs::read_to_string(&path).unwrap();
        let template = ahma_common::prompts::AhmaPrompts::generate_template();
        assert_eq!(
            written, template,
            "created file must hold the default template"
        );
    }

    #[test]
    fn test_setup_llm_prompts_creates_when_missing_interactive() {
        // interactive=true exercises the success print branch as well.
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        with_test_home(home, || {
            block_on(setup_llm_prompts(true)).expect("should create prompts file");
        });
        assert!(home.join(".ahma").join("prompts.toml").exists());
    }

    #[test]
    fn test_setup_llm_prompts_existing_identical_is_noop() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        let dir = home.join(".ahma");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prompts.toml");
        let template = ahma_common::prompts::AhmaPrompts::generate_template();
        std::fs::write(&path, &template).unwrap();

        with_test_home(home, || {
            block_on(setup_llm_prompts(false)).expect("identical content is a no-op");
        });
        // Content is unchanged.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), template);
    }

    #[test]
    fn test_setup_llm_prompts_existing_differs_noninteractive_keeps_file() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        let dir = home.join(".ahma");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prompts.toml");
        std::fs::write(&path, "# user-modified prompts\n").unwrap();

        with_test_home(home, || {
            block_on(setup_llm_prompts(false))
                .expect("non-interactive divergent content only prints a notice");
        });
        // Non-interactive must NOT overwrite the user's file.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# user-modified prompts\n"
        );
    }

    // ─── prompt_and_backup_prompts_file (no env needed; path is a parameter) ──

    #[tokio::test]
    async fn test_prompt_and_backup_identical_is_noop() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("prompts.toml");
        std::fs::write(&path, "SAME")?;
        prompt_and_backup_prompts_file(&path, "SAME", true).await?;
        // Unchanged and no backup created.
        assert_eq!(std::fs::read_to_string(&path)?, "SAME");
        assert!(!path.with_extension("toml.bak").exists());
        Ok(())
    }

    #[tokio::test]
    async fn test_prompt_and_backup_noninteractive_differs_keeps_file() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("prompts.toml");
        std::fs::write(&path, "OLD")?;
        prompt_and_backup_prompts_file(&path, "NEW", false).await?;
        // Non-interactive: notice only, original retained, no backup.
        assert_eq!(std::fs::read_to_string(&path)?, "OLD");
        assert!(!path.with_extension("toml.bak").exists());
        Ok(())
    }

    #[tokio::test]
    async fn test_prompt_and_backup_interactive_eof_declines() -> Result<()> {
        // Interactive but stdin is at EOF (nextest null stdin) -> prompt_yes_no_setup
        // returns false -> the "keep existing" branch retains the original file.
        let tmp = tempdir()?;
        let path = tmp.path().join("prompts.toml");
        std::fs::write(&path, "OLD")?;
        prompt_and_backup_prompts_file(&path, "NEW", true).await?;
        assert_eq!(std::fs::read_to_string(&path)?, "OLD");
        assert!(!path.with_extension("toml.bak").exists());
        Ok(())
    }

    // ─── stdin-reading prompts: EOF falls back to defaults ────────────────────
    // Under `cargo nextest` stdin is redirected to null, so read_line yields EOF
    // immediately and these return their documented default without blocking.

    #[test]
    fn test_prompt_transport_eof_defaults_to_stdio() {
        assert_eq!(prompt_transport(), "stdio");
    }

    #[test]
    fn test_prompt_multi_select_eof_uses_default_all() {
        // Empty input (EOF) -> default "all" -> every option selected.
        let chosen = prompt_multi_select("Pick:", &["a", "b", "c"], "all");
        assert_eq!(chosen, vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn test_prompt_yes_no_setup_eof_is_false() -> Result<()> {
        // EOF / empty line is treated as "no".
        assert!(!prompt_yes_no_setup("Proceed? [y/N]: ").await?);
        Ok(())
    }
}
