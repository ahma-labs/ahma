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
    /// The daemon's runtime directory, if known.
    pub runtime_dir: Option<PathBuf>,
    /// This binary's version and build id, to compare with the daemon's.
    pub version: String,
    pub build_id: String,
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
            runtime_dir: crate::daemon_hub::runtime_dir(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            build_id: crate::BUILD_ID.to_string(),
        }
    }
}

/// Run every check. Findings are ordered most serious first.
pub fn run(input: &DoctorInput) -> Vec<Finding> {
    let mut findings = Vec::new();
    let settings = check_settings(input, &mut findings);
    if let Some(settings) = &settings {
        check_missing_scopes(settings, &mut findings);
        check_missing_workspaces(settings, &mut findings);
        check_trust(settings, &input.workspace, &mut findings);
    }
    check_daemon(input, &mut findings);
    check_antigravity_permissions(
        input.home_dir.as_deref(),
        input.current_exe.as_deref(),
        &mut findings,
    );
    check_logs(&input.workspace, &mut findings);
    findings.sort_by_key(|f| std::cmp::Reverse(f.level));
    findings
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

fn check_daemon(input: &DoctorInput, out: &mut Vec<Finding>) {
    let Some(dir) = &input.runtime_dir else {
        return;
    };
    let finding = match crate::daemon_endpoint::probe(dir) {
        crate::daemon_endpoint::EndpointState::Live(endpoint) => {
            if endpoint.version == input.version && endpoint.build_id == input.build_id {
                Finding {
                    level: Level::Ok,
                    title: "Daemon is running this build".into(),
                    detail: format!("ahma {} (pid {})", endpoint.version, endpoint.pid),
                    fix: None,
                }
            } else {
                Finding {
                    level: Level::Warn,
                    title: "Daemon is a different build".into(),
                    detail: format!(
                        "The daemon is ahma {} ({}), this is {} ({}). Behaviour follows the \
                         daemon until it restarts: quit every ahma window and editor session \
                         using ahma, and the next one starts the new build.",
                        endpoint.version, endpoint.build_id, input.version, input.build_id
                    ),
                    fix: None,
                }
            }
        }
        crate::daemon_endpoint::EndpointState::Stale => Finding {
            level: Level::Info,
            title: "Daemon left a stale descriptor".into(),
            detail: "It will be cleaned up when the next daemon starts.".into(),
            fix: None,
        },
        crate::daemon_endpoint::EndpointState::Absent => Finding {
            level: Level::Info,
            title: "Daemon is not running".into(),
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
            runtime_dir: None,
            version: "1".into(),
            build_id: "b".into(),
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
            "x 2026  WARN t: daemon unavailable\nx 2026  WARN t: daemon unavailable\n",
        )
        .unwrap();
        let text = render(&run(&input(home.path(), ws.path())));
        assert!(text.contains("2× daemon unavailable"), "{text}");
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
}
