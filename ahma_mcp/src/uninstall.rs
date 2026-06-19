//! # Ahma Uninstall Wizard
//!
//! Symmetric teardown of `ahma setup`: removes every integration artifact that setup
//! created (MCP server entries, terminal hooks, agent skills, Claude Code plugin, and
//! optionally the ahma binary).  Only Ahma-managed keys and directories are removed;
//! other user content in the same config files is always preserved.
//!
//! ## Interactive mode (no flags)
//!
//! Asks two questions, mirroring `ahma setup`:
//! 1. **What** to remove: Agent skills, MCP servers, Terminal hooks, ahma binary.
//! 2. **Which platforms** (for MCP / hooks actions).
//! Then optionally: purge `~/.ahma` data dir (default no).
//!
//! ## Non-interactive / scripted mode
//!
//! `--auto` selects everything without prompting.  Individual action flags (`--mcp`,
//! `--hooks`, `--skills`, `--binary`) and `--platform` allow surgical removal.

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::hooks::{HookPlatform, HookScope, HooksUninstallArgs};
use crate::shell::cli::UninstallArgs;

// ── Action / platform enum mirrors (private) ─────────────────────────────────

/// An action that the wizard can remove.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum UninstallAction {
    Skills,
    Mcp,
    Hooks,
    Binary,
}

const UNINSTALL_ACTIONS: &[UninstallAction] = &[
    UninstallAction::Skills,
    UninstallAction::Mcp,
    UninstallAction::Hooks,
    UninstallAction::Binary,
];

impl UninstallAction {
    fn label(self) -> &'static str {
        match self {
            UninstallAction::Skills => "Agent skills",
            UninstallAction::Mcp => "MCP servers",
            UninstallAction::Hooks => "Terminal hooks",
            UninstallAction::Binary => "ahma binary",
        }
    }

    fn is_platform_specific(self) -> bool {
        matches!(self, UninstallAction::Mcp | UninstallAction::Hooks)
    }
}

/// A platform that setup can target (mirrors `setup.rs`).
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

    fn cli_name(self) -> &'static str {
        match self {
            Platform::Antigravity => "antigravity",
            Platform::ClaudeCode => "claude",
            Platform::ClaudeDesktop => "claude-desktop",
            Platform::Codex => "codex",
            Platform::Cursor => "cursor",
            Platform::Copilot => "copilot",
            Platform::LmStudio => "lmstudio",
            Platform::VsCode => "vscode",
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
}

// ── Entry point ──────────────────────────────────────────────────────────────

/// Runs the uninstall wizard.
pub async fn run(args: UninstallArgs) -> Result<()> {
    let interactive = !args.auto && io::stdin().is_terminal() && io::stdout().is_terminal();

    if interactive {
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("  Ahma Uninstall Wizard");
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

    // Question 2: which platforms (for per-platform actions).
    let platforms = if actions.iter().any(|a| a.is_platform_specific()) {
        select_platforms(&actions, &args.platforms, interactive)
    } else {
        Vec::new()
    };

    // Optionally purge the ~/.ahma data directory.
    let purge = args.purge
        || (interactive
            && actions.iter().any(|a| !a.is_platform_specific())
            && prompt_yes_no(
                "Also remove all ahma data and config in ~/.ahma? \
(TLS, prompts, settings, logs) [y/N]: ",
            )?);

    execute_actions(&actions, &platforms, args.dry_run, purge, interactive).await?;

    if interactive {
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("  Uninstall completed!");
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!();
    }

    Ok(())
}

// ── Selection helpers ─────────────────────────────────────────────────────────

fn select_actions(args: &UninstallArgs, interactive: bool) -> Vec<UninstallAction> {
    let mut flagged = Vec::new();
    if args.skills {
        flagged.push(UninstallAction::Skills);
    }
    if args.mcp {
        flagged.push(UninstallAction::Mcp);
    }
    if args.hooks {
        flagged.push(UninstallAction::Hooks);
    }
    if args.binary {
        flagged.push(UninstallAction::Binary);
    }
    if !flagged.is_empty() {
        return flagged;
    }

    let labels: Vec<&str> = UNINSTALL_ACTIONS.iter().map(|a| a.label()).collect();
    let chosen = prompt_multi_select_all(
        interactive,
        "What would you like to uninstall? (comma-separated numbers, default: all):",
        &labels,
    );
    chosen
        .into_iter()
        .filter_map(|i| UNINSTALL_ACTIONS.get(i).copied())
        .collect()
}

fn select_platforms(
    actions: &[UninstallAction],
    platform_filter: &[String],
    interactive: bool,
) -> Vec<Platform> {
    let want_mcp = actions.contains(&UninstallAction::Mcp);
    let want_hooks = actions.contains(&UninstallAction::Hooks);

    let relevant: Vec<Platform> = PLATFORMS
        .iter()
        .copied()
        .filter(|p| (want_mcp && p.supports_mcp()) || (want_hooks && p.supports_hooks()))
        .collect();

    // If caller specified platforms via --platform flag, filter to those.
    if !platform_filter.is_empty() {
        return relevant
            .into_iter()
            .filter(|p| {
                platform_filter.iter().any(|f| {
                    f.eq_ignore_ascii_case(p.cli_name()) || f.eq_ignore_ascii_case(p.label())
                })
            })
            .collect();
    }

    let labels: Vec<&str> = relevant.iter().map(|p| p.label()).collect();
    let chosen = prompt_multi_select_all(
        interactive,
        "From which platforms? (comma-separated numbers, default: all):",
        &labels,
    );
    chosen
        .into_iter()
        .filter_map(|i| relevant.get(i).copied())
        .collect()
}

// ── Action dispatcher ─────────────────────────────────────────────────────────

async fn execute_actions(
    actions: &[UninstallAction],
    platforms: &[Platform],
    dry_run: bool,
    purge: bool,
    interactive: bool,
) -> Result<()> {
    let mut affected_platforms: Vec<&str> = Vec::new();

    if actions.contains(&UninstallAction::Mcp) {
        let names = uninstall_mcp_config(platforms, dry_run)?;
        affected_platforms.extend(names);
    }
    if actions.contains(&UninstallAction::Hooks) {
        let names = uninstall_terminal_hooks(platforms, dry_run)?;
        affected_platforms.extend(names);
    }
    if actions.contains(&UninstallAction::Skills) {
        uninstall_agent_skills(dry_run, interactive)?;
    }
    if actions.contains(&UninstallAction::Binary) {
        uninstall_binary(dry_run)?;
    }
    if purge {
        purge_ahma_dir(dry_run)?;
    }

    print_restart_hints(interactive, &affected_platforms, dry_run);

    Ok(())
}

// ── MCP teardown ──────────────────────────────────────────────────────────────

fn uninstall_mcp_config(platforms: &[Platform], dry_run: bool) -> Result<Vec<&'static str>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("Could not resolve home directory"))?;
    let mut removed = Vec::new();

    for platform in platforms.iter().copied().filter(|p| p.supports_mcp()) {
        let result = match platform {
            Platform::VsCode => {
                if let Some(path) = vscode_mcp_path() {
                    remove_mcp_entry(&path, "servers", dry_run)
                        .with_context(|| format!("VS Code MCP config at {}", path.display()))?;
                    Some("VS Code (GitHub Copilot Chat)")
                } else {
                    None
                }
            }
            Platform::ClaudeCode => {
                let path = home.join(".claude.json");
                remove_mcp_entry(&path, "mcpServers", dry_run)
                    .context("Claude Code MCP config (~/.claude.json)")?;
                Some("Claude Code")
            }
            Platform::ClaudeDesktop => {
                if let Some(path) = claude_desktop_config_path() {
                    remove_mcp_entry(&path, "mcpServers", dry_run)
                        .with_context(|| format!("Claude Desktop config at {}", path.display()))?;
                    Some("Claude Desktop")
                } else {
                    None
                }
            }
            Platform::Cursor => {
                let path = home.join(".cursor").join("mcp.json");
                remove_mcp_entry(&path, "mcpServers", dry_run)
                    .context("Cursor MCP config (~/.cursor/mcp.json)")?;
                Some("Cursor")
            }
            Platform::Antigravity => {
                let path = home.join(".gemini").join("config").join("mcp_config.json");
                remove_mcp_entry(&path, "mcpServers", dry_run)
                    .with_context(|| format!("Antigravity MCP config at {}", path.display()))?;
                Some("Antigravity")
            }
            Platform::LmStudio => {
                let path = home.join(".lmstudio").join("mcp.json");
                remove_mcp_entry(&path, "mcpServers", dry_run)
                    .with_context(|| format!("LM Studio MCP config at {}", path.display()))?;
                Some("LM Studio")
            }
            Platform::Codex => {
                let path = home.join(".codex").join("config.toml");
                remove_codex_mcp(&path, dry_run).context("Codex config (~/.codex/config.toml)")?;
                Some("Codex CLI")
            }
            Platform::Copilot => None,
        };
        if let Some(name) = result {
            if dry_run {
                println!("[dry-run] Would remove Ahma MCP entry from {}", name);
            }
            removed.push(name);
        }
    }

    Ok(removed)
}

/// Remove the `"Ahma"` key from `config[servers_key]` in a JSON MCP config file.
///
/// Preserves all other content.  If the `servers_key` object becomes empty after
/// removal, the key is pruned too.  Never deletes the file itself.  No-ops gracefully
/// when the file or key is absent.
pub fn remove_mcp_entry(path: &Path, servers_key: &str, dry_run: bool) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let mut config: Value =
        serde_json::from_str(&content).unwrap_or_else(|_| Value::Object(serde_json::Map::new()));

    if !config.is_object() {
        return Ok(());
    }
    let obj = config.as_object_mut().unwrap();

    let Some(servers_val) = obj.get_mut(servers_key) else {
        return Ok(()); // servers key absent — nothing to do
    };

    if let Some(servers_obj) = servers_val.as_object_mut() {
        if servers_obj.remove("Ahma").is_none() {
            return Ok(()); // Ahma key already absent
        }
        // Prune now-empty servers object.
        let servers_empty = servers_obj.is_empty();
        if servers_empty {
            obj.remove(servers_key);
        }
    } else {
        return Ok(());
    }

    if dry_run {
        return Ok(());
    }

    let mut file = std::fs::File::create(path)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    serde_json::to_writer_pretty(&mut file, &config)
        .with_context(|| format!("Failed to serialize {}", path.display()))?;
    Ok(())
}

/// Remove `[mcp_servers.Ahma]` from a Codex TOML config.
///
/// Preserves all other content.  Prunes the `[mcp_servers]` table if it becomes
/// empty.  No-ops when the file or key is absent.
pub fn remove_codex_mcp(path: &Path, dry_run: bool) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let mut config: toml::Value =
        toml::from_str(&content).unwrap_or_else(|_| toml::Value::Table(toml::map::Map::new()));

    if !config.is_table() {
        return Ok(());
    }
    let table = config.as_table_mut().unwrap();

    let Some(mcp_servers) = table.get_mut("mcp_servers") else {
        return Ok(());
    };
    if let Some(mcp_table) = mcp_servers.as_table_mut() {
        if mcp_table.remove("Ahma").is_none() {
            return Ok(());
        }
        let empty = mcp_table.is_empty();
        if empty {
            table.remove("mcp_servers");
        }
    }

    if dry_run {
        return Ok(());
    }

    let serialized = toml::to_string_pretty(&config).context("Failed to serialize Codex config")?;
    std::fs::write(path, serialized)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

// ── Hooks teardown ────────────────────────────────────────────────────────────

fn uninstall_terminal_hooks(platforms: &[Platform], dry_run: bool) -> Result<Vec<&'static str>> {
    let mut removed = Vec::new();

    let hook_platforms: Vec<HookPlatform> = platforms
        .iter()
        .copied()
        .filter(|p| p.supports_hooks())
        .filter_map(|p| p.hook_platform())
        .collect();

    if hook_platforms.is_empty() {
        return Ok(removed);
    }

    let names: Vec<&str> = platforms
        .iter()
        .copied()
        .filter(|p| p.supports_hooks() && p.hook_platform().is_some())
        .map(|p| p.label())
        .collect();

    let uninstall_args = HooksUninstallArgs {
        platforms: hook_platforms,
        scope: HookScope::User,
        dry_run,
    };
    crate::hooks::run_uninstall(uninstall_args)?;
    removed.extend_from_slice(&names);

    if !dry_run {
        println!(
            "  Note: project-scoped hooks (if any) can be removed with: \
ahma hooks uninstall --scope project"
        );
    }

    Ok(removed)
}

// ── Skills / Claude plugin teardown ───────────────────────────────────────────

fn uninstall_agent_skills(dry_run: bool, interactive: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("Could not resolve home directory"))?;

    let skill_dir = home.join(".agents").join("skills").join("ahma");
    if skill_dir.exists() {
        if dry_run {
            println!("[dry-run] Would remove {}", skill_dir.display());
        } else {
            std::fs::remove_dir_all(&skill_dir)
                .with_context(|| format!("Failed to remove {}", skill_dir.display()))?;
            if interactive {
                println!("✓ Removed agent skill directory {}", skill_dir.display());
            }
        }
    }

    remove_claude_plugin(&home, dry_run, interactive)?;

    Ok(())
}

/// Remove the legacy Claude Code *plugin* install (used before ahma switched to
/// a native `~/.claude/skills/ahma/` personal skill). Setup now calls this for
/// migration cleanup, and uninstall calls it for full teardown.
///
/// - Deletes `~/.claude/plugins/cache/local/ahma/` (the entire version tree).
/// - Removes `ahma@local` from `~/.claude/plugins/installed_plugins.json`.
/// - Removes `enabledPlugins."ahma@local"` from `~/.claude/settings.json`
///   (careful not to touch any other keys, including hook entries).
pub fn remove_claude_plugin(home: &Path, dry_run: bool, interactive: bool) -> Result<()> {
    let plugins_dir = home
        .join(".claude")
        .join("plugins")
        .join("cache")
        .join("local")
        .join("ahma");

    if plugins_dir.exists() {
        if dry_run {
            println!("[dry-run] Would remove {}", plugins_dir.display());
        } else {
            std::fs::remove_dir_all(&plugins_dir)
                .with_context(|| format!("Failed to remove {}", plugins_dir.display()))?;
            if interactive {
                println!(
                    "✓ Removed Claude Code plugin cache {}",
                    plugins_dir.display()
                );
            }
        }
    }

    // Remove from installed_plugins.json
    let plugins_json = home
        .join(".claude")
        .join("plugins")
        .join("installed_plugins.json");
    remove_installed_plugin_entry(&plugins_json, "ahma@local", dry_run)?;

    // Remove from enabledPlugins in settings.json
    let settings_path = home.join(".claude").join("settings.json");
    disable_claude_plugin(&settings_path, "ahma@local", dry_run)?;

    Ok(())
}

/// Remove `plugin_key` from the `plugins` object in `installed_plugins.json`.
fn remove_installed_plugin_entry(path: &Path, plugin_key: &str, dry_run: bool) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let mut config: Value = serde_json::from_str(&content)
        .unwrap_or_else(|_| serde_json::json!({"version": 2, "plugins": {}}));

    let plugins = config
        .as_object_mut()
        .and_then(|o| o.get_mut("plugins"))
        .and_then(|p| p.as_object_mut());

    if let Some(map) = plugins {
        if map.remove(plugin_key).is_none() {
            return Ok(());
        }
    } else {
        return Ok(());
    }

    if dry_run {
        println!(
            "[dry-run] Would remove {} from {}",
            plugin_key,
            path.display()
        );
        return Ok(());
    }

    std::fs::write(path, serde_json::to_string_pretty(&config)?)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

/// Remove `enabledPlugins[plugin_key]` from Claude Code `settings.json`.
///
/// Only touches `enabledPlugins` — hooks and any other user keys are preserved.
fn disable_claude_plugin(path: &Path, plugin_key: &str, dry_run: bool) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let mut config: Value =
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}));

    let enabled = config
        .as_object_mut()
        .and_then(|o| o.get_mut("enabledPlugins"))
        .and_then(|e| e.as_object_mut());

    if let Some(map) = enabled {
        if map.remove(plugin_key).is_none() {
            return Ok(());
        }
    } else {
        return Ok(());
    }

    if dry_run {
        println!(
            "[dry-run] Would remove enabledPlugins.{} from {}",
            plugin_key,
            path.display()
        );
        return Ok(());
    }

    std::fs::write(path, serde_json::to_string_pretty(&config)?)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

// ── Binary teardown ───────────────────────────────────────────────────────────

fn uninstall_binary(dry_run: bool) -> Result<()> {
    let install_dir = resolve_install_dir()?;
    let binary_name = if cfg!(windows) { "ahma.exe" } else { "ahma" };
    let binary_path = install_dir.join(binary_name);
    let old_path = install_dir.join(if cfg!(windows) {
        "ahma.old.exe"
    } else {
        "ahma.old"
    });

    if dry_run {
        if binary_path.exists() {
            println!("[dry-run] Would remove {}", binary_path.display());
        }
        if old_path.exists() {
            println!("[dry-run] Would remove {}", old_path.display());
        }
        return Ok(());
    }

    #[cfg(unix)]
    {
        if old_path.exists() {
            let _ = std::fs::remove_file(&old_path);
        }
        if binary_path.exists() {
            std::fs::remove_file(&binary_path)
                .with_context(|| format!("Failed to remove {}", binary_path.display()))?;
            println!("✓ Removed {}", binary_path.display());
        } else {
            println!(
                "  Binary not found at {} — nothing to remove.",
                binary_path.display()
            );
        }
    }

    #[cfg(windows)]
    {
        // A running Windows .exe cannot delete itself.  Print instructions instead.
        if binary_path.exists() {
            println!(
                "  To remove the ahma binary on Windows, run this after closing all ahma processes:"
            );
            println!("    Remove-Item -Force \"{}\"", binary_path.display());
            if old_path.exists() {
                println!("    Remove-Item -Force \"{}\"", old_path.display());
            }
        } else {
            println!(
                "  Binary not found at {} — nothing to remove.",
                binary_path.display()
            );
        }
    }

    Ok(())
}

/// Resolve the directory where the binary was installed.
///
/// Respects `AHMA_INSTALL_DIR` environment variable, falling back to `~/.local/bin`.
fn resolve_install_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("AHMA_INSTALL_DIR")
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir));
    }
    crate::update::default_install_dir()
}

// ── Purge ~/.ahma ─────────────────────────────────────────────────────────────

fn purge_ahma_dir(dry_run: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("Could not resolve home directory"))?;
    let ahma_dir = home.join(".ahma");
    // The Antigravity setup creates ~/sandbox for the stdio MCP entry.
    let sandbox_dir = home.join("sandbox");

    if dry_run {
        if ahma_dir.exists() {
            println!("[dry-run] Would remove {}", ahma_dir.display());
        }
        if sandbox_dir.exists() {
            println!(
                "[dry-run] Would remove {} (Antigravity sandbox directory)",
                sandbox_dir.display()
            );
        }
        return Ok(());
    }

    if ahma_dir.exists() {
        std::fs::remove_dir_all(&ahma_dir)
            .with_context(|| format!("Failed to remove {}", ahma_dir.display()))?;
        println!("✓ Removed {}", ahma_dir.display());
    }

    if sandbox_dir.exists() {
        // Only remove the sandbox dir if it appears to have been created by ahma setup
        // (i.e. is empty or only contains an ahma marker).  We do NOT forcibly delete
        // a non-empty user directory named "sandbox".
        let entries: Vec<_> = std::fs::read_dir(&sandbox_dir)
            .map(|rd| rd.flatten().collect::<Vec<_>>())
            .unwrap_or_default();
        if entries.is_empty() {
            let _ = std::fs::remove_dir(&sandbox_dir);
            println!(
                "✓ Removed empty Antigravity sandbox dir {}",
                sandbox_dir.display()
            );
        } else {
            println!(
                "  Skipping non-empty {} — remove manually if desired.",
                sandbox_dir.display()
            );
        }
    }

    Ok(())
}

// ── Output ────────────────────────────────────────────────────────────────────

fn print_restart_hints(interactive: bool, affected_platforms: &[&str], dry_run: bool) {
    if dry_run {
        println!("\n[dry-run] No changes were made.");
        return;
    }
    if !interactive || affected_platforms.is_empty() {
        return;
    }
    println!(
        "\n✓ Uninstall complete! Restart (fully quit and reopen) these tools to apply changes:"
    );
    for name in affected_platforms {
        println!("    - {}", name);
    }
    println!();
    println!("  If an `ahma tui` is open, close it; any background ahma servers will shut down");
    println!("  automatically once no client is connected.");
    println!();
}

// ── Prompt helpers (sync) ─────────────────────────────────────────────────────

fn prompt_yes_no(prompt: &str) -> Result<bool> {
    print!("{}", prompt);
    let _ = io::stdout().flush();
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let t = input.trim().to_lowercase();
    Ok(t == "y" || t == "yes")
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

fn prompt_multi_select_all(interactive: bool, question: &str, labels: &[&str]) -> Vec<usize> {
    if !interactive {
        return (0..labels.len()).collect();
    }
    let default = "all".to_string();
    prompt_multi_select(question, labels, &default)
}

fn parse_selection_string(input: &str, max_val: usize) -> Vec<usize> {
    let t = input.trim();
    if t.eq_ignore_ascii_case("all") {
        return (0..max_val).collect();
    }
    let is_pure_digits = !t.is_empty() && t.chars().all(|c| c.is_ascii_digit());
    if is_pure_digits && max_val < 10 {
        parse_digit_sequence(t, max_val)
    } else {
        parse_separated_list(t, max_val)
    }
}

fn parse_digit_sequence(input: &str, max_val: usize) -> Vec<usize> {
    let mut out = Vec::new();
    for d in input
        .chars()
        .filter_map(|c| c.to_digit(10))
        .map(|d| d as usize)
    {
        if d >= 1 && d <= max_val && !out.contains(&(d - 1)) {
            out.push(d - 1);
        }
    }
    out
}

fn parse_separated_list(input: &str, max_val: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let normalized = input.replace([',', '.', ';'], " ");
    for n in normalized
        .split_whitespace()
        .filter_map(|p| p.parse::<usize>().ok())
        .filter(|&n| n >= 1 && n <= max_val)
        .map(|n| n - 1)
    {
        if !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

// ── Platform path helpers (mirrors setup.rs) ──────────────────────────────────

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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    // ── remove_mcp_entry ──────────────────────────────────────────────────────

    #[test]
    fn remove_mcp_entry_removes_ahma_key() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"Ahma":{"type":"stdio"},"Other":{"type":"stdio"}}}"#,
        )?;

        remove_mcp_entry(&path, "mcpServers", false)?;

        let content = std::fs::read_to_string(&path)?;
        let parsed: Value = serde_json::from_str(&content)?;
        assert!(
            parsed["mcpServers"]["Ahma"].is_null(),
            "Ahma key should be gone"
        );
        assert_eq!(
            parsed["mcpServers"]["Other"]["type"], "stdio",
            "Other key preserved"
        );
        Ok(())
    }

    #[test]
    fn remove_mcp_entry_prunes_empty_servers_object() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"Ahma":{"type":"stdio"}},"other":"val"}"#,
        )?;

        remove_mcp_entry(&path, "mcpServers", false)?;

        let content = std::fs::read_to_string(&path)?;
        let parsed: Value = serde_json::from_str(&content)?;
        assert!(
            parsed.as_object().unwrap().get("mcpServers").is_none(),
            "empty mcpServers should be pruned"
        );
        assert_eq!(parsed["other"], "val", "other content preserved");
        Ok(())
    }

    #[test]
    fn remove_mcp_entry_noop_when_file_missing() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("nonexistent.json");
        // Should succeed silently.
        remove_mcp_entry(&path, "mcpServers", false)?;
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn remove_mcp_entry_noop_when_ahma_absent() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("mcp.json");
        std::fs::write(&path, r#"{"mcpServers":{"Other":{"type":"stdio"}}}"#)?;

        remove_mcp_entry(&path, "mcpServers", false)?;

        let content = std::fs::read_to_string(&path)?;
        let parsed: Value = serde_json::from_str(&content)?;
        assert!(
            !parsed["mcpServers"]["Other"].is_null(),
            "Other key preserved"
        );
        Ok(())
    }

    #[test]
    fn remove_mcp_entry_dry_run_does_not_modify() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("mcp.json");
        let original = r#"{"mcpServers":{"Ahma":{"type":"stdio"}}}"#;
        std::fs::write(&path, original)?;

        remove_mcp_entry(&path, "mcpServers", true)?;

        let content = std::fs::read_to_string(&path)?;
        assert_eq!(content, original, "dry-run must not modify file");
        Ok(())
    }

    #[test]
    fn round_trip_merge_then_remove_mcp() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("mcp.json");
        let prior = r#"{"mcpServers":{"Other":{"type":"stdio"}},"extra":"kept"}"#;
        std::fs::write(&path, prior)?;

        // Install Ahma (same logic as setup.rs merge_mcp_json)
        crate::setup::merge_mcp_json(
            &path,
            "mcpServers",
            json!({"type":"stdio","command":"ahma"}),
        )?;
        let after_install: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        assert!(!after_install["mcpServers"]["Ahma"].is_null());

        // Remove Ahma
        remove_mcp_entry(&path, "mcpServers", false)?;
        let after_remove: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        assert!(
            after_remove["mcpServers"]["Ahma"].is_null(),
            "Ahma gone after remove"
        );
        assert!(
            !after_remove["mcpServers"]["Other"].is_null(),
            "Other preserved"
        );
        assert_eq!(after_remove["extra"], "kept", "extra preserved");
        Ok(())
    }

    // ── remove_codex_mcp ──────────────────────────────────────────────────────

    #[test]
    fn remove_codex_mcp_removes_ahma_section() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "[other]\nkey = \"val\"\n[mcp_servers.Ahma]\ncommand = \"ahma\"\n",
        )?;

        remove_codex_mcp(&path, false)?;

        let content = std::fs::read_to_string(&path)?;
        let parsed: toml::Value = toml::from_str(&content)?;
        assert!(
            parsed
                .get("mcp_servers")
                .and_then(|s| s.get("Ahma"))
                .is_none(),
            "Ahma section removed"
        );
        assert_eq!(
            parsed["other"]["key"].as_str(),
            Some("val"),
            "other preserved"
        );
        Ok(())
    }

    #[test]
    fn remove_codex_mcp_prunes_empty_mcp_servers() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "[mcp_servers.Ahma]\ncommand = \"ahma\"\n")?;

        remove_codex_mcp(&path, false)?;

        let content = std::fs::read_to_string(&path)?;
        let parsed: toml::Value = toml::from_str(&content)?;
        assert!(
            parsed.get("mcp_servers").is_none(),
            "empty mcp_servers pruned"
        );
        Ok(())
    }

    #[test]
    fn remove_codex_mcp_dry_run_no_write() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("config.toml");
        let original = "[mcp_servers.Ahma]\ncommand = \"ahma\"\n";
        std::fs::write(&path, original)?;

        remove_codex_mcp(&path, true)?;

        assert_eq!(std::fs::read_to_string(&path)?, original);
        Ok(())
    }

    // ── Claude plugin teardown ────────────────────────────────────────────────

    #[test]
    fn remove_installed_plugin_entry_removes_key() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("installed_plugins.json");
        std::fs::write(
            &path,
            r#"{"version":2,"plugins":{"ahma@local":[{}],"other@local":[{}]}}"#,
        )?;

        remove_installed_plugin_entry(&path, "ahma@local", false)?;

        let content = std::fs::read_to_string(&path)?;
        let parsed: Value = serde_json::from_str(&content)?;
        assert!(
            parsed["plugins"]["ahma@local"].is_null(),
            "ahma@local removed"
        );
        assert!(
            !parsed["plugins"]["other@local"].is_null(),
            "other@local preserved"
        );
        Ok(())
    }

    #[test]
    fn disable_claude_plugin_removes_enabled_entry() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"enabledPlugins":{"ahma@local":true,"other@local":true},"hooks":{}}"#,
        )?;

        disable_claude_plugin(&path, "ahma@local", false)?;

        let content = std::fs::read_to_string(&path)?;
        let parsed: Value = serde_json::from_str(&content)?;
        assert!(
            parsed["enabledPlugins"]["ahma@local"].is_null(),
            "ahma@local disabled"
        );
        assert_eq!(
            parsed["enabledPlugins"]["other@local"], true,
            "other plugin preserved"
        );
        // hooks key untouched
        assert!(parsed["hooks"].is_object(), "hooks preserved");
        Ok(())
    }

    #[test]
    fn disable_claude_plugin_dry_run_no_write() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("settings.json");
        let original = r#"{"enabledPlugins":{"ahma@local":true}}"#;
        std::fs::write(&path, original)?;

        disable_claude_plugin(&path, "ahma@local", true)?;

        assert_eq!(std::fs::read_to_string(&path)?, original);
        Ok(())
    }

    // ── Selection logic ───────────────────────────────────────────────────────

    #[test]
    fn select_actions_returns_flagged_subset() {
        let args = UninstallArgs {
            auto: false,
            mcp: true,
            hooks: false,
            skills: false,
            binary: false,
            platforms: Vec::new(),
            purge: false,
            dry_run: false,
        };
        let actions = select_actions(&args, false);
        assert_eq!(actions, vec![UninstallAction::Mcp]);
    }

    #[test]
    fn select_actions_auto_returns_all() {
        let args = UninstallArgs {
            auto: true,
            mcp: false,
            hooks: false,
            skills: false,
            binary: false,
            platforms: Vec::new(),
            purge: false,
            dry_run: false,
        };
        let actions = select_actions(&args, false);
        assert_eq!(actions.len(), UNINSTALL_ACTIONS.len());
    }

    #[test]
    fn select_platforms_filters_by_cli_name() {
        let actions = vec![UninstallAction::Mcp];
        let platforms = select_platforms(&actions, &["cursor".to_string()], false);
        assert_eq!(platforms.len(), 1);
        assert_eq!(platforms[0].cli_name(), "cursor");
    }

    #[test]
    fn parse_selection_string_all() {
        let result = parse_selection_string("all", 3);
        assert_eq!(result, vec![0, 1, 2]);
    }

    #[test]
    fn parse_selection_string_comma_separated() {
        let result = parse_selection_string("1,3", 4);
        assert_eq!(result, vec![0, 2]);
    }
}
