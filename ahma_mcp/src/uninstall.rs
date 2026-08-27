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

use crate::harness_target::{McpConfigFormat, PLATFORMS, Platform};
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

// ── Entry point ──────────────────────────────────────────────────────────────

/// Runs the uninstall wizard.
pub async fn run(args: UninstallArgs) -> Result<()> {
    let interactive = !args.auto && io::stdin().is_terminal() && io::stdout().is_terminal();

    if interactive {
        print_banner("Ahma Uninstall Wizard");
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
    let purge = should_purge_ahma_dir(&args, &actions, interactive)?;

    execute_actions(&actions, &platforms, args.dry_run, purge, interactive).await?;

    if interactive {
        print_banner("Uninstall completed!");
    }

    Ok(())
}

/// Prints a boxed banner with the given title line (interactive mode only).
fn print_banner(title: &str) {
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  {}", title);
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();
}

/// Decides whether to purge `~/.ahma`: forced via `--purge`, or (in interactive
/// mode, when at least one non-platform-specific action was selected) by
/// prompting the user. Non-interactive/non-forced runs default to `false`.
fn should_purge_ahma_dir(
    args: &UninstallArgs,
    actions: &[UninstallAction],
    interactive: bool,
) -> Result<bool> {
    if args.purge {
        return Ok(true);
    }
    if !interactive || !actions.iter().any(|a| !a.is_platform_specific()) {
        return Ok(false);
    }
    prompt_yes_no(
        "Also remove all ahma data and config in ~/.ahma? \
(TLS, prompts, settings, logs) [y/N]: ",
    )
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
        return filter_platforms_by_cli_args(relevant, platform_filter);
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

fn filter_platforms_by_cli_args(
    relevant: Vec<Platform>,
    platform_filter: &[String],
) -> Vec<Platform> {
    relevant
        .into_iter()
        .filter(|p| {
            platform_filter
                .iter()
                .any(|f| f.eq_ignore_ascii_case(p.cli_name()) || f.eq_ignore_ascii_case(p.label()))
        })
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
    let home = ahma_common::config::ahma_home_dir()
        .ok_or_else(|| anyhow!("Could not resolve home directory"))?;
    let mut removed = Vec::new();

    for platform in platforms.iter().copied().filter(|p| p.supports_mcp()) {
        if let Some(name) = remove_platform_mcp(platform, &home, dry_run)? {
            if dry_run {
                println!("[dry-run] Would remove Ahma MCP entry from {}", name);
            }
            removed.push(name);
        }
    }

    Ok(removed)
}

/// Remove the Ahma MCP entry for a single platform.
///
/// Returns the platform's display name when an entry was targeted, or `None`
/// when the platform has no MCP config path (or does not support MCP).
fn remove_platform_mcp(
    platform: Platform,
    home: &Path,
    dry_run: bool,
) -> Result<Option<&'static str>> {
    let Some((path, format)) = platform.mcp_config(home) else {
        return Ok(None);
    };

    match format {
        McpConfigFormat::Toml => remove_codex_mcp(&path, dry_run),
        McpConfigFormat::Json(servers_key) => remove_mcp_entry(&path, servers_key, dry_run),
    }
    .with_context(|| {
        format!(
            "{} MCP config at {}",
            platform.mcp_display_name(),
            path.display()
        )
    })?;

    Ok(Some(platform.mcp_display_name()))
}

/// Remove the `"Ahma"` key from `config[servers_key]` in a JSON MCP config file.
///
/// Preserves all other content, including any other MCP servers.  If Ahma was the
/// only server, the now-empty `servers_key` object is left in place (e.g.
/// `{ "mcpServers": {} }`) — this is the canonical minimal MCP config format, so the
/// file stays valid rather than becoming "empty".  Never deletes the file itself.
/// No-ops gracefully when the file or key is absent.
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

    let Some(servers_obj) = servers_val.as_object_mut() else {
        return Ok(());
    };
    if servers_obj.remove("Ahma").is_none() {
        return Ok(()); // Ahma key already absent
    }
    // Intentionally leave the (possibly now-empty) servers object in place.
    // `{ "mcpServers": {} }` is the canonical minimal MCP config, so we keep it
    // rather than pruning the key and risking an "empty" file.

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
    let home = ahma_common::config::ahma_home_dir()
        .ok_or_else(|| anyhow!("Could not resolve home directory"))?;

    for skill_dir in crate::setup::skill_install_dirs(&home) {
        if !skill_dir.exists() {
            continue;
        }
        if dry_run {
            println!("[dry-run] Would remove {}", skill_dir.display());
            continue;
        }
        std::fs::remove_dir_all(&skill_dir)
            .with_context(|| format!("Failed to remove {}", skill_dir.display()))?;
        if interactive {
            println!("✓ Removed agent skill directory {}", skill_dir.display());
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
    uninstall_binary_in(&resolve_install_dir()?, dry_run)
}

/// Remove the installed binary from `install_dir`.
///
/// Split out from [`uninstall_binary`] so tests can point at a temp directory directly
/// instead of steering the resolution with an environment variable. That mattered: the
/// only reason these tests used to set `AHMA_INSTALL_DIR` was to keep themselves from
/// deleting the developer's real `~/.local/bin/ahma`, which made a retired variable
/// look load-bearing when the real requirement was just an injectable parameter.
fn uninstall_binary_in(install_dir: &Path, dry_run: bool) -> Result<()> {
    let binary_name = crate::update::AHMA_BINARY_NAME;
    let binary_path = install_dir.join(binary_name);
    let old_path = install_dir.join(if cfg!(windows) {
        "ahma.old.exe"
    } else {
        "ahma.old"
    });

    if dry_run {
        print_dry_run_binary_removal(&[&binary_path, &old_path]);
        print_custom_install_hint(install_dir);
        return Ok(());
    }

    #[cfg(unix)]
    remove_binary_unix(&binary_path, &old_path)?;

    #[cfg(windows)]
    print_windows_removal_instructions(&binary_path, &old_path);

    print_custom_install_hint(install_dir);

    Ok(())
}

/// Print [`custom_install_hint`] when the running binary lives outside `install_dir`.
fn print_custom_install_hint(install_dir: &Path) {
    let exe = std::env::current_exe().ok();
    if let Some(hint) = custom_install_hint(install_dir, exe.as_deref()) {
        println!("{hint}");
    }
}

/// Print `[dry-run] Would remove ...` for each path that exists.
fn print_dry_run_binary_removal(paths: &[&Path]) {
    for path in paths {
        if path.exists() {
            println!("[dry-run] Would remove {}", path.display());
        }
    }
}

/// Delete the installed binary (and any leftover `ahma.old`) on Unix.
#[cfg(unix)]
fn remove_binary_unix(binary_path: &Path, old_path: &Path) -> Result<()> {
    if old_path.exists() {
        let _ = std::fs::remove_file(old_path);
    }
    if binary_path.exists() {
        std::fs::remove_file(binary_path)
            .with_context(|| format!("Failed to remove {}", binary_path.display()))?;
        println!("✓ Removed {}", binary_path.display());
    } else {
        println!(
            "  Binary not found at {} — nothing to remove.",
            binary_path.display()
        );
    }
    Ok(())
}

/// A running Windows `.exe` cannot delete itself, so print manual removal steps.
#[cfg(windows)]
fn print_windows_removal_instructions(binary_path: &Path, old_path: &Path) {
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

/// Resolve the directory where the binary was installed.
///
/// Delegates to [`crate::update::resolve_install_dir`] so uninstall deletes from exactly
/// the directory update installs into. `AHMA_INSTALL_DIR` is retired (R-CFG1.2) and is
/// warn-and-ignored there; an env var that decides which binary gets deleted is ambient
/// input with real teeth.
///
/// `ahma uninstall` has no `--install-dir` flag yet, so a non-default installation is
/// reported rather than removed — see [`custom_install_hint`].
fn resolve_install_dir() -> Result<PathBuf> {
    crate::update::resolve_install_dir(None)
}

/// A hint naming the *running* executable when it does not live in `install_dir`.
///
/// Replaces the one thing `AHMA_INSTALL_DIR` was genuinely useful for — uninstalling a
/// binary that was installed somewhere other than `~/.local/bin`. Rather than trusting
/// the environment to name that directory, ahma reports the path it can actually prove:
/// the executable currently running. The user deletes it themselves, so nothing outside
/// the default location is removed on the say-so of an inherited variable.
///
/// Returns `None` when the running executable is inside `install_dir` (nothing to add)
/// or when the current executable path cannot be determined.
fn custom_install_hint(install_dir: &Path, current_exe: Option<&Path>) -> Option<String> {
    let exe = current_exe?;
    let exe_dir = exe.parent()?;
    // Case-insensitive filesystems still compare case-sensitively via `Path`, so
    // canonicalize both sides before deciding they differ (AGENTS.md, Windows note).
    let same = match (
        dunce::canonicalize(exe_dir),
        dunce::canonicalize(install_dir),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => exe_dir == install_dir,
    };
    if same {
        return None;
    }
    Some(format!(
        "  Note: the ahma you are running is {}, which is outside {}.\n  \
         `ahma uninstall` only removes the binary from the default install directory; \
         delete that file yourself if you no longer want it.",
        exe.display(),
        install_dir.display()
    ))
}

// ── Purge ~/.ahma ─────────────────────────────────────────────────────────────

fn purge_ahma_dir(dry_run: bool) -> Result<()> {
    let home = ahma_common::config::ahma_home_dir()
        .ok_or_else(|| anyhow!("Could not resolve home directory"))?;
    let ahma_dir = home.join(".ahma");
    // Legacy: `ahma setup` used to pre-create ~/sandbox and point roots-less
    // clients at it. It no longer does (SPEC R5.2.3 — ahma must not invent a
    // scope), but uninstall still cleans up what older versions left behind.
    let legacy_sandbox_dir = home.join("sandbox");

    if dry_run {
        if ahma_dir.exists() {
            println!("[dry-run] Would remove {}", ahma_dir.display());
        }
        if legacy_sandbox_dir.exists() {
            println!(
                "[dry-run] Would remove {} (legacy sandbox directory)",
                legacy_sandbox_dir.display()
            );
        }
        return Ok(());
    }

    if ahma_dir.exists() {
        std::fs::remove_dir_all(&ahma_dir)
            .with_context(|| format!("Failed to remove {}", ahma_dir.display()))?;
        println!("✓ Removed {}", ahma_dir.display());
    }

    remove_legacy_sandbox_dir_if_empty(&legacy_sandbox_dir);

    Ok(())
}

/// Remove the legacy `~/sandbox` directory, but only if it is empty.
///
/// Only removes it if it appears to have been created by an older `ahma setup`
/// (i.e. is empty). We do NOT forcibly delete a non-empty user directory named
/// "sandbox".
fn remove_legacy_sandbox_dir_if_empty(sandbox_dir: &Path) {
    if !sandbox_dir.exists() {
        return;
    }
    let entries: Vec<_> = std::fs::read_dir(sandbox_dir)
        .map(|rd| rd.flatten().collect::<Vec<_>>())
        .unwrap_or_default();
    if entries.is_empty() {
        let _ = std::fs::remove_dir(sandbox_dir);
        println!(
            "✓ Removed empty legacy sandbox dir {}",
            sandbox_dir.display()
        );
    } else {
        println!(
            "  Skipping non-empty {} — remove manually if desired.",
            sandbox_dir.display()
        );
    }
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
    fn remove_mcp_entry_keeps_empty_servers_object() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"Ahma":{"type":"stdio"}},"other":"val"}"#,
        )?;

        remove_mcp_entry(&path, "mcpServers", false)?;

        let content = std::fs::read_to_string(&path)?;
        let parsed: Value = serde_json::from_str(&content)?;
        let servers = parsed
            .as_object()
            .unwrap()
            .get("mcpServers")
            .expect("mcpServers key must be retained as the minimal config");
        assert!(
            servers.as_object().unwrap().is_empty(),
            "mcpServers should be kept as an empty object, not pruned"
        );
        assert_eq!(parsed["other"], "val", "other content preserved");
        Ok(())
    }

    #[test]
    fn remove_mcp_entry_only_ahma_leaves_minimal_config() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("mcp.json");
        std::fs::write(&path, r#"{"mcpServers":{"Ahma":{"type":"stdio"}}}"#)?;

        remove_mcp_entry(&path, "mcpServers", false)?;

        let parsed: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        let obj = parsed.as_object().expect("top level object");
        assert_eq!(obj.len(), 1, "only mcpServers should remain");
        assert!(
            obj.get("mcpServers")
                .unwrap()
                .as_object()
                .unwrap()
                .is_empty(),
            "Cursor minimal format {{\"mcpServers\":{{}}}} must be preserved"
        );
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

    // ── Env-serialized helpers ────────────────────────────────────────────────

    use std::sync::{LazyLock, Mutex};

    static ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Override the home directory so [`ahma_common::config::ahma_home_dir`]
    /// resolves into a temp directory.  Returns the previous value for
    /// restoration.
    ///
    /// Uses the `AHMA_TEST_HOME` override rather than `HOME`/`USERPROFILE`
    /// because `dirs::home_dir()` ignores both env vars on Windows (it calls
    /// `SHGetKnownFolderPath`), so they cannot redirect home resolution there.
    fn set_home(tmp: &Path) -> Option<std::ffi::OsString> {
        let prev = std::env::var_os("AHMA_TEST_HOME");
        // SAFETY: test-only, serialized via ENV_MUTEX.
        unsafe {
            std::env::set_var("AHMA_TEST_HOME", tmp);
        }
        prev
    }

    fn restore_home(prev: Option<std::ffi::OsString>) {
        // SAFETY: test-only, serialized via ENV_MUTEX.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("AHMA_TEST_HOME", v),
                None => std::env::remove_var("AHMA_TEST_HOME"),
            }
        }
    }

    // ── remove_mcp_entry edge branches ────────────────────────────────────────

    #[test]
    fn remove_mcp_entry_malformed_json_is_noop() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("mcp.json");
        let original = "{not valid json";
        std::fs::write(&path, original)?;
        // Malformed → falls back to empty object → no servers key → Ok, no write.
        remove_mcp_entry(&path, "mcpServers", false)?;
        assert_eq!(std::fs::read_to_string(&path)?, original);
        Ok(())
    }

    #[test]
    fn remove_mcp_entry_non_object_top_level_is_noop() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("mcp.json");
        let original = "[1,2,3]";
        std::fs::write(&path, original)?;
        remove_mcp_entry(&path, "mcpServers", false)?;
        assert_eq!(std::fs::read_to_string(&path)?, original);
        Ok(())
    }

    #[test]
    fn remove_mcp_entry_servers_not_object_is_noop() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("mcp.json");
        let original = r#"{"mcpServers":"oops"}"#;
        std::fs::write(&path, original)?;
        remove_mcp_entry(&path, "mcpServers", false)?;
        assert_eq!(std::fs::read_to_string(&path)?, original);
        Ok(())
    }

    // ── remove_codex_mcp edge branches ────────────────────────────────────────

    #[test]
    fn remove_codex_mcp_noop_when_file_missing() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("missing.toml");
        remove_codex_mcp(&path, false)?;
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn remove_codex_mcp_ahma_absent_preserved() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("config.toml");
        let original = "[mcp_servers.Other]\ncommand = \"x\"\n";
        std::fs::write(&path, original)?;
        remove_codex_mcp(&path, false)?;
        let parsed: toml::Value = toml::from_str(&std::fs::read_to_string(&path)?)?;
        assert!(parsed["mcp_servers"]["Other"].get("command").is_some());
        Ok(())
    }

    #[test]
    fn remove_codex_mcp_malformed_is_noop() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("config.toml");
        let original = "this is = = not toml [[[";
        std::fs::write(&path, original)?;
        // Parse failure → empty table → no mcp_servers → Ok, no write.
        remove_codex_mcp(&path, false)?;
        assert_eq!(std::fs::read_to_string(&path)?, original);
        Ok(())
    }

    #[test]
    fn remove_codex_mcp_servers_not_table() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "mcp_servers = \"scalar\"\n")?;
        // mcp_servers present but not a table → if-let skipped → re-serialize unchanged.
        remove_codex_mcp(&path, false)?;
        let parsed: toml::Value = toml::from_str(&std::fs::read_to_string(&path)?)?;
        assert_eq!(parsed["mcp_servers"].as_str(), Some("scalar"));
        Ok(())
    }

    // ── installed_plugins / disable edge branches ─────────────────────────────

    #[test]
    fn remove_installed_plugin_entry_noop_when_missing() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("missing.json");
        remove_installed_plugin_entry(&path, "ahma@local", false)?;
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn remove_installed_plugin_entry_key_absent_preserved() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("installed_plugins.json");
        let original = r#"{"version":2,"plugins":{"other@local":[{}]}}"#;
        std::fs::write(&path, original)?;
        remove_installed_plugin_entry(&path, "ahma@local", false)?;
        let parsed: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        assert!(!parsed["plugins"]["other@local"].is_null());
        Ok(())
    }

    #[test]
    fn remove_installed_plugin_entry_plugins_not_object() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("installed_plugins.json");
        let original = r#"{"version":2,"plugins":"oops"}"#;
        std::fs::write(&path, original)?;
        remove_installed_plugin_entry(&path, "ahma@local", false)?;
        assert_eq!(std::fs::read_to_string(&path)?, original);
        Ok(())
    }

    #[test]
    fn remove_installed_plugin_entry_dry_run_no_write() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("installed_plugins.json");
        let original = r#"{"version":2,"plugins":{"ahma@local":[{}]}}"#;
        std::fs::write(&path, original)?;
        remove_installed_plugin_entry(&path, "ahma@local", true)?;
        assert_eq!(std::fs::read_to_string(&path)?, original);
        Ok(())
    }

    #[test]
    fn disable_claude_plugin_noop_when_missing() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("missing.json");
        disable_claude_plugin(&path, "ahma@local", false)?;
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn disable_claude_plugin_enabled_section_absent() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("settings.json");
        let original = r#"{"hooks":{}}"#;
        std::fs::write(&path, original)?;
        disable_claude_plugin(&path, "ahma@local", false)?;
        assert_eq!(std::fs::read_to_string(&path)?, original);
        Ok(())
    }

    #[test]
    fn disable_claude_plugin_key_absent_preserved() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("settings.json");
        let original = r#"{"enabledPlugins":{"other@local":true}}"#;
        std::fs::write(&path, original)?;
        disable_claude_plugin(&path, "ahma@local", false)?;
        let parsed: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        assert_eq!(parsed["enabledPlugins"]["other@local"], true);
        Ok(())
    }

    // ── remove_claude_plugin (home-injected) ──────────────────────────────────

    #[test]
    fn remove_claude_plugin_full_teardown() -> Result<()> {
        let tmp = tempdir()?;
        let home = tmp.path();
        let plugins_cache = home
            .join(".claude")
            .join("plugins")
            .join("cache")
            .join("local")
            .join("ahma")
            .join("0.1.0");
        std::fs::create_dir_all(&plugins_cache)?;
        std::fs::write(plugins_cache.join("marker.txt"), "x")?;

        let installed = home
            .join(".claude")
            .join("plugins")
            .join("installed_plugins.json");
        std::fs::write(
            &installed,
            r#"{"version":2,"plugins":{"ahma@local":[{}],"keep@local":[{}]}}"#,
        )?;

        let settings = home.join(".claude").join("settings.json");
        std::fs::write(
            &settings,
            r#"{"enabledPlugins":{"ahma@local":true,"keep@local":true}}"#,
        )?;

        remove_claude_plugin(home, false, false)?;

        let cache_root = home
            .join(".claude")
            .join("plugins")
            .join("cache")
            .join("local")
            .join("ahma");
        assert!(!cache_root.exists(), "plugin cache tree removed");

        let installed_parsed: Value = serde_json::from_str(&std::fs::read_to_string(&installed)?)?;
        assert!(installed_parsed["plugins"]["ahma@local"].is_null());
        assert!(!installed_parsed["plugins"]["keep@local"].is_null());

        let settings_parsed: Value = serde_json::from_str(&std::fs::read_to_string(&settings)?)?;
        assert!(settings_parsed["enabledPlugins"]["ahma@local"].is_null());
        assert_eq!(settings_parsed["enabledPlugins"]["keep@local"], true);
        Ok(())
    }

    #[test]
    fn remove_claude_plugin_dry_run_keeps_cache() -> Result<()> {
        let tmp = tempdir()?;
        let home = tmp.path();
        let plugins_cache = home
            .join(".claude")
            .join("plugins")
            .join("cache")
            .join("local")
            .join("ahma");
        std::fs::create_dir_all(&plugins_cache)?;
        remove_claude_plugin(home, true, false)?;
        assert!(plugins_cache.exists(), "dry-run must not remove cache");
        Ok(())
    }

    #[test]
    fn remove_claude_plugin_nothing_present_is_ok() -> Result<()> {
        let tmp = tempdir()?;
        // No ~/.claude tree at all — should succeed silently.
        remove_claude_plugin(tmp.path(), false, true)?;
        Ok(())
    }

    // ── remove_platform_mcp per-platform branches ─────────────────────────────

    fn write_json(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn remove_platform_mcp_claude_code() -> Result<()> {
        let tmp = tempdir()?;
        let home = tmp.path();
        write_json(
            &home.join(".claude.json"),
            r#"{"mcpServers":{"Ahma":{"type":"stdio"}}}"#,
        );
        let name = remove_platform_mcp(Platform::ClaudeCode, home, false)?;
        assert_eq!(name, Some("Claude Code"));
        let parsed: Value =
            serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json"))?)?;
        let servers = parsed
            .as_object()
            .unwrap()
            .get("mcpServers")
            .expect("mcpServers retained as minimal config");
        assert!(servers.as_object().unwrap().is_empty());
        Ok(())
    }

    #[test]
    fn remove_platform_mcp_cursor() -> Result<()> {
        let tmp = tempdir()?;
        let home = tmp.path();
        write_json(
            &home.join(".cursor").join("mcp.json"),
            r#"{"mcpServers":{"Ahma":{"type":"stdio"},"Keep":{"type":"stdio"}}}"#,
        );
        let name = remove_platform_mcp(Platform::Cursor, home, false)?;
        assert_eq!(name, Some("Cursor"));
        let parsed: Value = serde_json::from_str(&std::fs::read_to_string(
            home.join(".cursor").join("mcp.json"),
        )?)?;
        assert!(parsed["mcpServers"]["Ahma"].is_null());
        assert!(!parsed["mcpServers"]["Keep"].is_null());
        Ok(())
    }

    #[test]
    fn remove_platform_mcp_antigravity() -> Result<()> {
        let tmp = tempdir()?;
        let home = tmp.path();
        write_json(
            &home.join(".gemini").join("config").join("mcp_config.json"),
            r#"{"mcpServers":{"Ahma":{"type":"stdio"}}}"#,
        );
        let name = remove_platform_mcp(Platform::Antigravity, home, false)?;
        assert_eq!(name, Some("Antigravity"));
        Ok(())
    }

    #[test]
    fn remove_platform_mcp_lmstudio() -> Result<()> {
        let tmp = tempdir()?;
        let home = tmp.path();
        write_json(
            &home.join(".lmstudio").join("mcp.json"),
            r#"{"mcpServers":{"Ahma":{"type":"stdio"}}}"#,
        );
        let name = remove_platform_mcp(Platform::LmStudio, home, false)?;
        assert_eq!(name, Some("LM Studio"));
        Ok(())
    }

    #[test]
    fn remove_platform_mcp_codex() -> Result<()> {
        let tmp = tempdir()?;
        let home = tmp.path();
        let cfg = home.join(".codex").join("config.toml");
        std::fs::create_dir_all(cfg.parent().unwrap())?;
        std::fs::write(&cfg, "[mcp_servers.Ahma]\ncommand = \"ahma\"\n")?;
        let name = remove_platform_mcp(Platform::Codex, home, false)?;
        assert_eq!(name, Some("Codex CLI"));
        let parsed: toml::Value = toml::from_str(&std::fs::read_to_string(&cfg)?)?;
        assert!(parsed.get("mcp_servers").is_none());
        Ok(())
    }

    #[test]
    fn remove_platform_mcp_copilot_is_none() -> Result<()> {
        let tmp = tempdir()?;
        let name = remove_platform_mcp(Platform::Copilot, tmp.path(), false)?;
        assert_eq!(name, None);
        Ok(())
    }

    #[test]
    fn remove_platform_mcp_vscode_and_desktop_dry_run() -> Result<()> {
        // These use real-home-derived paths internally; drive with dry_run so no
        // mutation occurs, but the branch returning Some(name) is exercised.
        let tmp = tempdir()?;
        assert_eq!(
            remove_platform_mcp(Platform::VsCode, tmp.path(), true)?,
            Some("VS Code (GitHub Copilot Chat)")
        );
        assert_eq!(
            remove_platform_mcp(Platform::ClaudeDesktop, tmp.path(), true)?,
            Some("Claude Desktop")
        );
        Ok(())
    }

    // ── uninstall_mcp_config (home-injected via HOME) ─────────────────────────

    #[test]
    fn uninstall_mcp_config_collects_removed_names() -> Result<()> {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = tempdir()?;
        let prev = set_home(tmp.path());
        write_json(
            &tmp.path().join(".cursor").join("mcp.json"),
            r#"{"mcpServers":{"Ahma":{"type":"stdio"}}}"#,
        );
        let result = uninstall_mcp_config(&[Platform::Cursor, Platform::Copilot], false);
        restore_home(prev);
        let removed = result?;
        // Copilot has no MCP path (filtered by supports_mcp); Cursor yields a name.
        assert_eq!(removed, vec!["Cursor"]);
        Ok(())
    }

    #[test]
    fn uninstall_mcp_config_empty_platforms() -> Result<()> {
        let removed = uninstall_mcp_config(&[], false)?;
        assert!(removed.is_empty());
        Ok(())
    }

    // ── uninstall_terminal_hooks ──────────────────────────────────────────────

    #[test]
    fn uninstall_terminal_hooks_empty_when_no_hook_platforms() -> Result<()> {
        // VsCode does not support hooks → hook_platforms empty → early return.
        let removed = uninstall_terminal_hooks(&[Platform::VsCode], false)?;
        assert!(removed.is_empty());
        Ok(())
    }

    // ── uninstall_agent_skills (home-injected) ────────────────────────────────

    #[test]
    fn uninstall_agent_skills_removes_skill_dir() -> Result<()> {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = tempdir()?;
        let prev = set_home(tmp.path());
        let skill_dirs = crate::setup::skill_install_dirs(tmp.path());
        for skill_dir in &skill_dirs {
            std::fs::create_dir_all(skill_dir).ok();
            std::fs::write(skill_dir.join("SKILL.md"), "x").ok();
        }
        let result = uninstall_agent_skills(false, false);
        let still_there: Vec<_> = skill_dirs.iter().filter(|d| d.exists()).collect();
        restore_home(prev);
        result?;
        assert!(
            still_there.is_empty(),
            "all skill dirs should be removed, still present: {still_there:?}"
        );
        Ok(())
    }

    #[test]
    fn uninstall_agent_skills_dry_run_keeps_dir() -> Result<()> {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = tempdir()?;
        let prev = set_home(tmp.path());
        let skill_dirs = crate::setup::skill_install_dirs(tmp.path());
        for skill_dir in &skill_dirs {
            std::fs::create_dir_all(skill_dir).ok();
        }
        let result = uninstall_agent_skills(true, true);
        let all_still_there = skill_dirs.iter().all(|d| d.exists());
        restore_home(prev);
        result?;
        assert!(all_still_there, "dry-run keeps skill dirs");
        Ok(())
    }

    // ── purge_ahma_dir (home-injected) ────────────────────────────────────────

    #[test]
    fn purge_ahma_dir_removes_data_and_empty_sandbox() -> Result<()> {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = tempdir()?;
        let prev = set_home(tmp.path());
        let ahma_dir = tmp.path().join(".ahma");
        std::fs::create_dir_all(&ahma_dir).ok();
        std::fs::write(ahma_dir.join("settings.json"), "{}").ok();
        let sandbox = tmp.path().join("sandbox");
        std::fs::create_dir_all(&sandbox).ok();
        let result = purge_ahma_dir(false);
        let ahma_gone = !ahma_dir.exists();
        let sandbox_gone = !sandbox.exists();
        restore_home(prev);
        result?;
        assert!(ahma_gone, ".ahma removed");
        assert!(sandbox_gone, "empty sandbox removed");
        Ok(())
    }

    #[test]
    fn purge_ahma_dir_keeps_nonempty_sandbox() -> Result<()> {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = tempdir()?;
        let prev = set_home(tmp.path());
        let sandbox = tmp.path().join("sandbox");
        std::fs::create_dir_all(&sandbox).ok();
        std::fs::write(sandbox.join("important.txt"), "user data").ok();
        let result = purge_ahma_dir(false);
        let still_there = sandbox.exists();
        restore_home(prev);
        result?;
        assert!(still_there, "non-empty sandbox preserved");
        Ok(())
    }

    #[test]
    fn purge_ahma_dir_dry_run_keeps_everything() -> Result<()> {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = tempdir()?;
        let prev = set_home(tmp.path());
        let ahma_dir = tmp.path().join(".ahma");
        std::fs::create_dir_all(&ahma_dir).ok();
        let result = purge_ahma_dir(true);
        let still_there = ahma_dir.exists();
        restore_home(prev);
        result?;
        assert!(still_there, "dry-run keeps .ahma");
        Ok(())
    }

    // ── resolve_install_dir / uninstall_binary ────────────────────────────────

    /// R-CFG1.2: `AHMA_INSTALL_DIR` is retired. `ahma uninstall` must delete from the
    /// default install directory even when the variable names a different one —
    /// otherwise an inherited variable decides which binary gets removed.
    #[test]
    fn resolve_install_dir_ignores_retired_env() -> Result<()> {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = tempdir()?;
        let prev = std::env::var_os("AHMA_INSTALL_DIR");
        // SAFETY: test-only, serialized via ENV_MUTEX.
        unsafe { std::env::set_var("AHMA_INSTALL_DIR", tmp.path()) };
        let dir = resolve_install_dir();
        let warned = crate::warn_retired_env("AHMA_INSTALL_DIR");
        // SAFETY: restore.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("AHMA_INSTALL_DIR", v),
                None => std::env::remove_var("AHMA_INSTALL_DIR"),
            }
        }
        assert!(warned, "a set retired variable must still be warned about");
        let dir = dir?;
        assert_ne!(
            dir,
            tmp.path(),
            "AHMA_INSTALL_DIR is retired and must not steer uninstall"
        );
        assert_eq!(
            dir,
            crate::update::default_install_dir()?,
            "uninstall must target the same directory `ahma update` installs into"
        );
        Ok(())
    }

    /// `ahma uninstall` and `ahma update` must agree on the install directory —
    /// the two halves of one lifecycle share a single resolution.
    #[test]
    fn uninstall_and_update_resolve_the_same_install_dir() -> Result<()> {
        assert_eq!(
            resolve_install_dir()?,
            crate::update::resolve_install_dir(None)?
        );
        Ok(())
    }

    #[test]
    fn uninstall_binary_dry_run_keeps_binary() -> Result<()> {
        let tmp = tempdir()?;
        let bin = tmp.path().join(crate::update::AHMA_BINARY_NAME);
        std::fs::write(&bin, "binary").ok();
        uninstall_binary_in(tmp.path(), true)?;
        assert!(bin.exists(), "dry-run keeps binary");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn uninstall_binary_removes_binary_and_old() -> Result<()> {
        let tmp = tempdir()?;
        let bin = tmp.path().join("ahma");
        let old = tmp.path().join("ahma.old");
        std::fs::write(&bin, "binary").ok();
        std::fs::write(&old, "old").ok();
        uninstall_binary_in(tmp.path(), false)?;
        assert!(!bin.exists(), "binary removed");
        assert!(!old.exists(), "ahma.old removed");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn uninstall_binary_missing_binary_is_ok() -> Result<()> {
        let tmp = tempdir()?;
        uninstall_binary_in(tmp.path(), false)?;
        Ok(())
    }

    /// The hint replaces what `AHMA_INSTALL_DIR` was actually useful for: telling a user
    /// who installed elsewhere which file to delete. It reports the path ahma can prove
    /// (its own executable) instead of trusting one the environment supplied.
    #[test]
    fn custom_install_hint_names_the_running_exe_when_outside_install_dir() -> Result<()> {
        let install = tempdir()?;
        let elsewhere = tempdir()?;
        let exe = elsewhere.path().join("ahma");
        let hint = custom_install_hint(install.path(), Some(&exe))
            .expect("an exe outside the install dir must produce a hint");
        assert!(
            hint.contains(&exe.display().to_string()),
            "hint must name the running executable: {hint}"
        );
        Ok(())
    }

    #[test]
    fn custom_install_hint_is_silent_for_the_normal_case() -> Result<()> {
        let install = tempdir()?;
        let exe = install.path().join("ahma");
        assert!(
            custom_install_hint(install.path(), Some(&exe)).is_none(),
            "no hint when the running exe is already in the install dir"
        );
        assert!(
            custom_install_hint(install.path(), None).is_none(),
            "no hint when the current exe cannot be determined"
        );
        Ok(())
    }

    // ── execute_actions dispatcher (dry-run, home-injected) ───────────────────

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn execute_actions_dry_run_dispatches_all() -> Result<()> {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = tempdir()?;
        let prev = set_home(tmp.path());
        // Seed a cursor MCP config so the MCP branch finds something.
        write_json(
            &tmp.path().join(".cursor").join("mcp.json"),
            r#"{"mcpServers":{"Ahma":{"type":"stdio"}}}"#,
        );
        // dry_run = true, so the Binary action only *prints* what it would remove;
        // it never touches the real install directory. No env steering needed.
        let result = execute_actions(
            &[
                UninstallAction::Mcp,
                UninstallAction::Skills,
                UninstallAction::Binary,
            ],
            &[Platform::Cursor],
            true, // dry_run
            true, // purge
            false,
        )
        .await;

        // Cursor config must be untouched in dry-run.
        let unchanged = std::fs::read_to_string(tmp.path().join(".cursor").join("mcp.json"))?;
        restore_home(prev);
        result?;
        assert!(unchanged.contains("Ahma"), "dry-run leaves config intact");
        Ok(())
    }

    // ── select_actions / select_platforms additional coverage ─────────────────

    #[test]
    fn select_actions_all_flags() {
        let args = UninstallArgs {
            auto: false,
            mcp: true,
            hooks: true,
            skills: true,
            binary: true,
            platforms: Vec::new(),
            purge: false,
            dry_run: false,
        };
        let actions = select_actions(&args, false);
        assert_eq!(actions.len(), 4);
        assert!(actions.contains(&UninstallAction::Skills));
        assert!(actions.contains(&UninstallAction::Binary));
    }

    #[test]
    fn select_platforms_no_filter_non_interactive_returns_relevant() {
        // Hooks action only → relevant platforms are those supporting hooks.
        let actions = vec![UninstallAction::Hooks];
        let platforms = select_platforms(&actions, &[], false);
        assert!(!platforms.is_empty());
        assert!(platforms.iter().all(|p| p.supports_hooks()));
        // VS Code does not support hooks → excluded.
        assert!(!platforms.iter().any(|p| p.cli_name() == "vscode"));
    }

    #[test]
    fn select_platforms_filter_by_label_case_insensitive() {
        let actions = vec![UninstallAction::Mcp];
        let platforms = select_platforms(&actions, &["claude code".to_string()], false);
        assert_eq!(platforms.len(), 1);
        assert_eq!(platforms[0].cli_name(), "claude");
    }

    #[test]
    fn select_platforms_filter_no_match_empty() {
        let actions = vec![UninstallAction::Mcp];
        let platforms = select_platforms(&actions, &["does-not-exist".to_string()], false);
        assert!(platforms.is_empty());
    }

    #[test]
    fn select_platforms_mcp_excludes_copilot() {
        let actions = vec![UninstallAction::Mcp];
        let platforms = select_platforms(&actions, &[], false);
        assert!(!platforms.iter().any(|p| p.cli_name() == "copilot"));
    }

    // ── enum metadata coverage ────────────────────────────────────────────────

    #[test]
    fn action_labels_and_platform_specificity() {
        for &a in UNINSTALL_ACTIONS {
            assert!(!a.label().is_empty());
        }
        assert!(UninstallAction::Mcp.is_platform_specific());
        assert!(UninstallAction::Hooks.is_platform_specific());
        assert!(!UninstallAction::Skills.is_platform_specific());
        assert!(!UninstallAction::Binary.is_platform_specific());
    }

    #[test]
    fn platform_metadata_is_consistent() {
        for &p in PLATFORMS {
            assert!(!p.label().is_empty());
            assert!(!p.cli_name().is_empty());
            // Every hook-supporting platform must map to a HookPlatform, and
            // hook_platform() is None for non-hook platforms in this set.
            if p.hook_platform().is_some() {
                assert!(p.supports_hooks());
            }
        }
        assert!(!Platform::Copilot.supports_mcp());
        assert!(Platform::Cursor.supports_mcp());
        assert!(!Platform::VsCode.supports_hooks());
        assert!(!Platform::ClaudeDesktop.supports_hooks());
        assert!(!Platform::LmStudio.supports_hooks());
        assert!(Platform::ClaudeCode.supports_hooks());
    }

    // ── parse helpers additional coverage ─────────────────────────────────────

    #[test]
    fn parse_digit_sequence_dedups_and_bounds() {
        // "1123" with max 4 → indices [0,1,2], duplicates ignored, 3 in range.
        let result = parse_selection_string("1123", 4);
        assert_eq!(result, vec![0, 1, 2]);
    }

    #[test]
    fn parse_separated_list_ignores_out_of_range_and_dups() {
        // max 3 → "1 2 5 2" → [0,1] (5 out of range, second 2 deduped).
        let result = parse_selection_string("1 2 5 2", 3);
        assert_eq!(result, vec![0, 1]);
    }

    #[test]
    fn parse_selection_string_large_max_uses_separated() {
        // max_val >= 10 forces the separated-list path even for pure digits.
        let result = parse_selection_string("12", 20);
        assert_eq!(result, vec![11]);
    }

    #[test]
    fn parse_selection_string_empty_is_empty() {
        assert!(parse_selection_string("", 4).is_empty());
        assert!(parse_selection_string("   ", 4).is_empty());
    }

    #[test]
    fn parse_selection_string_dotted_and_semicolon_separators() {
        let result = parse_selection_string("1.3;2", 4);
        assert_eq!(result, vec![0, 2, 1]);
    }

    // ── prompt_multi_select_all non-interactive ───────────────────────────────

    #[test]
    fn prompt_multi_select_all_non_interactive_selects_all() {
        let labels = ["a", "b", "c"];
        let result = prompt_multi_select_all(false, "q", &labels);
        assert_eq!(result, vec![0, 1, 2]);
    }

    // ── print_restart_hints (smoke, no panic) ─────────────────────────────────

    #[test]
    fn print_restart_hints_variants_do_not_panic() {
        print_restart_hints(true, &["Cursor", "Claude Code"], false);
        print_restart_hints(false, &[], false);
        print_restart_hints(true, &[], false);
        print_restart_hints(true, &["Cursor"], true); // dry_run branch
    }
}
