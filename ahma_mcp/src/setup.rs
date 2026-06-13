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
    println!("\nChoose how your AI tools connect to ahma:");
    println!("  1) stdio  (recommended - private ahma instance per project)");
    println!("  2) http   (one shared server over TCP)");
    println!("  3) unix   (one shared server over Unix socket, Unix only)");
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
#[derive(Clone, Copy, PartialEq, Eq)]
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
/// GitHub Copilot has no MCP config target here, and VS Code is configured via
/// MCP only (no terminal hook wrapper).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Platform {
    Antigravity,
    ClaudeCode,
    Codex,
    Cursor,
    Copilot,
    VsCode,
}

const PLATFORMS: &[Platform] = &[
    Platform::Antigravity,
    Platform::ClaudeCode,
    Platform::Codex,
    Platform::Cursor,
    Platform::Copilot,
    Platform::VsCode,
];

impl Platform {
    fn label(self) -> &'static str {
        match self {
            Platform::Antigravity => "Antigravity",
            Platform::ClaudeCode => "Claude Code",
            Platform::Codex => "Codex",
            Platform::Cursor => "Cursor",
            Platform::Copilot => "GitHub Copilot CLI",
            Platform::VsCode => "VS Code (GitHub Copilot Chat)",
        }
    }

    fn supports_mcp(self) -> bool {
        !matches!(self, Platform::Copilot)
    }

    fn supports_hooks(self) -> bool {
        !matches!(self, Platform::VsCode)
    }

    fn hook_platform(self) -> Option<HookPlatform> {
        match self {
            Platform::Antigravity => Some(HookPlatform::Antigravity),
            Platform::ClaudeCode => Some(HookPlatform::Claude),
            Platform::Codex => Some(HookPlatform::Codex),
            Platform::Cursor => Some(HookPlatform::Cursor),
            Platform::Copilot => Some(HookPlatform::Copilot),
            Platform::VsCode => None,
        }
    }

    /// Apply MCP server configuration for this platform. Returns the display
    /// name on success, or `None` if there was nothing to configure.
    fn configure_mcp(
        self,
        transport: &str,
        servers_entry: &serde_json::Value,
        ant_servers_entry: &serde_json::Value,
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
            Platform::Cursor => {
                let path = home.join(".cursor").join("mcp.json");
                merge_mcp_json(&path, "mcpServers", servers_entry.clone())?;
                return Ok(Some("Cursor"));
            }
            Platform::Antigravity => {
                let path = home.join(".gemini").join("config").join("mcp_config.json");
                merge_mcp_json(&path, "mcpServers", ant_servers_entry.clone())?;
                return Ok(Some("Antigravity"));
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

/// Question 1: determine which actions to run.
///
/// Explicit `--mcp`/`--hooks`/`--skills`/`--tls` flags select a fixed subset
/// (for scripting). Otherwise the user is asked interactively; `--auto` and
/// non-interactive sessions default to all actions.
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

    let labels: Vec<&str> = SETUP_ACTIONS.iter().map(|a| a.label()).collect();
    let chosen = prompt_multi_select_all(
        interactive,
        "What would you like to set up? (comma-separated numbers):",
        &labels,
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
            "--tmp",
            "--log-monitor"
        ]
    })
}

fn build_antigravity_servers_entry(transport: &str, home: &Path) -> serde_json::Value {
    if let Some(url) = mcp_shared_transport_url(transport) {
        return json!({ "url": url });
    }
    json!({
        "command": "ahma",
        "args": [
            "serve",
            "stdio",
            "--tools",
            "simplify",
            "--tmp",
            "--log-monitor",
            "--sandbox-scope",
            home.to_string_lossy().to_string()
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
    let ant_servers_entry = build_antigravity_servers_entry(transport, &home);

    let mut configured = Vec::new();

    for platform in platforms.iter().copied().filter(|p| p.supports_mcp()) {
        if let Some(name) =
            platform.configure_mcp(transport, &servers_entry, &ant_servers_entry, &home)?
        {
            configured.push(name);
        }
    }

    print_mcp_restart_hints(interactive, &configured, transport);

    Ok(())
}

fn merge_mcp_json(path: &Path, servers_key: &str, value: serde_json::Value) -> Result<()> {
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
                toml::Value::String("--tmp".to_string()),
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

fn maybe_install_claude_plugin(home: &Path, interactive: bool) {
    if !home.join(".claude").exists() {
        return;
    }
    match install_claude_code_plugin(home) {
        Ok(plugin_dir) if interactive => {
            println!(
                "✓ Installed ahma as Claude Code plugin at {}",
                plugin_dir.display()
            );
        }
        Err(e) if interactive => {
            println!("  Note: Could not install Claude Code plugin: {e}");
        }
        _ => {}
    }
}

async fn setup_agent_skills(interactive: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("Could not resolve home directory"))?;

    // Generic cross-agent path (~/.agents/skills/ahma/SKILL.md)
    let skill_dir = home.join(".agents").join("skills").join("ahma");
    let skill_path = skill_dir.join("SKILL.md");
    std::fs::create_dir_all(&skill_dir)
        .with_context(|| format!("Failed to create directory {}", skill_dir.display()))?;
    std::fs::write(&skill_path, SKILL_CONTENT)
        .with_context(|| format!("Failed to write skill to {}", skill_path.display()))?;

    maybe_install_claude_plugin(&home, interactive);

    if interactive {
        println!("✓ Installed ahma skill to {}", skill_path.display());
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

/// Installs the ahma skill as a Claude Code plugin by writing files into
/// `~/.claude/plugins/cache/local/ahma/<version>/` and registering it in
/// `installed_plugins.json` and `settings.json`.
///
/// Returns the plugin directory path on success.
fn install_claude_code_plugin(home: &Path) -> Result<PathBuf> {
    let version = env!("CARGO_PKG_VERSION");
    let plugin_dir = home
        .join(".claude")
        .join("plugins")
        .join("cache")
        .join("local")
        .join("ahma")
        .join(version);

    // Write skills/ahma/SKILL.md
    let skill_dir = plugin_dir.join("skills").join("ahma");
    std::fs::create_dir_all(&skill_dir)
        .with_context(|| format!("Failed to create {}", skill_dir.display()))?;
    std::fs::write(skill_dir.join("SKILL.md"), SKILL_CONTENT)
        .context("Failed to write Claude Code plugin SKILL.md")?;

    // Write .claude-plugin/plugin.json
    let meta_dir = plugin_dir.join(".claude-plugin");
    std::fs::create_dir_all(&meta_dir)?;
    let plugin_json = json!({
        "name": "ahma",
        "description": "Kernel-sandboxed MCP server for AI agents. Wraps CLI tools with filesystem sandboxing, async execution, and live log monitoring.",
        "author": { "name": "Paul Houghton" }
    });
    std::fs::write(
        meta_dir.join("plugin.json"),
        serde_json::to_string_pretty(&plugin_json)?,
    )
    .context("Failed to write plugin.json")?;

    // Register in installed_plugins.json
    let now = chrono::Utc::now().to_rfc3339();
    let install_entry = json!({
        "scope": "user",
        "installPath": plugin_dir.to_string_lossy().to_string(),
        "version": version,
        "installedAt": now,
        "lastUpdated": now,
        "gitCommitSha": "local"
    });
    let plugins_json_path = home
        .join(".claude")
        .join("plugins")
        .join("installed_plugins.json");
    merge_installed_plugins(&plugins_json_path, "ahma@local", install_entry)?;

    // Enable in settings.json
    let settings_path = home.join(".claude").join("settings.json");
    enable_claude_code_plugin(&settings_path, "ahma@local")?;

    Ok(plugin_dir)
}

/// Adds or replaces the `plugin_key` entry in `installed_plugins.json`.
fn merge_installed_plugins(path: &Path, plugin_key: &str, entry: serde_json::Value) -> Result<()> {
    let mut config: serde_json::Value = if path.exists() {
        let content = std::fs::read_to_string(path)?;
        serde_json::from_str(&content).unwrap_or_else(|_| json!({"version": 2, "plugins": {}}))
    } else {
        json!({"version": 2, "plugins": {}})
    };

    if !config.is_object() {
        config = json!({"version": 2, "plugins": {}});
    }

    let obj = config.as_object_mut().unwrap();
    if !obj.get("plugins").is_some_and(|v| v.is_object()) {
        obj.insert("plugins".to_string(), json!({}));
    }

    obj.get_mut("plugins")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert(plugin_key.to_string(), json!([entry]));

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&config)?)?;
    Ok(())
}

/// Adds `plugin_key: true` to `enabledPlugins` in `settings.json`.
fn enable_claude_code_plugin(path: &Path, plugin_key: &str) -> Result<()> {
    let mut config: serde_json::Value = if path.exists() {
        let content = std::fs::read_to_string(path)?;
        serde_json::from_str(&content).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };

    if !config.is_object() {
        config = json!({});
    }

    let obj = config.as_object_mut().unwrap();
    if !obj.get("enabledPlugins").is_some_and(|v| v.is_object()) {
        obj.insert("enabledPlugins".to_string(), json!({}));
    }

    obj.get_mut("enabledPlugins")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert(plugin_key.to_string(), json!(true));

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&config)?)?;
    Ok(())
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
    fn test_merge_installed_plugins_new_file() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("installed_plugins.json");
        let entry = json!({"scope": "user", "installPath": "/some/path", "version": "1.0.0"});

        merge_installed_plugins(&path, "ahma@local", entry)?;

        let content = std::fs::read_to_string(&path)?;
        let parsed: serde_json::Value = serde_json::from_str(&content)?;
        assert_eq!(parsed["version"], 2);
        assert_eq!(parsed["plugins"]["ahma@local"][0]["version"], "1.0.0");
        assert_eq!(parsed["plugins"]["ahma@local"][0]["scope"], "user");
        Ok(())
    }

    #[test]
    fn test_merge_installed_plugins_preserves_other_entries() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("installed_plugins.json");
        std::fs::write(
            &path,
            r#"{"version":2,"plugins":{"other@marketplace":[{"scope":"user","version":"2.0.0"}]}}"#,
        )?;
        let entry = json!({"scope": "user", "installPath": "/p", "version": "0.11.0"});

        merge_installed_plugins(&path, "ahma@local", entry)?;

        let content = std::fs::read_to_string(&path)?;
        let parsed: serde_json::Value = serde_json::from_str(&content)?;
        assert_eq!(
            parsed["plugins"]["other@marketplace"][0]["version"],
            "2.0.0"
        );
        assert_eq!(parsed["plugins"]["ahma@local"][0]["version"], "0.11.0");
        Ok(())
    }

    #[test]
    fn test_enable_claude_code_plugin_new_file() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("settings.json");

        enable_claude_code_plugin(&path, "ahma@local")?;

        let content = std::fs::read_to_string(&path)?;
        let parsed: serde_json::Value = serde_json::from_str(&content)?;
        assert_eq!(parsed["enabledPlugins"]["ahma@local"], true);
        Ok(())
    }

    #[test]
    fn test_enable_claude_code_plugin_preserves_other_settings() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"model":"sonnet","enabledPlugins":{"github@claude-plugins-official":true}}"#,
        )?;

        enable_claude_code_plugin(&path, "ahma@local")?;

        let content = std::fs::read_to_string(&path)?;
        let parsed: serde_json::Value = serde_json::from_str(&content)?;
        assert_eq!(parsed["model"], "sonnet");
        assert_eq!(
            parsed["enabledPlugins"]["github@claude-plugins-official"],
            true
        );
        assert_eq!(parsed["enabledPlugins"]["ahma@local"], true);
        Ok(())
    }

    #[test]
    fn test_install_claude_code_plugin() -> Result<()> {
        let tmp = tempdir()?;
        let home = tmp.path();

        // Create ~/.claude/ so the function proceeds
        std::fs::create_dir_all(home.join(".claude"))?;

        let plugin_dir = install_claude_code_plugin(home)?;

        // Check SKILL.md was written
        assert!(
            plugin_dir
                .join("skills")
                .join("ahma")
                .join("SKILL.md")
                .exists()
        );
        // Check plugin.json was written
        let plugin_json_path = plugin_dir.join(".claude-plugin").join("plugin.json");
        assert!(plugin_json_path.exists());
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&plugin_json_path)?)?;
        assert_eq!(meta["name"], "ahma");
        // Check installed_plugins.json was updated
        let plugins_json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(
            home.join(".claude")
                .join("plugins")
                .join("installed_plugins.json"),
        )?)?;
        assert!(plugins_json["plugins"]["ahma@local"].is_array());
        // Check settings.json was updated
        let settings: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(
            home.join(".claude").join("settings.json"),
        )?)?;
        assert_eq!(settings["enabledPlugins"]["ahma@local"], true);
        Ok(())
    }

    #[test]
    fn test_install_claude_code_plugin_skipped_when_no_dot_claude() -> Result<()> {
        let tmp = tempdir()?;
        let home = tmp.path();
        // ~/.claude/ does NOT exist — plugin install should be skipped
        assert!(!home.join(".claude").exists());
        // setup_agent_skills checks for ~/.claude/ before calling install_claude_code_plugin,
        // so verify install_claude_code_plugin itself still works (the guard is in the caller)
        // but also verify it doesn't panic on an absent parent
        let result = install_claude_code_plugin(home);
        // It will succeed by creating the dirs under the tmp home
        assert!(result.is_ok());
        Ok(())
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
}
