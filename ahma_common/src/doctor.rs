//! `ahma doctor` / TUI `/doctor`: look at ahma's own state and say plainly
//! what is wrong, why it matters, and what would fix it (SPEC R-DOCTOR).
//!
//! Everything here is **read-only** except [`Fix::apply`], and a fix is only
//! ever applied after a human confirmed that exact fix — the TUI and the CLI
//! both ask first. A model may *explain* a report or *suggest* a fix; it never
//! applies one (the TUI's `/doctor <question>` hands the model this report as
//! context and has no path from its answer to [`Fix::apply`]).
//!
//! The checks live in `ahma_common` so the CLI (`ahma_mcp`) and the TUI
//! (`ahma_tui`) run the same ones and cannot drift apart.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::{AhmaSettings, settings_path};
use crate::permissions::{
    AuditAction, GrantKind, GrantTier, TRUSTED_WORKSPACE_TOOL, append_audit, audit_entry,
    workspace_key,
};

/// How much a finding matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Fine — said so the report is also a reassurance, not only a list of faults.
    Ok,
    /// Worth knowing; nothing is wrong.
    Info,
    /// Something is off and has a cost.
    Warn,
    /// Something is broken.
    Problem,
}

impl Level {
    /// A one-character marker (ASCII, SPEC R22.3).
    pub fn marker(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Info => "i",
            Level::Warn => "!",
            Level::Problem => "x",
        }
    }
}

/// One thing the doctor noticed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub level: Level,
    pub title: String,
    pub detail: String,
    pub fix: Option<Fix>,
}

/// A change the doctor can make — only after the user confirmed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fix {
    /// Remove persistent scope grants whose folder no longer exists.
    RemoveMissingScopes(Vec<PathBuf>),
    /// Forget approvals and trust recorded for folders that no longer exist.
    ForgetMissingWorkspaces(Vec<PathBuf>),
    /// Clean up bloated one-off and malformed grants in ~/.gemini configs, ensuring clean ahma command grants.
    RepairAntigravityPermissions {
        cli_settings: Option<PathBuf>,
        config_file: Option<PathBuf>,
        ide_mcp: Option<PathBuf>,
        clean_grants: Vec<String>,
    },
}

impl Fix {
    /// What confirming this fix would do, in one sentence.
    pub fn describe(&self) -> String {
        match self {
            Fix::RemoveMissingScopes(paths) => format!(
                "Remove {} granted folder(s) that no longer exist from ~/.ahma/settings.toml: {}",
                paths.len(),
                join_paths(paths)
            ),
            Fix::ForgetMissingWorkspaces(paths) => format!(
                "Forget approvals recorded for {} folder(s) that no longer exist: {}",
                paths.len(),
                join_paths(paths)
            ),
            Fix::RepairAntigravityPermissions { .. } => {
                "Clean up bloated one-off and malformed grants in ~/.gemini configs, and repair Antigravity MCP settings".to_string()
            }
        }
    }

    /// Make the change. Writes through [`AhmaSettings::update`] (never over a
    /// file it cannot parse) and records each removal in the audit log,
    /// stamped `now` (RFC 3339, supplied by the caller).
    pub fn apply(&self, now: &str) -> anyhow::Result<String> {
        match self {
            Fix::RemoveMissingScopes(paths) => apply_remove_missing_scopes(paths, now),
            Fix::ForgetMissingWorkspaces(paths) => apply_forget_missing_workspaces(paths, now),
            Fix::RepairAntigravityPermissions {
                cli_settings,
                config_file,
                ide_mcp,
                clean_grants,
            } => apply_repair_antigravity_permissions(
                cli_settings.as_ref(),
                config_file.as_ref(),
                ide_mcp.as_ref(),
                clean_grants,
                now,
            ),
        }
    }
}

fn apply_remove_missing_scopes(paths: &[PathBuf], now: &str) -> anyhow::Result<String> {
    AhmaSettings::update(|s| {
        s.sandbox
            .persistent_scopes
            .retain(|scope| !paths.contains(&scope.path));
    })?;
    for path in paths {
        append_audit(&audit_entry(
            now.to_string(),
            AuditAction::Revoke,
            GrantKind::FsScope,
            path.display().to_string(),
            None,
            GrantTier::Always,
            Some("doctor".to_string()),
        ));
    }
    Ok(format!("Removed {} granted folder(s)", paths.len()))
}

fn apply_forget_missing_workspaces(paths: &[PathBuf], now: &str) -> anyhow::Result<String> {
    AhmaSettings::update(|s| {
        s.permissions
            .tool_approvals
            .retain(|a| !paths.contains(&a.workspace));
    })?;
    for path in paths {
        append_audit(&audit_entry(
            now.to_string(),
            AuditAction::Revoke,
            GrantKind::Tool,
            "*",
            None,
            GrantTier::Always,
            Some(format!("doctor: {}", path.display())),
        ));
    }
    Ok(format!("Forgot approvals for {} folder(s)", paths.len()))
}

fn apply_repair_antigravity_permissions(
    cli_settings: Option<&PathBuf>,
    config_file: Option<&PathBuf>,
    ide_mcp: Option<&PathBuf>,
    clean_grants: &[String],
    now: &str,
) -> anyhow::Result<String> {
    let mut files_cleaned = 0;
    if let Some(path) = cli_settings {
        clean_antigravity_file(path, clean_grants, false)?;
        files_cleaned += 1;
    }
    if let Some(path) = config_file {
        clean_antigravity_file(path, clean_grants, true)?;
        files_cleaned += 1;
    }
    if let Some(path) = ide_mcp {
        clean_antigravity_ide_mcp(path)?;
        files_cleaned += 1;
    }
    append_audit(&audit_entry(
        now.to_string(),
        AuditAction::Grant,
        GrantKind::Tool,
        "antigravity-permissions",
        None,
        GrantTier::Always,
        Some("doctor".to_string()),
    ));
    Ok(format!(
        "Cleaned Antigravity permissions in {files_cleaned} file(s)"
    ))
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Where the doctor looks. Explicit so every check is testable against a
/// temporary home and workspace.
pub struct DoctorInput {
    /// The settings file (`~/.ahma/settings.toml`).
    pub settings_file: Option<PathBuf>,
    /// User home directory (`~`).
    pub home_dir: Option<PathBuf>,
    /// Current ahma binary path.
    pub current_exe: Option<PathBuf>,
    /// The folder the user is working in.
    pub workspace: PathBuf,
    /// What the per-user hub says about itself, if it is running.
    pub hub: HubStatus,
    /// This binary's version and build id, to compare with the hub's.
    pub version: String,
    pub build_id: String,
    /// How git can authenticate from inside the sandbox (SSH agent, HTTPS
    /// credential helpers). `None` skips the check (tests, no git on PATH).
    pub git_auth: Option<GitAuthProbe>,
}

impl DoctorInput {
    /// The real locations for `workspace`.
    pub fn for_workspace(workspace: &Path) -> Self {
        Self {
            settings_file: settings_path(),
            home_dir: crate::config::ahma_home_dir(),
            current_exe: std::env::current_exe()
                .ok()
                .map(|p| dunce::canonicalize(&p).unwrap_or(p)),
            workspace: workspace.to_path_buf(),
            hub: probe_hub_blocking(&crate::hub::default_socket_path()),
            version: env!("CARGO_PKG_VERSION").to_string(),
            build_id: crate::BUILD_ID.to_string(),
            git_auth: Some(GitAuthProbe::collect(
                crate::config::ahma_home_dir().as_deref(),
            )),
        }
    }
}

/// Run every check. Findings are ordered most serious first.
pub fn run(input: &DoctorInput) -> Vec<Finding> {
    let mut findings = Vec::new();
    let settings = check_settings(input, &mut findings);
    if let Some(settings) = &settings {
        check_missing_scopes(settings, &mut findings);
        check_global_scopes(settings, &mut findings);
        check_missing_workspaces(settings, &mut findings);
        check_trust(settings, &input.workspace, &mut findings);
    }
    check_hub(input, &mut findings);
    check_antigravity_permissions(
        input.home_dir.as_deref(),
        input.current_exe.as_deref(),
        &mut findings,
    );
    check_grant_tools_auto_allowed(input.home_dir.as_deref(), &input.workspace, &mut findings);
    check_hook_coverage(input.home_dir.as_deref(), &mut findings);
    if let Some(probe) = &input.git_auth {
        let default_settings;
        let settings = match &settings {
            Some(s) => s,
            None => {
                default_settings = AhmaSettings::default();
                &default_settings
            }
        };
        check_git_auth(probe, settings, &mut findings);
    }
    check_logs(&input.workspace, &mut findings);
    findings.sort_by_key(|f| std::cmp::Reverse(f.level));
    findings
}

/// The MCP tools whose only job is to *request* a wider sandbox or network
/// scope. A harness that auto-allows them removes the one human step ahma
/// cannot supply itself when the harness also cannot show an elicitation
/// prompt (SPEC R5.4.5). This is how an Antigravity agent granted itself a
/// persistent read-write scope.
const GRANT_TOOLS: [&str; 2] = ["sandbox_grant", "network_grant"];

/// Every `permissions.allow`-style list in a harness settings file that names
/// one of the grant tools, as `(file, entry)` pairs.
fn grant_tool_allow_entries(path: &Path) -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    let mut hits = Vec::new();
    fn walk(v: &serde_json::Value, under_allow: bool, hits: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, child) in map {
                    walk(child, under_allow || k == "allow", hits);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    walk(item, under_allow, hits);
                }
            }
            serde_json::Value::String(s)
                if under_allow
                    && GRANT_TOOLS.iter().any(|t| s.contains(t))
                    && !hits.contains(s) =>
            {
                hits.push(s.clone());
            }
            _ => {}
        }
    }
    walk(&doc, false, &mut hits);
    hits
}

/// Warn when a harness is configured to approve `sandbox_grant`/`network_grant`
/// without asking a human.
fn check_grant_tools_auto_allowed(home: Option<&Path>, workspace: &Path, out: &mut Vec<Finding>) {
    let Some(home) = home else { return };
    let candidates = [
        home.join(".gemini")
            .join("antigravity-cli")
            .join("settings.json"),
        home.join(".gemini").join("config").join("config.json"),
        home.join(".claude").join("settings.json"),
        workspace.join(".claude").join("settings.json"),
        workspace.join(".claude").join("settings.local.json"),
        home.join(".cursor").join("mcp.json"),
        home.join(".codex").join("config.toml"),
    ];
    let mut detail = Vec::new();
    for path in candidates.iter().filter(|p| p.exists()) {
        let entries = if path.extension().is_some_and(|e| e == "toml") {
            // Codex keeps its allow list in TOML; a plain line scan is enough.
            std::fs::read_to_string(path)
                .map(|raw| {
                    raw.lines()
                        .filter(|l| GRANT_TOOLS.iter().any(|t| l.contains(t)))
                        .map(|l| l.trim().to_string())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        } else {
            grant_tool_allow_entries(path)
        };
        for e in entries {
            detail.push(format!("{}: {e}", path.display()));
        }
    }
    if detail.is_empty() {
        return;
    }
    out.push(Finding {
        level: Level::Warn,
        title: "A harness auto-approves ahma's grant tools".into(),
        detail: format!(
            "`sandbox_grant` / `network_grant` only *request* a wider scope; the human decides. \
             These allow-list entries let the model call them without a prompt, and a harness \
             that cannot show ahma's own approval prompt then has no human in the loop at all \
             (an agent granted itself a read-write directory this way). Remove them: {}",
            detail.join("; ")
        ),
        fix: None,
    });
}

/// Whether a harness's hook file carries ahma's shell hook and its edit guard.
fn hook_file_coverage(path: &Path) -> Option<(bool, bool)> {
    let raw = std::fs::read_to_string(path).ok()?;
    let shell = raw.contains("hooks exec") || raw.contains("'hooks' 'exec'");
    let guard = raw.contains("ahma-edit-guard-v1");
    (shell || guard).then_some((shell, guard))
}

/// Report, per harness with an ahma shell hook, whether the client's native
/// file edits are confined too (SPEC R5.5.6), and what Claude Code's own
/// sandbox setting is — informational, since ahma enforces regardless (R7.2).
fn check_hook_coverage(home: Option<&Path>, out: &mut Vec<Finding>) {
    let Some(home) = home else { return };
    let files = [
        ("Claude Code", home.join(".claude").join("settings.json")),
        ("Cursor", home.join(".cursor").join("hooks.json")),
        ("Codex", home.join(".codex").join("hooks.json")),
        (
            "GitHub Copilot CLI",
            home.join(".copilot").join("hooks").join("ahma.json"),
        ),
        (
            "Antigravity",
            home.join(".gemini").join("config").join("hooks.json"),
        ),
    ];
    let mut unguarded = Vec::new();
    let mut covered = Vec::new();
    for (name, path) in &files {
        match hook_file_coverage(path) {
            Some((true, false)) => unguarded.push(*name),
            Some((true, true)) => covered.push(*name),
            _ => {}
        }
    }
    let claude_sandbox = std::fs::read_to_string(home.join(".claude").join("settings.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("sandbox")?.get("enabled")?.as_bool())
        .unwrap_or(false);
    if !unguarded.is_empty() {
        out.push(Finding {
            level: Level::Warn,
            title: "Native file edits are not confined in some clients".into(),
            detail: format!(
                "{} route shell commands through ahma's sandbox but have no edit guard, so the \
                 client's own Edit/Write tools can reach any path on this machine. Run \
                 `ahma hooks install` to add it (SPEC R5.5.6).",
                unguarded.join(", ")
            ),
            fix: None,
        });
    }
    if !covered.is_empty() || !unguarded.is_empty() {
        out.push(Finding {
            level: Level::Info,
            title: "Terminal hooks apply ahma's own sandbox".into(),
            detail: format!(
                "Shell hook + edit guard: {}. ahma never defers to a client on an environment \
                 marker; it defers only when the kernel refuses to nest its sandbox (SPEC R7.2). \
                 Claude Code's own Bash sandbox is {} — ahma's hook is the sandbox either way.",
                if covered.is_empty() {
                    "none".to_string()
                } else {
                    covered.join(", ")
                },
                if claude_sandbox { "on" } else { "off" }
            ),
            fix: None,
        });
    }
}

fn check_settings(input: &DoctorInput, out: &mut Vec<Finding>) -> Option<AhmaSettings> {
    let Some(file) = &input.settings_file else {
        out.push(Finding {
            level: Level::Warn,
            title: "No home directory".into(),
            detail: "ahma cannot find ~/.ahma, so nothing it is told to remember is kept.".into(),
            fix: None,
        });
        return None;
    };
    match AhmaSettings::load_from_result(file) {
        Ok(settings) => {
            out.push(Finding {
                level: Level::Ok,
                title: "Settings file reads cleanly".into(),
                detail: file.display().to_string(),
                fix: None,
            });
            Some(settings)
        }
        Err(e) => {
            out.push(Finding {
                level: Level::Problem,
                title: "Settings file does not parse".into(),
                detail: format!(
                    "{e}. ahma refuses to start servers with it (fail-closed) and will not \
                     overwrite it. Fix the line named, or move the file aside and run \
                     `ahma settings init`."
                ),
                fix: None,
            });
            None
        }
    }
}

fn check_missing_scopes(settings: &AhmaSettings, out: &mut Vec<Finding>) {
    let missing: Vec<PathBuf> = settings
        .sandbox
        .persistent_scopes
        .iter()
        .filter(|s| !s.path.exists())
        .map(|s| s.path.clone())
        .collect();
    if missing.is_empty() {
        return;
    }
    out.push(Finding {
        level: Level::Warn,
        title: "Granted folders that no longer exist".into(),
        detail: format!(
            "{} — every ahma session tries to add these to its sandbox and logs a warning. \
             (Test runs used to write such grants into the real settings file.)",
            join_paths(&missing)
        ),
        fix: Some(Fix::RemoveMissingScopes(missing)),
    });
}

/// A grant with no workspace reaches every project's agent on this machine
/// (SPEC R5.4.11). Say so, and how to narrow it.
fn check_global_scopes(settings: &AhmaSettings, out: &mut Vec<Finding>) {
    let global: Vec<String> = settings
        .sandbox
        .persistent_scopes
        .iter()
        .filter(|s| s.workspace.is_none())
        .map(|s| format!("{} ({})", s.path.display(), s.access.label()))
        .collect();
    if global.is_empty() {
        return;
    }
    out.push(Finding {
        level: Level::Warn,
        title: "Some granted folders apply to every workspace".into(),
        detail: format!(
            "These grants have no workspace, so an agent in any project can use them: {}. \
             Narrow each one: `ahma sandbox revoke <path>`, then `ahma sandbox grant <path>` \
             from inside the project that needs it.",
            global.join(", ")
        ),
        fix: None,
    });
}

fn check_missing_workspaces(settings: &AhmaSettings, out: &mut Vec<Finding>) {
    let missing: Vec<PathBuf> = settings
        .permissions
        .tool_approvals
        .iter()
        .filter(|a| !a.workspace.exists())
        .map(|a| a.workspace.clone())
        .collect();
    if missing.is_empty() {
        return;
    }
    out.push(Finding {
        level: Level::Info,
        title: "Approvals for folders that no longer exist".into(),
        detail: join_paths(&missing),
        fix: Some(Fix::ForgetMissingWorkspaces(missing)),
    });
}

fn check_trust(settings: &AhmaSettings, workspace: &Path, out: &mut Vec<Finding>) {
    let key = workspace_key(workspace);
    let perms = &settings.permissions;
    let tools: Vec<&str> = perms
        .tool_approvals
        .iter()
        .filter(|a| a.workspace == key)
        .flat_map(|a| a.tools.iter().map(String::as_str))
        .filter(|t| *t != TRUSTED_WORKSPACE_TOOL)
        .collect();
    let (title, detail) = if perms.is_workspace_trusted(&key) {
        (
            "This folder is trusted",
            "Tools run inside it without asking; outside it, network and settings still ask. \
             /settings trust to change."
                .to_string(),
        )
    } else {
        (
            "This folder is not trusted",
            format!(
                "Tools that change something ask first{}. /settings trust to trust it.",
                if tools.is_empty() {
                    String::new()
                } else {
                    format!(" (always allowed here: {})", tools.join(", "))
                }
            ),
        )
    };
    out.push(Finding {
        level: Level::Info,
        title: title.into(),
        detail,
        fix: None,
    });
}

/// Whether the per-user hub is running, and which build, as it says
/// itself (SPEC R-HUB.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubStatus {
    /// Nothing answers on its socket.
    NotRunning,
    /// It answers; `version` is its `/health` version (`semver+build_id`), if
    /// it gave a readable one.
    Running { version: Option<String> },
}

/// How long the doctor waits for the hub to answer. It is local, so a
/// hub that takes longer is itself worth reporting as not answering.
const HUB_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Ask the hub at `socket` for its `/health`.
///
/// The socket answering is what says a hub is running: nothing is read
/// from a file that could outlive it, and the lock is left alone, so a doctor
/// run can never make a starting hub lose its own lock and stand down.
pub async fn probe_hub(socket: &Path) -> HubStatus {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let exchange = async {
        let mut stream = crate::local_socket::LocalStream::connect(socket)
            .await
            .ok()?;
        let mut reply = Vec::new();
        let answered = stream
            .write_all(b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .await
            .is_ok()
            && stream.read_to_end(&mut reply).await.is_ok();
        Some(answered.then_some(reply).and_then(|r| health_version(&r)))
    };
    match tokio::time::timeout(HUB_PROBE_TIMEOUT, exchange).await {
        Ok(Some(version)) => HubStatus::Running { version },
        Ok(None) => HubStatus::NotRunning,
        // Connected, then said nothing in time.
        Err(_) => HubStatus::Running { version: None },
    }
}

/// The `version` of a raw HTTP `/health` response, if it is a 2xx with one.
fn health_version(response: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(response).ok()?;
    let (head, body) = text.split_once("\r\n\r\n")?;
    let status = head.lines().next()?.split_whitespace().nth(1)?;
    if !status.starts_with('2') {
        return None;
    }
    let json: serde_json::Value = serde_json::from_str(body).ok()?;
    Some(json.get("version")?.as_str()?.to_string())
}

/// [`probe_hub`] for a synchronous caller, inside a tokio runtime or not:
/// it runs on a thread of its own with a runtime of its own.
pub fn probe_hub_blocking(socket: &Path) -> HubStatus {
    let socket = socket.to_path_buf();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        Some(runtime.block_on(probe_hub(&socket)))
    })
    .join()
    .ok()
    .flatten()
    .unwrap_or(HubStatus::NotRunning)
}

fn check_hub(input: &DoctorInput, out: &mut Vec<Finding>) {
    let ours = format!("{}+{}", input.version, input.build_id);
    let finding = match &input.hub {
        HubStatus::Running {
            version: Some(theirs),
        } if *theirs == ours => Finding {
            level: Level::Ok,
            title: "Hub is running this build".into(),
            detail: format!("ahma {theirs}"),
            fix: None,
        },
        HubStatus::Running {
            version: Some(theirs),
        } => Finding {
            level: Level::Warn,
            title: "Hub is a different build".into(),
            detail: format!(
                "The hub is ahma {theirs}, this is {ours}. Behaviour follows the hub \
                 until it restarts: quit every ahma window and editor session using ahma, and \
                 the next one starts the new build."
            ),
            fix: None,
        },
        HubStatus::Running { version: None } => Finding {
            level: Level::Info,
            title: "Hub is running".into(),
            detail: "It did not say which build it is.".into(),
            fix: None,
        },
        HubStatus::NotRunning => Finding {
            level: Level::Info,
            title: "Hub is not running".into(),
            detail: "It starts by itself when needed.".into(),
            fix: None,
        },
    };
    out.push(finding);
}

fn is_bloated_ahma_grant(s: &str) -> bool {
    s.contains("hooks")
        && s.contains("run-shell")
        && (s.contains("--command") || s.contains("--payload-base64") || s.contains("--cwd"))
}

fn is_malformed_ahma_grant(s: &str) -> bool {
    if s.contains("regex:") {
        return false;
    }
    s.contains("ahma") && (s.contains(".*") || s.contains(r"\.") || s.contains('\n'))
}

fn is_grant_unwanted(s: &str, bloated: &mut usize, malformed: &mut usize) -> bool {
    if is_bloated_ahma_grant(s) {
        *bloated += 1;
        true
    } else if is_malformed_ahma_grant(s) {
        *malformed += 1;
        true
    } else {
        false
    }
}

fn clean_grants_vec(
    grants: &[serde_json::Value],
    clean_grants: &[String],
) -> (Vec<serde_json::Value>, usize, usize) {
    let mut cleaned = Vec::new();
    let mut bloated_count = 0;
    let mut malformed_count = 0;

    for g in grants {
        let Some(s) = g.as_str() else { continue };
        if is_grant_unwanted(s, &mut bloated_count, &mut malformed_count) {
            continue;
        }
        if !cleaned.contains(g) {
            cleaned.push(g.clone());
        }
    }

    for add in clean_grants {
        let val = serde_json::Value::String(add.clone());
        if !cleaned.contains(&val) {
            cleaned.push(val);
        }
    }

    (cleaned, bloated_count, malformed_count)
}

fn clean_antigravity_file(
    path: &Path,
    clean_grants: &[String],
    is_config_file: bool,
) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(path)?;
    let mut doc: serde_json::Value =
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}));
    let Some(root) = doc.as_object_mut() else {
        anyhow::bail!("Root of {} is not an object", path.display());
    };

    if let Some(perms) = root.get_mut("permissions").and_then(|p| p.as_object_mut())
        && let Some(allow) = perms.get("allow").and_then(|a| a.as_array())
    {
        let (cleaned, _, _) = clean_grants_vec(allow, clean_grants);
        perms.insert("allow".to_string(), serde_json::Value::Array(cleaned));
    }

    if is_config_file
        && let Some(user_settings) = root.get_mut("userSettings").and_then(|u| u.as_object_mut())
        && let Some(gpg) = user_settings
            .get_mut("globalPermissionGrants")
            .and_then(|g| g.as_object_mut())
        && let Some(allow) = gpg.get("allow").and_then(|a| a.as_array())
    {
        let (cleaned, _, _) = clean_grants_vec(allow, clean_grants);
        gpg.insert("allow".to_string(), serde_json::Value::Array(cleaned));
    }

    let formatted = serde_json::to_string_pretty(&doc)?;
    std::fs::write(path, formatted)?;
    Ok(())
}

fn remove_tmp_from_args(args: &mut Vec<serde_json::Value>) -> bool {
    let before_len = args.len();
    args.retain(|arg| arg.as_str() != Some("--tmp"));
    let mut modified = args.len() != before_len;

    for arg in args.iter_mut() {
        if let Some(s) = arg.as_str()
            && s.contains("--tmp")
        {
            let cleaned = s
                .replace("--tmp ", "")
                .replace(" --tmp", "")
                .replace("--tmp", "");
            *arg = serde_json::Value::String(cleaned);
            modified = true;
        }
    }
    modified
}

fn clean_antigravity_ide_mcp(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(path)?;
    let mut doc: serde_json::Value =
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}));
    let mut modified = false;
    if let Some(args) = doc
        .get_mut("mcpServers")
        .and_then(|s| s.get_mut("Ahma"))
        .and_then(|a| a.get_mut("args"))
        .and_then(|a| a.as_array_mut())
    {
        modified = remove_tmp_from_args(args);
    }
    if modified {
        let formatted = serde_json::to_string_pretty(&doc)?;
        std::fs::write(path, formatted)?;
    }
    Ok(())
}

fn antigravity_clean_grants(current_exe: Option<&Path>) -> Vec<String> {
    let raw_exe = current_exe
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "ahma".to_string());

    vec![
        format!("command({raw_exe} hooks run-shell)"),
        "command(ahma hooks run-shell)".to_string(),
        "command(regex:.*ahma.* hooks run-shell)".to_string(),
        format!("command({raw_exe} hooks run-shell --wrapped-by ahma-hooks-wrapper-v1)"),
        "command(ahma hooks run-shell --wrapped-by ahma-hooks-wrapper-v1)".to_string(),
        "command(regex:.*ahma.* hooks run-shell --wrapped-by ahma-hooks-wrapper-v1)".to_string(),
    ]
}

fn check_grant_array(arr: &serde_json::Value, clean_grants: &[String]) -> (usize, usize, bool) {
    let Some(items) = arr.as_array() else {
        return (0, 0, false);
    };
    let mut bloated = 0;
    let mut malformed = 0;
    for item in items {
        if let Some(s) = item.as_str() {
            if is_bloated_ahma_grant(s) {
                bloated += 1;
            } else if is_malformed_ahma_grant(s) {
                malformed += 1;
            }
        }
    }
    let missing = clean_grants
        .iter()
        .any(|cg| !items.iter().any(|i| i.as_str() == Some(cg.as_str())));
    (bloated, malformed, missing)
}

fn scan_antigravity_file(
    path: &Path,
    is_config: bool,
    clean_grants: &[String],
) -> (usize, usize, bool) {
    if !path.exists() {
        return (0, 0, false);
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return (0, 0, false);
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&content) else {
        return (0, 0, false);
    };
    let mut total_bloated = 0;
    let mut total_malformed = 0;
    let mut missing = false;

    if let Some(perms_allow) = doc.get("permissions").and_then(|p| p.get("allow")) {
        let (sb, sm, smiss) = check_grant_array(perms_allow, clean_grants);
        total_bloated += sb;
        total_malformed += sm;
        if smiss {
            missing = true;
        }
    } else {
        missing = true;
    }

    if is_config {
        if let Some(gpg_allow) = doc
            .get("userSettings")
            .and_then(|u| u.get("globalPermissionGrants"))
            .and_then(|g| g.get("allow"))
        {
            let (sb, sm, smiss) = check_grant_array(gpg_allow, clean_grants);
            total_bloated += sb;
            total_malformed += sm;
            if smiss {
                missing = true;
            }
        } else {
            missing = true;
        }
    }

    (total_bloated, total_malformed, missing)
}

fn check_ide_has_retired_tmp(ide_path: &Path) -> bool {
    if !ide_path.exists() {
        return false;
    }
    let Ok(content) = std::fs::read_to_string(ide_path) else {
        return false;
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    doc.get("mcpServers")
        .and_then(|s| s.get("Ahma"))
        .and_then(|a| a.get("args"))
        .and_then(|a| a.as_array())
        .is_some_and(|args| {
            args.iter()
                .any(|arg| arg.as_str().is_some_and(|s| s.contains("--tmp")))
        })
}

fn check_antigravity_permissions(
    home: Option<&Path>,
    current_exe: Option<&Path>,
    out: &mut Vec<Finding>,
) {
    let Some(home) = home else { return };
    let cli_path = home
        .join(".gemini")
        .join("antigravity-cli")
        .join("settings.json");
    let config_path = home.join(".gemini").join("config").join("config.json");
    let ide_path = home.join(".antigravity").join("mcp.json");

    if !cli_path.exists() && !config_path.exists() && !ide_path.exists() {
        return;
    }

    let clean_grants = antigravity_clean_grants(current_exe);

    let (cb, cm, cmiss) = scan_antigravity_file(&cli_path, false, &clean_grants);
    let (gb, gm, gmiss) = scan_antigravity_file(&config_path, true, &clean_grants);

    let total_bloated = cb + gb;
    let total_malformed = cm + gm;
    let missing_clean = (cli_path.exists() && cmiss) || (config_path.exists() && gmiss);
    let ide_has_retired_tmp = check_ide_has_retired_tmp(&ide_path);

    if total_bloated > 0 || total_malformed > 0 || missing_clean || ide_has_retired_tmp {
        let level = if total_bloated > 0 || total_malformed > 0 || ide_has_retired_tmp {
            Level::Warn
        } else {
            Level::Info
        };
        let mut detail = format!(
            "Found {total_bloated} bloated one-off wrapped command(s) and {total_malformed} malformed grant(s) across ~/.gemini settings. \
             These cause Antigravity to repeatedly prompt for permission on every tool call."
        );
        if ide_has_retired_tmp {
            detail.push_str(" Also found retired --tmp flag in ~/.antigravity/mcp.json.");
        }
        out.push(Finding {
            level,
            title: "Antigravity permission grants need cleanup".into(),
            detail,
            fix: Some(Fix::RepairAntigravityPermissions {
                cli_settings: cli_path.exists().then_some(cli_path),
                config_file: config_path.exists().then_some(config_path),
                ide_mcp: (ide_path.exists() && ide_has_retired_tmp).then_some(ide_path),
                clean_grants,
            }),
        });
    } else {
        out.push(Finding {
            level: Level::Ok,
            title: "Antigravity command permissions are clean".into(),
            detail: "Clean prefix grants are active in ~/.gemini config files; no bloated or malformed entries found.".into(),
            fix: None,
        });
    }
}

fn inspect_log_dir(dir: &Path) -> Option<(u64, Option<PathBuf>)> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut total: u64 = 0;
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        total += meta.len();
        if !entry.file_name().to_string_lossy().starts_with("ahma.log") {
            continue;
        }
        let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        if newest.as_ref().is_none_or(|(t, _)| modified > *t) {
            newest = Some((modified, entry.path()));
        }
    }
    Some((total, newest.map(|(_, p)| p)))
}

/// The newest log under `<workspace>/.ahma/logs`, its size, and its most
/// repeated warnings and errors.
fn check_logs(workspace: &Path, out: &mut Vec<Finding>) {
    let dir = workspace.join(".ahma").join("logs");
    let Some((total, newest)) = inspect_log_dir(&dir) else {
        return;
    };
    const LARGE: u64 = 500 * 1024 * 1024;
    if total > LARGE {
        out.push(Finding {
            level: Level::Warn,
            title: "Logs are large".into(),
            detail: format!(
                "{} MB in {}. Old files can be deleted safely.",
                total / (1024 * 1024),
                dir.display()
            ),
            fix: None,
        });
    }
    let Some(newest) = newest else { return };
    let Ok(text) = std::fs::read_to_string(&newest) else {
        return;
    };
    let top = repeated_warnings(&text, 3);
    if top.is_empty() {
        out.push(Finding {
            level: Level::Ok,
            title: "No warnings in the latest log".into(),
            detail: newest.display().to_string(),
            fix: None,
        });
        return;
    }
    let lines: Vec<String> = top.iter().map(|(msg, n)| format!("{n}× {msg}")).collect();
    out.push(Finding {
        level: Level::Info,
        title: "Most repeated warnings in the latest log".into(),
        detail: format!("{}\n{}", newest.display(), lines.join("\n")),
        fix: None,
    });
}

/// The `limit` most frequent WARN/ERROR messages, with digits masked so
/// repeats that differ only by an id or a time group together.
pub fn repeated_warnings(log: &str, limit: usize) -> Vec<(String, usize)> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for line in log.lines() {
        let Some(rest) = line
            .split_once(" WARN ")
            .or_else(|| line.split_once(" ERROR "))
            .map(|(_, rest)| rest)
        else {
            continue;
        };
        // `target: message` — keep the message.
        let message = rest.split_once(": ").map_or(rest, |(_, m)| m);
        let masked: String = message
            .chars()
            .take(120)
            .map(|c| if c.is_ascii_digit() { '#' } else { c })
            .collect();
        *counts.entry(masked.trim().to_string()).or_default() += 1;
    }
    let mut top: Vec<(String, usize)> = counts.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    top.truncate(limit);
    top
}

/// The report as plain text: one block per finding, fixes numbered from 1 in
/// the order [`fixes`] returns them.
pub fn render(findings: &[Finding]) -> String {
    let mut out = String::new();
    let mut n = 0;
    for f in findings {
        out.push_str(&format!("[{}] {}\n", f.level.marker(), f.title));
        for line in f.detail.lines() {
            out.push_str(&format!("    {line}\n"));
        }
        if let Some(fix) = &f.fix {
            n += 1;
            out.push_str(&format!("    fix {n}: {}\n", fix.describe()));
        }
    }
    out
}

/// The fixes in `findings`, in report order (fix 1 is the first).
pub fn fixes(findings: &[Finding]) -> Vec<Fix> {
    findings.iter().filter_map(|f| f.fix.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PersistentScope, ScopeAccess};

    fn input(home: &Path, workspace: &Path) -> DoctorInput {
        DoctorInput {
            settings_file: Some(home.join("settings.toml")),
            home_dir: Some(home.to_path_buf()),
            current_exe: Some(PathBuf::from("/usr/local/bin/ahma")),
            workspace: workspace.to_path_buf(),
            hub: HubStatus::NotRunning,
            version: "1".into(),
            build_id: "b".into(),
            git_auth: None,
        }
    }

    #[test]
    fn a_granted_folder_that_is_gone_is_reported_with_a_fix() {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let mut settings = AhmaSettings::default();
        settings.sandbox.persistent_scopes.push(PersistentScope {
            path: PathBuf::from("/opt/two-does-not-exist"),
            access: ScopeAccess::Rw,
            workspace: None,
            granted_by: Some("cargo_build".into()),
            granted_at: None,
            note: None,
        });
        settings
            .save_to(&home.path().join("settings.toml"))
            .unwrap();

        let findings = run(&input(home.path(), ws.path()));
        let fixes = fixes(&findings);
        assert_eq!(
            fixes,
            vec![Fix::RemoveMissingScopes(vec![PathBuf::from(
                "/opt/two-does-not-exist"
            )])]
        );
        assert!(render(&findings).contains("fix 1: Remove 1 granted folder"));
        assert_eq!(findings[0].level, Level::Warn, "most serious first");
    }

    #[test]
    fn an_unparseable_settings_file_is_a_problem_and_nothing_else_is_guessed() {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("settings.toml"), "[sandbox\n").unwrap();
        let findings = run(&input(home.path(), ws.path()));
        assert_eq!(findings[0].level, Level::Problem);
        assert!(fixes(&findings).is_empty());
    }

    #[test]
    fn repeated_warnings_group_lines_that_differ_only_by_numbers() {
        let log = "\
pid=1 2026-09-23T05:16:50Z  WARN ahma: Skipping persistent scope /opt/two: could not create
pid=2 2026-09-23T05:17:50Z  WARN ahma: Skipping persistent scope /opt/two: could not create
pid=3 2026-09-23T05:18:50Z  WARN ahma: refusing to grant git dir 17 times
pid=4 2026-09-23T05:18:51Z  INFO ahma: fine
";
        let top = repeated_warnings(log, 3);
        assert_eq!(top[0].1, 2);
        assert!(top[0].0.starts_with("Skipping persistent scope"));
        assert_eq!(top.len(), 2);
    }

    #[test]
    fn logs_with_warnings_are_summarised() {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let logs = ws.path().join(".ahma").join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join("ahma.log"),
            "x 2026  WARN t: hub unavailable\nx 2026  WARN t: hub unavailable\n",
        )
        .unwrap();
        let text = render(&run(&input(home.path(), ws.path())));
        assert!(text.contains("2× hub unavailable"), "{text}");
    }

    #[test]
    fn antigravity_bloated_and_malformed_grants_are_reported_and_repaired() {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();

        let cli_dir = home.path().join(".gemini").join("antigravity-cli");
        std::fs::create_dir_all(&cli_dir).unwrap();
        let cli_file = cli_dir.join("settings.json");
        let initial_cli = serde_json::json!({
            "permissions": {
                "allow": [
                    "mcp(Ahma/run_terminal_command)",
                    "command('/usr/local/bin/ahma' 'hooks' 'run-shell' '--wrapped-by' 'ahma-hooks-wrapper-v1' '--cwd' '/some/dir' '--command' 'tail -n 100 /path/to/log')",
                    "command(/usr/local/bin/\\ahma hooks run-shell.*)",
                    "command(git status)"
                ]
            }
        });
        std::fs::write(
            &cli_file,
            serde_json::to_string_pretty(&initial_cli).unwrap(),
        )
        .unwrap();

        let config_dir = home.path().join(".gemini").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_file = config_dir.join("config.json");
        let initial_config = serde_json::json!({
            "userSettings": {
                "globalPermissionGrants": {
                    "allow": [
                        "command(xcodebuild)",
                        "command(/usr/local/bin/ahma hooks run-shell.*)",
                        "command('/usr/local/bin/ahma' 'hooks' 'run-shell'.*)"
                    ]
                }
            },
            "permissions": {
                "allow": [
                    "command(/usr/local/bin/ahma hooks run-shell.*)"
                ]
            }
        });
        std::fs::write(
            &config_file,
            serde_json::to_string_pretty(&initial_config).unwrap(),
        )
        .unwrap();

        let ide_dir = home.path().join(".antigravity");
        std::fs::create_dir_all(&ide_dir).unwrap();
        let ide_file = ide_dir.join("mcp.json");
        let initial_ide = serde_json::json!({
            "mcpServers": {
                "Ahma": {
                    "command": "ahma",
                    "args": ["serve", "stdio", "--tools", "simplify", "--tmp", "--log-monitor"]
                }
            }
        });
        std::fs::write(
            &ide_file,
            serde_json::to_string_pretty(&initial_ide).unwrap(),
        )
        .unwrap();

        let findings = run(&input(home.path(), ws.path()));
        let report = render(&findings);
        assert!(
            report.contains("Antigravity permission grants need cleanup"),
            "{report}"
        );
        assert!(
            report.contains("retired --tmp flag in ~/.antigravity/mcp.json"),
            "{report}"
        );

        let fix_list = fixes(&findings);
        assert_eq!(fix_list.len(), 1);
        let Fix::RepairAntigravityPermissions { .. } = &fix_list[0] else {
            panic!("Expected RepairAntigravityPermissions fix");
        };

        let result = fix_list[0].apply("2026-09-25T12:00:00Z").unwrap();
        assert!(result.contains("Cleaned Antigravity permissions in 3 file(s)"));

        let cli_content = std::fs::read_to_string(&cli_file).unwrap();
        let cli_doc: serde_json::Value = serde_json::from_str(&cli_content).unwrap();
        let cli_allow = cli_doc["permissions"]["allow"].as_array().unwrap();
        assert!(
            !cli_allow
                .iter()
                .any(|v| v.as_str().unwrap().contains("--command"))
        );
        assert!(
            !cli_allow
                .iter()
                .any(|v| v.as_str().unwrap().ends_with(".*)"))
        );
        assert!(
            !cli_allow
                .iter()
                .any(|v| v.as_str().unwrap().contains(r"\."))
        );
        assert!(
            cli_allow
                .iter()
                .any(|v| v.as_str() == Some("mcp(Ahma/run_terminal_command)"))
        );
        assert!(
            cli_allow
                .iter()
                .any(|v| v.as_str() == Some("command(git status)"))
        );
        assert!(
            cli_allow
                .iter()
                .any(|v| v.as_str() == Some("command(/usr/local/bin/ahma hooks run-shell)"))
        );
        assert!(
            cli_allow
                .iter()
                .any(|v| v.as_str() == Some("command(ahma hooks run-shell)"))
        );
        assert!(
            cli_allow
                .iter()
                .any(|v| v.as_str() == Some("command(regex:.*ahma.* hooks run-shell)"))
        );

        let config_content = std::fs::read_to_string(&config_file).unwrap();
        let config_doc: serde_json::Value = serde_json::from_str(&config_content).unwrap();
        let gpg_allow = config_doc["userSettings"]["globalPermissionGrants"]["allow"]
            .as_array()
            .unwrap();
        assert!(
            gpg_allow
                .iter()
                .any(|v| v.as_str() == Some("command(xcodebuild)"))
        );
        assert!(
            !gpg_allow
                .iter()
                .any(|v| v.as_str().unwrap().ends_with(".*)"))
        );
        assert!(
            gpg_allow
                .iter()
                .any(|v| v.as_str() == Some("command(/usr/local/bin/ahma hooks run-shell)"))
        );

        let ide_content = std::fs::read_to_string(&ide_file).unwrap();
        let ide_doc: serde_json::Value = serde_json::from_str(&ide_content).unwrap();
        let ide_args = ide_doc["mcpServers"]["Ahma"]["args"].as_array().unwrap();
        assert!(!ide_args.iter().any(|v| v.as_str() == Some("--tmp")));
        assert!(ide_args.iter().any(|v| v.as_str() == Some("--log-monitor")));

        let findings_after = run(&input(home.path(), ws.path()));
        assert!(
            findings_after
                .iter()
                .any(|f| f.title == "Antigravity command permissions are clean")
        );
        assert!(fixes(&findings_after).is_empty());
    }

    fn hub_finding(hub: HubStatus) -> Finding {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let mut input = input(home.path(), ws.path());
        input.version = "0.21.9".into();
        input.build_id = "abc1234".into();
        input.hub = hub;
        run(&input)
            .into_iter()
            .find(|f| f.title.starts_with("Hub"))
            .expect("the hub always gets a finding")
    }

    /// The doctor names the hub's build from what the hub says about
    /// itself on its socket (SPEC R-HUB.2), not from a descriptor file
    /// that could outlive it.
    #[test]
    fn the_hub_is_reported_from_its_own_health() {
        let same = hub_finding(HubStatus::Running {
            version: Some("0.21.9+abc1234".into()),
        });
        assert_eq!(same.title, "Hub is running this build");
        assert_eq!(same.level, Level::Ok);

        let other = hub_finding(HubStatus::Running {
            version: Some("0.21.8+old0000".into()),
        });
        assert_eq!(other.title, "Hub is a different build");
        assert_eq!(other.level, Level::Warn);
        assert!(
            other.detail.contains("0.21.8+old0000") && other.detail.contains("0.21.9+abc1234"),
            "{}",
            other.detail
        );

        let mute = hub_finding(HubStatus::Running { version: None });
        assert_eq!(mute.title, "Hub is running");

        let absent = hub_finding(HubStatus::NotRunning);
        assert_eq!(absent.title, "Hub is not running");
        assert_eq!(absent.level, Level::Info);
    }

    /// Answer one `/health` request on `socket` with `body`.
    fn serve_health_once(socket: &Path, body: &'static str) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = crate::local_socket::LocalListener::bind(socket).unwrap();
        tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes()).await;
            let _ = stream.shutdown().await;
        });
    }

    #[tokio::test]
    async fn the_hub_is_probed_over_its_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mcp.sock");
        serve_health_once(&socket, r#"{"status":"OK","version":"0.21.9+abc1234"}"#);
        assert_eq!(
            probe_hub(&socket).await,
            HubStatus::Running {
                version: Some("0.21.9+abc1234".into())
            }
        );
        assert_eq!(
            probe_hub(&dir.path().join("absent.sock")).await,
            HubStatus::NotRunning
        );
    }

    /// `ahma doctor` and the TUI's `/doctor` are synchronous and the TUI's
    /// runs inside its runtime, so the blocking probe must be callable there.
    #[tokio::test]
    async fn the_blocking_probe_works_inside_a_runtime() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            probe_hub_blocking(&dir.path().join("absent.sock")),
            HubStatus::NotRunning
        );
    }
}

#[cfg(test)]
mod hook_and_grant_tool_tests {
    use super::*;

    fn input(home: &Path, workspace: &Path) -> DoctorInput {
        DoctorInput {
            settings_file: Some(home.join("settings.toml")),
            home_dir: Some(home.to_path_buf()),
            current_exe: Some(PathBuf::from("/usr/local/bin/ahma")),
            workspace: workspace.to_path_buf(),
            hub: HubStatus::NotRunning,
            version: "1".into(),
            build_id: "b".into(),
            git_auth: None,
        }
    }

    #[test]
    fn doctor_warns_when_grant_tools_are_auto_allowed_in_antigravity_settings() {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let cli_dir = home.path().join(".gemini").join("antigravity-cli");
        std::fs::create_dir_all(&cli_dir).unwrap();
        std::fs::write(
            cli_dir.join("settings.json"),
            r#"{"permissions":{"allow":["mcp(Ahma/run_terminal_command)","mcp(Ahma/sandbox_grant)"]},"toolPermission":"always-proceed"}"#,
        )
        .unwrap();
        let findings = run(&input(home.path(), ws.path()));
        let f = findings
            .iter()
            .find(|f| f.title.contains("auto-approves ahma's grant tools"))
            .expect("the auto-allowed grant tool must be reported");
        assert_eq!(f.level, Level::Warn);
        assert!(f.detail.contains("mcp(Ahma/sandbox_grant)"), "{}", f.detail);

        // A clean allow list says nothing.
        std::fs::write(
            cli_dir.join("settings.json"),
            r#"{"permissions":{"allow":["mcp(Ahma/run_terminal_command)"]}}"#,
        )
        .unwrap();
        let findings = run(&input(home.path(), ws.path()));
        assert!(
            !findings
                .iter()
                .any(|f| f.title.contains("auto-approves ahma's grant tools"))
        );
    }

    #[test]
    fn doctor_warns_when_a_shell_hook_has_no_edit_guard() {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let claude = home.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join("settings.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"'/usr/local/bin/ahma' 'hooks' 'exec' '--platform' 'claude'"}]}]}}"#,
        )
        .unwrap();
        let findings = run(&input(home.path(), ws.path()));
        let f = findings
            .iter()
            .find(|f| f.title.contains("Native file edits are not confined"))
            .expect("a guard-less shell hook must be reported");
        assert!(f.detail.contains("Claude Code"), "{}", f.detail);
        let info = findings
            .iter()
            .find(|f| f.title.contains("Terminal hooks apply ahma's own sandbox"))
            .expect("the enforcement model is stated");
        assert!(
            info.detail.contains("Bash sandbox is off"),
            "{}",
            info.detail
        );

        // With the guard present the warning goes away.
        std::fs::write(
            claude.join("settings.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"ahma hooks exec --platform claude"}]},{"matcher":"Edit|Write","hooks":[{"type":"command","command":"ahma hooks edit-guard --managed-id ahma-edit-guard-v1"}]}]}}"#,
        )
        .unwrap();
        let findings = run(&input(home.path(), ws.path()));
        assert!(
            !findings
                .iter()
                .any(|f| f.title.contains("Native file edits are not confined"))
        );
    }
}

#[cfg(test)]
mod git_auth_tests {
    use super::*;
    use crate::config::AhmaSettings;

    fn probe(keys: &[&str], identities: Option<usize>, helpers: &[&str]) -> GitAuthProbe {
        GitAuthProbe {
            ssh_keys: keys.iter().map(PathBuf::from).collect(),
            agent_identities: identities,
            credential_helpers: helpers.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// The sandbox denies reads of `~/.ssh/id_*`, so inside it ssh has only the
    /// agent — and on the host the user never noticed the agent was empty,
    /// because ssh read the key file directly. The finding says exactly what
    /// to run.
    #[test]
    fn an_empty_ssh_agent_with_keys_present_says_exactly_what_to_run() {
        let mut out = Vec::new();
        check_git_auth(
            &probe(&["/h/.ssh/id_ed25519"], Some(0), &[]),
            &AhmaSettings::default(),
            &mut out,
        );
        let f = out
            .iter()
            .find(|f| f.title.contains("SSH"))
            .expect("an SSH finding");
        assert_eq!(f.level, Level::Warn);
        assert!(f.detail.starts_with("One thing to do: "), "{}", f.detail);
        assert!(f.detail.contains("ssh-add"), "{}", f.detail);
        assert!(f.detail.contains("/h/.ssh/id_ed25519"), "{}", f.detail);
    }

    /// HTTPS is the more common transport. A keychain helper is blocked only by
    /// one sandbox setting, and a `gh` helper only by one deny entry; each
    /// finding names the exact key.
    #[test]
    fn https_helper_blocked_by_settings_is_reported_with_the_key() {
        let mut settings = AhmaSettings::default();
        settings.sandbox.allow_keychain = false;
        let mut out = Vec::new();
        check_git_auth(&probe(&[], None, &["osxkeychain"]), &settings, &mut out);
        let f = out
            .iter()
            .find(|f| f.title.contains("HTTPS"))
            .expect("an HTTPS finding");
        assert_eq!(f.level, Level::Warn);
        assert!(f.detail.starts_with("One thing to do: "), "{}", f.detail);
        assert!(f.detail.contains("allow_keychain"), "{}", f.detail);

        let mut settings = AhmaSettings::default();
        settings
            .sandbox
            .deny_credential_reads
            .push(PathBuf::from("~/.config/gh"));
        let mut out = Vec::new();
        check_git_auth(
            &probe(&[], None, &["!/opt/homebrew/bin/gh auth git-credential"]),
            &settings,
            &mut out,
        );
        let f = out.iter().find(|f| f.title.contains("HTTPS")).unwrap();
        assert_eq!(f.level, Level::Warn);
        assert!(f.detail.contains("deny_credential_reads"), "{}", f.detail);
    }

    #[test]
    fn working_git_auth_is_reported_as_nothing_to_do() {
        let mut out = Vec::new();
        check_git_auth(
            &probe(&["/h/.ssh/id_ed25519"], Some(1), &["osxkeychain"]),
            &AhmaSettings::default(),
            &mut out,
        );
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].level, Level::Info);
        assert!(
            out[0].detail.starts_with("Nothing more to do"),
            "{}",
            out[0].detail
        );
        assert!(out[0].detail.contains("osxkeychain"));
    }

    /// No key, no helper: every push will prompt for a password, which no
    /// sandboxed command can answer. Still one exact action.
    #[test]
    fn no_credentials_at_all_is_one_exact_action() {
        let mut out = Vec::new();
        check_git_auth(&probe(&[], None, &[]), &AhmaSettings::default(), &mut out);
        let f = out.iter().find(|f| f.title.contains("Git")).unwrap();
        assert_eq!(f.level, Level::Info);
        assert!(f.detail.starts_with("One thing to do: "), "{}", f.detail);
        assert!(f.detail.contains("gh auth login"), "{}", f.detail);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Git authentication under the sandbox (SPEC R-DOCTOR.4)
// ─────────────────────────────────────────────────────────────────────────────

/// How git can authenticate from inside the sandbox, as observed on this
/// machine. Collected once by [`GitAuthProbe::collect`]; the check itself is
/// pure so it can be tested without an agent or a keychain.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitAuthProbe {
    /// SSH private keys under `~/.ssh` (`id_*` without `.pub`).
    pub ssh_keys: Vec<PathBuf>,
    /// Identities the SSH agent holds: `Some(0)` is an empty agent, `None`
    /// means no agent answered (`SSH_AUTH_SOCK` unset or dead).
    pub agent_identities: Option<usize>,
    /// Every `credential.helper` git is configured with (system + global).
    pub credential_helpers: Vec<String>,
}

impl GitAuthProbe {
    /// Observe the real machine. Each probe is a short local command; a
    /// failure to run one is recorded as "unknown", never as an error.
    pub fn collect(home: Option<&Path>) -> Self {
        let ssh_keys = home
            .map(|h| h.join(".ssh"))
            .and_then(|dir| std::fs::read_dir(dir).ok())
            .map(|entries| {
                let mut keys: Vec<PathBuf> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                        name.starts_with("id_") && !name.ends_with(".pub")
                    })
                    .collect();
                keys.sort();
                keys
            })
            .unwrap_or_default();
        let agent_identities = std::process::Command::new("ssh-add")
            .arg("-l")
            .output()
            .ok()
            .and_then(|out| {
                let text = String::from_utf8_lossy(&out.stdout).to_string()
                    + &String::from_utf8_lossy(&out.stderr);
                if out.status.success() {
                    Some(text.lines().filter(|l| !l.trim().is_empty()).count())
                } else if text.contains("no identities") {
                    Some(0)
                } else {
                    None
                }
            });
        let mut cmd = std::process::Command::new("git");
        cmd.args(["config", "--get-all", "credential.helper"]);
        if let Some(h) = home {
            cmd.current_dir(h);
        }
        let credential_helpers = cmd
            .output()
            .ok()
            .map(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            ssh_keys,
            agent_identities,
            credential_helpers,
        }
    }
}

/// Whether `git` can authenticate from inside the sandbox, over SSH and over
/// HTTPS, and — when it cannot — the one exact thing to do.
///
/// The sandbox denies reads of `~/.ssh/id_*` (R6.2.3), so inside it ssh has
/// only the agent; on the host ssh reads the key file directly, which is why an
/// empty agent is invisible until the first sandboxed `git push`. HTTPS is the
/// more common transport and ahma does not block its helpers: the keychain is
/// allowed unless `[sandbox] allow_keychain = false`, and `~/.config/gh` is
/// readable unless listed in `deny_credential_reads`. Each finding names the
/// one key or command, per the "do you need to do anything" rule.
fn check_git_auth(probe: &GitAuthProbe, settings: &AhmaSettings, out: &mut Vec<Finding>) {
    let keys_present = !probe.ssh_keys.is_empty();
    let helpers = &probe.credential_helpers;
    let keychain_helper = helpers.iter().any(|h| h.contains("osxkeychain"));
    let gh_helper = helpers.iter().any(|h| h.contains("gh"));
    let gh_denied = settings.sandbox.deny_credential_reads.iter().any(|p| {
        let s = p.to_string_lossy();
        s.ends_with(".config/gh") || s.ends_with(".config/gh/")
    });
    let mut problems = 0usize;
    let first_key = probe
        .ssh_keys
        .first()
        .map(|k| k.display().to_string())
        .unwrap_or_else(|| "~/.ssh/id_ed25519".to_string());
    let add_cmd = if cfg!(target_os = "macos") {
        format!("ssh-add --apple-use-keychain {first_key}")
    } else {
        format!("ssh-add {first_key}")
    };

    if keys_present {
        match probe.agent_identities {
            Some(0) => {
                problems += 1;
                out.push(Finding {
                    level: Level::Warn,
                    title: "Git over SSH fails inside the sandbox: the SSH agent holds no key"
                        .into(),
                    detail: format!(
                        "One thing to do: run `{add_cmd}` in a terminal on the host.\n\nThe \
                         sandbox denies reads of your private keys (`{first_key}`, SPEC R6.2.3) \
                         and forwards only the agent socket, so a sandboxed `git fetch`/`push` \
                         over SSH can authenticate only through the agent. On the host ssh \
                         reads the key file directly, which is why this was invisible until now."
                    ),
                    fix: None,
                });
            }
            None => {
                problems += 1;
                out.push(Finding {
                    level: Level::Warn,
                    title: "Git over SSH fails inside the sandbox: no SSH agent answered".into(),
                    detail: format!(
                        "One thing to do: start an agent and load your key — `eval \"$(ssh-agent \
                         -s)\" && {add_cmd}` — then launch your editor or ahma from that shell so \
                         `SSH_AUTH_SOCK` is inherited.\n\nInside the sandbox the private key file \
                         is unreadable by design (SPEC R6.2.3); the agent is the only way ssh \
                         can sign."
                    ),
                    fix: None,
                });
            }
            Some(_) => {}
        }
    }

    if keychain_helper && !settings.sandbox.allow_keychain {
        problems += 1;
        out.push(Finding {
            level: Level::Warn,
            title: "Git over HTTPS fails inside the sandbox: the keychain helper is blocked".into(),
            detail: "One thing to do: set `allow_keychain = true` under `[sandbox]` in \
                     ~/.ahma/settings.toml (or remove the key; the default is on).\n\nYour \
                     `credential.helper` is `osxkeychain`, and `[sandbox] allow_keychain = false` \
                     denies sandboxed commands the login keychain, so every HTTPS push prompts \
                     for a password no sandboxed command can answer."
                .into(),
            fix: None,
        });
    } else if gh_helper && gh_denied {
        problems += 1;
        out.push(Finding {
            level: Level::Warn,
            title: "Git over HTTPS fails inside the sandbox: the `gh` helper cannot read its token"
                .into(),
            detail:
                "One thing to do: remove `~/.config/gh` from `[sandbox] deny_credential_reads` \
                     in ~/.ahma/settings.toml, or switch git to the keychain helper: `git config \
                     --global credential.helper osxkeychain`.\n\nYour `credential.helper` runs \
                     `gh auth git-credential`, which reads `~/.config/gh/hosts.yml`; that \
                     directory is on your deny list, so the helper fails inside the sandbox."
                    .into(),
            fix: None,
        });
    } else if helpers.is_empty() && !keys_present {
        out.push(Finding {
            level: Level::Info,
            title: "Git has no way to authenticate from inside the sandbox".into(),
            detail: "One thing to do: run `gh auth login` on the host, then `gh auth setup-git` \
                     (stores a token in the keychain, which the sandbox allows); or create an SSH \
                     key and load it with `ssh-add`.\n\nNo `credential.helper` is configured and \
                     no key is under ~/.ssh, so a sandboxed `git push` over HTTPS prompts for a \
                     password it cannot read. Public clones and fetches still work."
                .into(),
            fix: None,
        });
        return;
    }

    if problems == 0 && (keys_present || !helpers.is_empty()) {
        let https = if helpers.is_empty() {
            "HTTPS: no credential helper (public fetches only)".to_string()
        } else {
            format!("HTTPS via {}", helpers.join(", "))
        };
        let ssh = match (keys_present, probe.agent_identities) {
            (true, Some(n)) => format!(
                "SSH via the agent ({n} identit{})",
                if n == 1 { "y" } else { "ies" }
            ),
            (true, None) => "SSH: agent state unknown".to_string(),
            (false, _) => "SSH: no key under ~/.ssh".to_string(),
        };
        out.push(Finding {
            level: Level::Info,
            title: "Git can authenticate from inside the sandbox".into(),
            detail: format!(
                "Nothing more to do. {https}; {ssh}. The sandbox forwards the agent socket and \
                 allows the keychain; it denies only the private key files themselves (SPEC \
                 R6.2.3)."
            ),
            fix: None,
        });
    }
}
