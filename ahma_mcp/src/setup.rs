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
            Platform::Copilot => "GitHub Copilot",
            Platform::VsCode => "VS Code",
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
                    return Ok(Some("VS Code"));
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

    if actions.contains(&SetupAction::Mcp) {
        setup_mcp_config(&platforms, transport, interactive).await?;
    }
    if actions.contains(&SetupAction::Hooks) {
        setup_terminal_hooks(&platforms, interactive).await?;
    }
    if actions.contains(&SetupAction::Skills) {
        setup_agent_skills(interactive).await?;
    }
    if actions.contains(&SetupAction::Tls) {
        setup_tls()?;
    }

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

fn default_all_selection(count: usize) -> String {
    (1..=count)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_selection_string(input: &str, max_val: usize) -> Vec<usize> {
    let mut selections = Vec::new();
    let normalized = input.replace(',', " ");
    for part in normalized.split_whitespace() {
        if let Some(num) = part
            .parse::<usize>()
            .ok()
            .filter(|&n| n >= 1 && n <= max_val)
        {
            selections.push(num - 1);
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
            "rust,simplify",
            "--tmp",
            "--log-monitor"
        ]
    })
}

fn build_antigravity_servers_entry(transport: &str) -> serde_json::Value {
    if let Some(url) = mcp_shared_transport_url(transport) {
        return json!({ "url": url });
    }
    json!({
        "command": "ahma",
        "args": [
            "serve",
            "stdio",
            "--tools",
            "rust,simplify",
            "--tmp",
            "--log-monitor"
        ],
        "env": {
            "AHMA_SANDBOX_SCOPE": "~"
        }
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
            "  Start the HTTP server before opening tools: ahma serve http --tools rust,simplify"
        ),
        "unix" => println!(
            "  Start the Unix socket server before opening tools: ahma serve unix --socket-path /tmp/ahma.sock --tools rust,simplify"
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
    let ant_servers_entry = build_antigravity_servers_entry(transport);

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
                toml::Value::String("rust,simplify".to_string()),
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

async fn setup_agent_skills(interactive: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("Could not resolve home directory"))?;
    let skill_dir = home.join(".agents").join("skills").join("ahma");
    let skill_path = skill_dir.join("SKILL.md");

    std::fs::create_dir_all(&skill_dir)
        .with_context(|| format!("Failed to create directory {}", skill_dir.display()))?;

    std::fs::write(&skill_path, SKILL_CONTENT)
        .with_context(|| format!("Failed to write skill to {}", skill_path.display()))?;

    if interactive {
        println!("✓ Installed ahma skill to {}", skill_path.display());
        println!();
    }
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
}
