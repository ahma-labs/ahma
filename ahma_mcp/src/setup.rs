//! Native Rust setup wizard for ahma.
//!
//! Configures global MCP servers, terminal hooks, agent skills, and TLS.

use anyhow::{Context, Result, anyhow};
use serde_json::json;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::harness_target::{McpConfigFormat, PLATFORMS, Platform};
use crate::hooks::{HookScope, HooksInstallArgs};
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

/// Apply MCP server configuration for one platform. Returns the display name on
/// success, or `None` if the platform has no MCP config target.
///
/// Where the config lives and how it is encoded are facts about the harness and
/// live on [`Platform`]; this function only decides *which entry* to write, which
/// is genuinely setup's business.
fn configure_mcp(
    platform: Platform,
    transport: &str,
    servers_entry: &serde_json::Value,
    scoped_servers_entry: &serde_json::Value,
    home: &Path,
) -> Result<Option<&'static str>> {
    let Some((path, format)) = platform.mcp_config(home) else {
        return Ok(None);
    };

    match format {
        McpConfigFormat::Toml => {
            merge_codex_toml(&path, build_codex_toml_value(transport))?;
        }
        McpConfigFormat::Json(servers_key) => {
            let entry = select_mcp_json_entry(
                platform,
                transport,
                servers_entry,
                scoped_servers_entry,
                home,
            );
            merge_mcp_json(&path, servers_key, entry)?;
        }
    }

    Ok(Some(platform.mcp_display_name()))
}

/// Choose which JSON MCP entry shape a platform gets: Claude Desktop's
/// type-less entry, the plain entry for platforms that answer `roots/list`,
/// or the scope-carrying entry for platforms that don't (see
/// `build_scoped_servers_entry` for why the scope can't be injected here).
fn select_mcp_json_entry(
    platform: Platform,
    transport: &str,
    servers_entry: &serde_json::Value,
    scoped_servers_entry: &serde_json::Value,
    home: &Path,
) -> serde_json::Value {
    if platform == Platform::ClaudeDesktop {
        // Claude Desktop's entry omits the `"type"` wrapper field.
        build_claude_desktop_mcp_entry(transport, home)
    } else if platform.sends_roots_list() {
        servers_entry.clone()
    } else {
        // No roots/list means ahma cannot discover the workspace, so the
        // entry has to carry the scope explicitly.
        scoped_servers_entry.clone()
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
    let interactive = is_interactive_session(&args);

    if interactive {
        print_wizard_banner();
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
    let platforms = select_platforms_for(&actions, interactive);
    let transport = select_transport_for(&actions, interactive);

    execute_actions(&actions, &platforms, transport, interactive).await?;

    if interactive {
        print_wizard_footer();
    }

    Ok(())
}

/// Question 2, skipped entirely when nothing selected is per-platform: TLS and
/// skills are global, so asking "on which platforms?" for them is noise.
fn select_platforms_for(actions: &[SetupAction], interactive: bool) -> Vec<Platform> {
    if !actions.iter().any(|a| a.is_platform_specific()) {
        return Vec::new();
    }
    select_platforms(actions, interactive)
}

/// The connection transport is a required detail of the MCP action alone, and is
/// only ever asked for — every non-interactive path takes the stdio default.
fn select_transport_for(actions: &[SetupAction], interactive: bool) -> &'static str {
    if interactive && actions.contains(&SetupAction::Mcp) {
        prompt_transport()
    } else {
        "stdio"
    }
}

/// Whether the wizard should ask questions interactively: not `--auto`, and
/// both stdin/stdout are attached to a real terminal (not piped/redirected).
fn is_interactive_session(args: &SetupArgs) -> bool {
    !args.auto && io::stdin().is_terminal() && io::stdout().is_terminal()
}

fn print_wizard_banner() {
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  Ahma Setup Wizard");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();
}

fn print_wizard_footer() {
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  Setup Completed!");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();
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
        "args": ["serve", "stdio", "--tools", "simplify", "--log-monitor"]
    })
}

/// The stdio entry for clients whose config file omits the `"type"` field
/// (Antigravity, LM Studio).
///
/// It used to also pre-create `~/sandbox` and pin these clients to it with
/// `--sandbox-scope`, on the belief that they never send `roots/list`. Both
/// halves were wrong. Antigravity *does* answer `roots/list` — with an empty
/// array (R5.2.7) — and, more importantly, SPEC R5.2.3/R5.4.2 forbid `ahma
/// setup` injecting a scope the user did not choose: this file is client-owned,
/// so a scope written here is exactly the over-broad path the container-root
/// rules exist to distrust. The observed cost of the old behaviour was a session
/// silently locked to `~/sandbox`, where every command failed with an
/// ordinary-looking shell error.
///
/// A roots-empty client now reaches its scope through elicitation (R5.3.1) or
/// the user's own `[sandbox] container_root` (R5.2.3).
fn build_scoped_servers_entry(transport: &str, _home: &Path) -> serde_json::Value {
    if let Some(url) = mcp_shared_transport_url(transport) {
        return json!({ "url": url });
    }
    json!({
        "command": "ahma",
        "args": ["serve", "stdio", "--tools", "simplify", "--log-monitor"]
    })
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
        "args": ["serve", "stdio", "--tools", "simplify", "--log-monitor"]
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

async fn setup_mcp_config(
    platforms: &[Platform],
    transport: &str,
    interactive: bool,
) -> Result<()> {
    let home = ahma_common::config::ahma_home_dir()
        .ok_or_else(|| anyhow!("Could not resolve home directory"))?;

    let servers_entry = build_mcp_servers_entry(transport);
    let scoped_servers_entry = build_scoped_servers_entry(transport, &home);

    let mut configured = Vec::new();

    for platform in platforms.iter().copied().filter(|p| p.supports_mcp()) {
        if let Some(name) = configure_mcp(
            platform,
            transport,
            &servers_entry,
            &scoped_servers_entry,
            &home,
        )? {
            configured.push(name);
        }
    }

    print_mcp_restart_hints(interactive, &configured, transport);

    Ok(())
}

#[derive(Debug, Clone)]
pub struct McpConfigDrift {
    pub platform_name: &'static str,
    pub config_path: PathBuf,
    pub is_toml: bool,
    pub servers_key: &'static str,
    pub recommended_json: Option<serde_json::Value>,
    pub recommended_toml: Option<toml::Value>,
}

impl McpConfigDrift {
    pub fn apply_update(&self) -> Result<()> {
        if self.is_toml {
            if let Some(ref val) = self.recommended_toml {
                merge_codex_toml(&self.config_path, val.clone())?;
            }
        } else if let Some(ref val) = self.recommended_json {
            merge_mcp_json(&self.config_path, self.servers_key, val.clone())?;
        }
        Ok(())
    }
}

pub fn detect_mcp_config_drifts() -> Vec<McpConfigDrift> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };

    let servers_entry = build_mcp_servers_entry("stdio");
    let scoped_servers_entry = build_scoped_servers_entry("stdio", &home);

    PLATFORMS
        .iter()
        .copied()
        .filter(|p| p.supports_mcp())
        .filter_map(|platform| {
            let (path, format) = platform.mcp_config(&home)?;
            if !path.exists() {
                return None;
            }
            match format {
                McpConfigFormat::Toml => {
                    detect_toml_drift(platform, path, build_codex_toml_value("stdio"))
                }
                McpConfigFormat::Json(servers_key) => {
                    let recommended = select_mcp_json_entry(
                        platform,
                        "stdio",
                        &servers_entry,
                        &scoped_servers_entry,
                        &home,
                    );
                    detect_json_drift(platform, path, servers_key, recommended)
                }
            }
        })
        .collect()
}

/// Drift for one Codex-style TOML config, or `None` when there is nothing to
/// update: the file is unreadable or unparseable, carries no Ahma entry at all
/// (setup never wrote one, so drift correction has nothing to correct), or the
/// entry already matches `recommended`.
fn detect_toml_drift(
    platform: Platform,
    path: PathBuf,
    recommended: toml::Value,
) -> Option<McpConfigDrift> {
    let content = std::fs::read_to_string(&path).ok()?;
    let parsed = toml::from_str::<toml::Value>(&content).ok()?;
    let existing = parsed.get("mcp_servers")?.get("Ahma")?;
    if existing == &recommended {
        return None;
    }
    Some(McpConfigDrift {
        platform_name: platform.label(),
        config_path: path,
        is_toml: true,
        servers_key: "mcp_servers",
        recommended_json: None,
        recommended_toml: Some(recommended),
    })
}

/// Drift for one JSON MCP config. Same "nothing to update" cases as
/// [`detect_toml_drift`].
fn detect_json_drift(
    platform: Platform,
    path: PathBuf,
    servers_key: &'static str,
    recommended: serde_json::Value,
) -> Option<McpConfigDrift> {
    let content = std::fs::read_to_string(&path).ok()?;
    let parsed = serde_json::from_str::<serde_json::Value>(&content).ok()?;
    let existing = parsed.get(servers_key)?.get("Ahma")?;
    if existing == &recommended {
        return None;
    }
    Some(McpConfigDrift {
        platform_name: platform.label(),
        config_path: path,
        is_toml: false,
        servers_key,
        recommended_json: Some(recommended),
        recommended_toml: None,
    })
}

/// Copy `path` to `<path>.bak` before setup rewrites it in place.
///
/// Only a *changed* Ahma entry in an *existing* file earns a backup, so
/// re-running setup with matching config leaves no new `.bak` behind
/// (idempotence). A failed copy is a warning, not an error: losing the backup
/// must not stop setup from writing a working config.
fn backup_config_before_change(path: &Path, has_changed: bool) {
    if !has_changed || !path.exists() {
        return;
    }
    let backup_path = PathBuf::from(format!("{}.bak", path.display()));
    match std::fs::copy(path, &backup_path) {
        Ok(_) => tracing::info!(
            "Created backup of {} at {}",
            path.display(),
            backup_path.display()
        ),
        Err(e) => tracing::warn!(
            "Could not create backup of {} at {}: {}",
            path.display(),
            backup_path.display(),
            e
        ),
    }
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create directory {}", parent.display()))
}

/// Read `path` as a JSON object. An absent file, unparseable content, or a
/// non-object root all yield an empty object: a client config ahma cannot
/// understand is replaced rather than allowed to block setup. An I/O error on a
/// file that *does* exist is propagated, because silently discarding a config we
/// were merely unable to read would destroy user data.
fn load_json_object(path: &Path) -> Result<serde_json::Map<String, serde_json::Value>> {
    if !path.exists() {
        return Ok(serde_json::Map::new());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    Ok(match serde_json::from_str::<serde_json::Value>(&content) {
        Ok(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    })
}

/// The `servers_key` sub-object of `root`, created (or replaced, if the key
/// holds something that is not an object) so the caller can insert into it.
fn json_servers_map_mut<'a>(
    root: &'a mut serde_json::Map<String, serde_json::Value>,
    servers_key: &str,
) -> &'a mut serde_json::Map<String, serde_json::Value> {
    let slot = root
        .entry(servers_key.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !slot.is_object() {
        *slot = serde_json::Value::Object(serde_json::Map::new());
    }
    slot.as_object_mut()
        .expect("slot was just normalised to an object")
}

pub(crate) fn merge_mcp_json(
    path: &Path,
    servers_key: &str,
    value: serde_json::Value,
) -> Result<()> {
    let mut config = load_json_object(path)?;

    // Only the "Ahma" key is ours; every other key in the file is the user's and
    // is carried through untouched.
    let servers = json_servers_map_mut(&mut config, servers_key);
    let has_changed = servers.get("Ahma") != Some(&value);
    backup_config_before_change(path, has_changed);
    servers.insert("Ahma".to_string(), value);

    ensure_parent_dir(path)?;
    let mut file = std::fs::File::create(path)
        .with_context(|| format!("Failed to write {}", path.display()))?;
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
                toml::Value::String("--log-monitor".to_string()),
            ];
            table.insert("args".to_string(), toml::Value::Array(args));
        }
    }
    toml::Value::Table(table)
}

/// TOML counterpart of [`load_json_object`], with the same fallback rules.
fn load_toml_table(path: &Path) -> Result<toml::map::Map<String, toml::Value>> {
    if !path.exists() {
        return Ok(toml::map::Map::new());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    Ok(match toml::from_str::<toml::Value>(&content) {
        Ok(toml::Value::Table(table)) => table,
        _ => toml::map::Map::new(),
    })
}

/// TOML counterpart of [`json_servers_map_mut`]. Codex's key is fixed, so unlike
/// the JSON side there is nothing to parameterise.
fn toml_mcp_servers_map_mut(
    root: &mut toml::map::Map<String, toml::Value>,
) -> &mut toml::map::Map<String, toml::Value> {
    let slot = root
        .entry("mcp_servers".to_string())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    if !slot.is_table() {
        *slot = toml::Value::Table(toml::map::Map::new());
    }
    slot.as_table_mut()
        .expect("slot was just normalised to a table")
}

fn merge_codex_toml(path: &Path, value: toml::Value) -> Result<()> {
    let mut config = load_toml_table(path)?;

    // Only `mcp_servers.Ahma` is ours; the rest of Codex's config is untouched.
    let mcp_servers = toml_mcp_servers_map_mut(&mut config);
    let has_changed = mcp_servers.get("Ahma") != Some(&value);
    backup_config_before_change(path, has_changed);
    mcp_servers.insert("Ahma".to_string(), value);

    ensure_parent_dir(path)?;
    let content = toml::to_string_pretty(&toml::Value::Table(config))?;
    std::fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))
}

async fn setup_terminal_hooks(platforms: &[Platform], interactive: bool) -> Result<()> {
    let (hook_platforms, names): (Vec<_>, Vec<_>) = platforms
        .iter()
        .copied()
        .filter(|p| p.supports_hooks())
        .filter_map(|p| p.hook_platform().map(|hook| (hook, p.label())))
        .unzip();

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
        tokio::fs::create_dir_all(&skill_dir)
            .await
            .with_context(|| format!("Failed to create directory {}", skill_dir.display()))?;
        tokio::fs::write(&skill_path, SKILL_CONTENT)
            .await
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
    let current_content = tokio::fs::read_to_string(path).await?;
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
    if tokio::fs::try_exists(&backup_path).await.unwrap_or(false) {
        let _ = tokio::fs::remove_file(&backup_path).await;
    }
    tokio::fs::rename(path, &backup_path).await?;
    tokio::fs::write(path, new_template).await?;
    println!(
        "✓ Updated ~/.ahma/prompts.toml. Old version backed up to {}",
        backup_path.display()
    );
    println!();
    Ok(())
}

async fn setup_llm_prompts(interactive: bool) -> Result<()> {
    use ahma_common::prompts::AhmaPrompts;

    let Some(path) = ahma_common::prompts::global_prompts_path() else {
        return Ok(());
    };

    let new_template = AhmaPrompts::generate_template();

    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, &new_template).await?;
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
    fn test_antigravity_servers_entry_injects_no_scope_and_creates_nothing() {
        // REGRESSION (SPEC R5.2.3 / R5.4.2): this entry used to pre-create
        // `~/sandbox` and pin Antigravity to it with `--sandbox-scope`. That
        // silently locked the session to a directory the user never chose, and
        // every command run there failed with an ordinary-looking shell error.
        let tmp = tempdir().unwrap();
        let fake_home = tmp.path();
        let entry = build_scoped_servers_entry("stdio", fake_home);

        let args = entry["args"].as_array().expect("args must be an array");
        assert!(
            !args.iter().any(|a| a.as_str() == Some("--sandbox-scope")),
            "setup must not inject a scope into a client-owned config: {args:?}"
        );
        assert!(
            !fake_home.join("sandbox").exists(),
            "setup must not pre-create ~/sandbox"
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

    // ─── Generated configs never carry --tmp or an injected scope ────────────
    //
    // SPEC R5.4.2: these files are *client-owned* — anyone configuring the client
    // can edit them — so `ahma setup` must not write a sandbox scope into one.
    // It used to write `--sandbox-scope ~/sandbox` for Antigravity and LM Studio,
    // which pinned those sessions to a directory the user never chose.

    /// Assert a generated stdio entry carries neither the temp-dir downgrade nor
    /// a scope ahma chose on the user's behalf.
    fn assert_no_tmp_and_no_injected_scope(entry: &serde_json::Value, who: &str) {
        let args = entry["args"].as_array().expect("args must be array");
        assert!(
            !args.iter().any(|a| a.as_str() == Some("--tmp")),
            "{who} entry must NOT include --tmp (R5.4.2): {args:?}"
        );
        assert!(
            !args.iter().any(|a| a.as_str() == Some("--sandbox-scope")),
            "{who} entry must NOT inject a sandbox scope (R5.2.3/R5.4.2): {args:?}"
        );
    }

    #[test]
    fn test_default_mcp_entry_has_no_tmp_or_injected_scope() {
        assert_no_tmp_and_no_injected_scope(&build_mcp_servers_entry("stdio"), "default stdio");
    }

    #[test]
    fn test_claude_desktop_entry_has_no_tmp_or_injected_scope() {
        let tmp = tempdir().unwrap();
        let entry = build_claude_desktop_mcp_entry("stdio", tmp.path());
        assert_no_tmp_and_no_injected_scope(&entry, "Claude Desktop");
    }

    #[test]
    fn test_antigravity_entry_has_no_tmp_or_injected_scope() {
        let tmp = tempdir().unwrap();
        let entry = build_scoped_servers_entry("stdio", tmp.path());
        assert_no_tmp_and_no_injected_scope(&entry, "Antigravity");
    }

    /// `ahma setup` must not create `~/sandbox` (or any other directory) as a
    /// side effect of building the Antigravity/LM Studio entry.
    #[test]
    fn test_scoped_entry_creates_no_directory() {
        let tmp = tempdir().unwrap();
        let _ = build_scoped_servers_entry("stdio", tmp.path());
        let created: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert!(
            created.is_empty(),
            "setup must not pre-create any directory under home: {created:?}"
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
        assert!(args.iter().any(|a| a.as_str() == Some("--log-monitor")));
        assert!(
            !args.iter().any(|a| a.as_str() == Some("--sandbox-scope")),
            "setup must not inject a scope into a client-owned config: {args:?}"
        );
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

    // ─── platform-specific config locations ───────────────────────────────────

    #[test]
    fn test_claude_desktop_config_path_points_at_claude() {
        let home = Path::new("/home/tester");
        let (p, _) = Platform::ClaudeDesktop
            .mcp_config(home)
            .expect("Claude Desktop has an MCP config target");
        let s = p.to_string_lossy();
        assert!(s.contains("Claude"), "path should mention Claude: {s}");
        assert!(s.ends_with("claude_desktop_config.json"));
    }

    #[test]
    fn test_vscode_mcp_path_points_at_code_mcp_json() {
        let home = Path::new("/home/tester");
        let (p, format) = Platform::VsCode
            .mcp_config(home)
            .expect("VS Code has an MCP config target");
        let s = p.to_string_lossy();
        assert!(s.contains("Code"), "path should mention Code: {s}");
        assert!(s.ends_with("mcp.json"));
        assert_eq!(format, McpConfigFormat::Json("servers"));
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
        let name = configure_mcp(Platform::ClaudeCode, "stdio", &servers, &scoped, home)?;
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
        let name = configure_mcp(Platform::Cursor, "stdio", &servers, &scoped, home)?;
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
        let name = configure_mcp(Platform::Antigravity, "stdio", &servers, &scoped, home)?;
        assert_eq!(name, Some("Antigravity"));
        let path = home.join(".gemini").join("config").join("mcp_config.json");
        let parsed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
        // The scoped entry differs from the default one only by omitting the
        // "type" field — it carries no scope of its own (R5.2.3 / R5.4.2).
        let ahma = &parsed["mcpServers"]["Ahma"];
        assert!(
            ahma.get("type").is_none(),
            "Antigravity entry must omit the type field: {ahma}"
        );
        let args = ahma["args"].as_array().unwrap();
        assert!(!args.iter().any(|a| a == "--sandbox-scope"));
        Ok(())
    }

    #[test]
    fn test_configure_mcp_lmstudio_uses_scoped_entry() -> Result<()> {
        let tmp = tempdir()?;
        let home = tmp.path();
        let servers = build_mcp_servers_entry("stdio");
        let scoped = build_scoped_servers_entry("stdio", home);
        let name = configure_mcp(Platform::LmStudio, "stdio", &servers, &scoped, home)?;
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
        let name = configure_mcp(Platform::Codex, "http", &servers, &scoped, home)?;
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
        let name = configure_mcp(Platform::Copilot, "stdio", &servers, &scoped, home)?;
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

    use parking_lot::Mutex;
    use std::sync::LazyLock;

    static SETUP_ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Run `f` with `AHMA_TEST_HOME` pointed at `home`, restoring the prior value
    /// afterwards. Serialized so concurrent tests don't clobber the env var.
    fn with_test_home<R>(home: &Path, f: impl FnOnce() -> R) -> R {
        let _guard = SETUP_ENV_MUTEX.lock();
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

    #[test]
    fn test_merge_mcp_json_creates_backup_when_updating() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("mcp.json");
        let initial = serde_json::json!({
            "mcpServers": {
                "Ahma": {
                    "command": "ahma",
                    "args": ["serve", "stdio"]
                }
            }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&initial)?)?;

        let updated = serde_json::json!({
            "command": "ahma",
            "args": ["serve", "stdio", "--sandbox", "--sandbox-scope", "/tmp/sandbox"]
        });

        merge_mcp_json(&path, "mcpServers", updated)?;

        let backup_path = PathBuf::from(format!("{}.bak", path.display()));
        assert!(backup_path.exists(), "Backup file must be created");
        let backup_content: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(backup_path)?)?;
        assert_eq!(
            backup_content["mcpServers"]["Ahma"]["args"],
            serde_json::json!(["serve", "stdio"])
        );

        let current_content: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        assert_eq!(
            current_content["mcpServers"]["Ahma"]["args"],
            serde_json::json!([
                "serve",
                "stdio",
                "--sandbox",
                "--sandbox-scope",
                "/tmp/sandbox"
            ])
        );
        Ok(())
    }

    /// Re-running setup over an already-correct file must be a no-op on disk:
    /// no `.bak` is left behind, and the user's other keys survive.
    #[test]
    fn test_merge_mcp_json_unchanged_entry_writes_no_backup() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("mcp.json");
        let entry = build_mcp_servers_entry("stdio");

        merge_mcp_json(&path, "mcpServers", entry.clone())?;
        merge_mcp_json(&path, "mcpServers", entry.clone())?;

        let backup_path = PathBuf::from(format!("{}.bak", path.display()));
        assert!(
            !backup_path.exists(),
            "an unchanged Ahma entry must not produce a backup"
        );
        let parsed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        assert_eq!(parsed["mcpServers"]["Ahma"], entry);
        Ok(())
    }

    #[test]
    fn test_merge_codex_toml_unchanged_entry_writes_no_backup() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("config.toml");
        let value = build_codex_toml_value("stdio");

        merge_codex_toml(&path, value.clone())?;
        merge_codex_toml(&path, value)?;

        let backup_path = PathBuf::from(format!("{}.bak", path.display()));
        assert!(
            !backup_path.exists(),
            "an unchanged Ahma entry must not produce a backup"
        );
        let parsed: toml::Value = toml::from_str(&std::fs::read_to_string(&path)?)?;
        assert_eq!(
            parsed["mcp_servers"]["Ahma"]["command"].as_str(),
            Some("ahma")
        );
        Ok(())
    }

    #[test]
    fn test_mcp_config_drift_apply_update() -> Result<()> {
        let tmp = tempdir()?;
        let path = tmp.path().join("mcp.json");
        let initial = serde_json::json!({
            "mcpServers": {
                "Ahma": {
                    "command": "ahma",
                    "args": ["serve", "stdio"]
                }
            }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&initial)?)?;

        let drift = McpConfigDrift {
            platform_name: "Test Platform",
            config_path: path.clone(),
            is_toml: false,
            servers_key: "mcpServers",
            recommended_json: Some(serde_json::json!({
                "command": "ahma",
                "args": ["serve", "stdio", "--sandbox"]
            })),
            recommended_toml: None,
        };

        drift.apply_update()?;

        let backup_path = PathBuf::from(format!("{}.bak", path.display()));
        assert!(backup_path.exists());
        let updated: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
        assert_eq!(
            updated["mcpServers"]["Ahma"]["args"],
            serde_json::json!(["serve", "stdio", "--sandbox"])
        );
        Ok(())
    }
}
