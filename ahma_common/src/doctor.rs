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
        }
    }

    /// Make the change. Writes through [`AhmaSettings::update`] (never over a
    /// file it cannot parse) and records each removal in the audit log,
    /// stamped `now` (RFC 3339, supplied by the caller).
    pub fn apply(&self, now: &str) -> anyhow::Result<String> {
        let now = now.to_string();
        match self {
            Fix::RemoveMissingScopes(paths) => {
                AhmaSettings::update(|s| {
                    s.sandbox
                        .persistent_scopes
                        .retain(|scope| !paths.contains(&scope.path));
                })?;
                for path in paths {
                    append_audit(&audit_entry(
                        now.clone(),
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
            Fix::ForgetMissingWorkspaces(paths) => {
                AhmaSettings::update(|s| {
                    s.permissions
                        .tool_approvals
                        .retain(|a| !paths.contains(&a.workspace));
                })?;
                for path in paths {
                    append_audit(&audit_entry(
                        now.clone(),
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
        }
    }
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

/// The newest log under `<workspace>/.ahma/logs`, its size, and its most
/// repeated warnings and errors.
fn check_logs(workspace: &Path, out: &mut Vec<Finding>) {
    let dir = workspace.join(".ahma").join("logs");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut total: u64 = 0;
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        total += meta.len();
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("ahma.log") {
            continue;
        }
        let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        if newest.as_ref().is_none_or(|(t, _)| modified > *t) {
            newest = Some((modified, entry.path()));
        }
    }
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
    let Some((_, newest)) = newest else { return };
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
}
