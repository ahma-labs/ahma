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

fn configure_mcp_platform(
    idx: usize,
    transport: &str,
    servers_entry: &serde_json::Value,
    ant_servers_entry: &serde_json::Value,
    home: &Path,
) -> Result<Option<&'static str>> {
    match idx {
        0 => {
            // VS Code
            if let Some(path) = vscode_mcp_path() {
                merge_mcp_json(&path, "servers", servers_entry.clone())?;
                return Ok(Some("VS Code"));
            }
        }
        1 => {
            // Claude Code
            let path = home.join(".claude.json");
            merge_mcp_json(&path, "mcpServers", servers_entry.clone())?;
            return Ok(Some("Claude Code"));
        }
        2 => {
            // Cursor
            let path = home.join(".cursor").join("mcp.json");
            merge_mcp_json(&path, "mcpServers", servers_entry.clone())?;
            return Ok(Some("Cursor"));
        }
        3 => {
            // Antigravity
            let path = home.join(".gemini").join("config").join("mcp_config.json");
            merge_mcp_json(&path, "mcpServers", ant_servers_entry.clone())?;
            return Ok(Some("Antigravity"));
        }
        4 => {
            // Codex CLI
            let path = home.join(".codex").join("config.toml");
            let toml_val = build_codex_toml_value(transport);
            merge_codex_toml(&path, toml_val)?;
            return Ok(Some("Codex CLI"));
        }
        _ => {}
    }
    Ok(None)
}

/// Runs the setup wizard.
pub async fn run(args: SetupArgs) -> Result<()> {
    let interactive = !args.auto && io::stdin().is_terminal() && io::stdout().is_terminal();

    if interactive {
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("  Ahma Setup Wizard");
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!();
    }

    // 1. Configure MCP Servers
    if !args.hooks && !args.skills && !args.tls {
        let configure_mcp = if interactive {
            prompt_yes_no(
                "Configure ahma as a global MCP server for your AI tools?",
                true,
            )
        } else {
            true
        };

        if configure_mcp {
            setup_mcp_config(interactive).await?;
        }
    }

    // 2. Configure Terminal Hooks
    if !args.mcp && !args.skills && !args.tls {
        let configure_hooks = if interactive {
            prompt_yes_no(
                "Install user-scoped terminal hooks for Cursor, Claude Code, Codex, and GitHub Copilot?",
                false,
            )
        } else {
            true
        };

        if configure_hooks {
            setup_terminal_hooks(interactive).await?;
        }
    }

    // 3. Configure TLS Certificates
    if !args.mcp && !args.hooks && !args.skills {
        let configure_tls = if interactive {
            prompt_yes_no(
                "Initialize local TLS certificates for QUIC/HTTP3 transport?",
                false,
            )
        } else {
            true
        };

        if configure_tls {
            setup_tls()?;
        }
    }

    // 4. Configure Agent Skills
    if !args.mcp && !args.hooks && !args.tls {
        let configure_skills = if interactive {
            prompt_yes_no("Install Ahma agent skills to ~/.agents/skills/?", true)
        } else {
            true
        };

        if configure_skills {
            setup_agent_skills(interactive).await?;
        }
    }

    if interactive {
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("  Setup Completed!");
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!();
    }

    Ok(())
}

fn prompt_yes_no(question: &str, default_yes: bool) -> bool {
    let suffix = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!("{} {}: ", question, suffix);
    let _ = io::stdout().flush();
    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return default_yes;
    }
    let trimmed = input.trim().to_lowercase();
    if trimmed.is_empty() {
        return default_yes;
    }
    trimmed.starts_with('y')
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

async fn setup_mcp_config(interactive: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("Could not resolve home directory"))?;

    let platforms = vec![
        "VS Code",
        "Claude Code",
        "Cursor",
        "Antigravity",
        "Codex CLI",
    ];

    let selected = if interactive {
        prompt_multi_select(
            "Select platforms to configure (comma-separated numbers):",
            &platforms,
            "1,2,3,4,5",
        )
    } else {
        vec![0, 1, 2, 3, 4]
    };

    if selected.is_empty() {
        return Ok(());
    }

    let transport = if interactive {
        prompt_transport()
    } else {
        "stdio"
    };

    let servers_entry = match transport {
        "http" => json!({
            "type": "http",
            "url": "http://localhost:3000/mcp"
        }),
        "unix" => json!({
            "type": "http",
            "url": "unix:///tmp/ahma.sock#/mcp"
        }),
        _ => {
            // Default stdio
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
    };

    let ant_servers_entry = match transport {
        "http" => json!({
            "url": "http://localhost:3000/mcp"
        }),
        "unix" => json!({
            "url": "unix:///tmp/ahma.sock#/mcp"
        }),
        _ => json!({
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
        }),
    };

    let mut configured = Vec::new();

    for idx in selected {
        if let Some(name) =
            configure_mcp_platform(idx, transport, &servers_entry, &ant_servers_entry, &home)?
        {
            configured.push(name);
        }
    }

    if interactive && !configured.is_empty() {
        println!("\n✓ MCP setup complete! Restart these tools to apply changes:");
        for p in configured {
            println!("    - {}", p);
        }
        if transport == "http" {
            println!(
                "  Start the HTTP server before opening tools: ahma serve http --tools rust,simplify"
            );
        } else if transport == "unix" {
            println!(
                "  Start the Unix socket server before opening tools: ahma serve unix --socket-path /tmp/ahma.sock --tools rust,simplify"
            );
        }
        println!();
    }

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

fn platform_from_index(idx: usize) -> Option<(HookPlatform, &'static str)> {
    match idx {
        0 => Some((HookPlatform::Cursor, "Cursor")),
        1 => Some((HookPlatform::Claude, "Claude Code")),
        2 => Some((HookPlatform::Codex, "Codex")),
        3 => Some((HookPlatform::Copilot, "GitHub Copilot")),
        4 => Some((HookPlatform::Antigravity, "Antigravity")),
        _ => None,
    }
}

async fn setup_terminal_hooks(interactive: bool) -> Result<()> {
    let tools = vec![
        "Cursor",
        "Claude Code",
        "Codex",
        "GitHub Copilot",
        "Antigravity",
    ];

    let selected = if interactive {
        prompt_multi_select(
            "Select platforms to configure hooks (comma-separated numbers):",
            &tools,
            "1,2,3,4,5",
        )
    } else {
        vec![0, 1, 2, 3, 4]
    };

    if selected.is_empty() {
        return Ok(());
    }

    let mut platforms = Vec::new();
    let mut names = Vec::new();

    for idx in selected {
        if let Some((platform, name)) = platform_from_index(idx) {
            platforms.push(platform);
            names.push(name);
        }
    }

    let install_args = HooksInstallArgs {
        platforms,
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
