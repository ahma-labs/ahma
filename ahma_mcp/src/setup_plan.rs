//! The `ahma setup` plan: proposed changes, one validator, one applier
//! (SPEC R-SETUP.3).
//!
//! Anything that wants to change a setting or a grant during setup — a
//! deterministic detector, a doctor fix, the user typing a request, a model's
//! suggestion — becomes a [`Proposal`] holding one [`Change`] from a **closed**
//! set. [`validate`] is the single gate: it applies the same checks the
//! dedicated commands apply (the trust tier of the target file, the ledger-key
//! rule, the hard grant denylist, the host patterns), so a proposal from any
//! source is refused for the same reasons, in the same words. [`apply`]
//! validates again (the file may have changed since the plan was shown) and then
//! calls the existing audited writer; it is called only after a human's `y` for
//! that one proposal. Nothing here writes during [`build_plan`] or [`validate`].

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use ahma_common::config::{
    AhmaSettings, ScopeAccess, SettingsFile, SettingsTier, ledger_key_command, setting_changes,
    settings_tier,
};
use ahma_common::doctor::Fix;
use ahma_common::scope_grant::{GrantRisk, NewGrant, classify_grant_risk, persist_grant};
use ahma_common::setup_detect::{Detected, Edit};
use anyhow::{Context, Result};

/// Where a proposal came from, shown beside it so the human knows how much to
/// trust it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A deterministic detector (`gpu`, `source-readers`, `profile`).
    Detector(&'static str),
    /// A finding of `ahma doctor`.
    Doctor,
    /// Something the user asked for in their own words.
    User,
    /// A model's suggestion; the name is the model.
    Model(String),
    /// Missing or stale integrations (MCP entries, hooks, skills).
    Install,
}

impl Source {
    /// A short label for the plan.
    pub fn label(&self) -> String {
        match self {
            Self::Detector(name) => format!("detected: {name}"),
            Self::Doctor => "ahma doctor".into(),
            Self::User => "you asked".into(),
            Self::Model(name) => format!("suggested by {name}"),
            Self::Install => "installation".into(),
        }
    }
}

/// An integration to (re)install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallItem {
    /// Bring every installed client's MCP entry up to date.
    McpConfig,
}

/// The closed set of things setup can change. A model's suggestion is parsed
/// into one of these or dropped; it cannot name anything else.
#[derive(Debug, Clone)]
pub enum Change {
    /// Set a setting to a TOML value, as `ahma settings set` reads it.
    Set {
        file: SettingsFile,
        key: String,
        value: String,
    },
    /// Return a setting to its default.
    Unset { file: SettingsFile, key: String },
    /// Add one entry to a list of strings.
    AddToList {
        file: SettingsFile,
        key: String,
        item: String,
    },
    /// Remove one entry from a list of strings.
    RemoveFromList {
        file: SettingsFile,
        key: String,
        item: String,
    },
    /// Grant a folder to this workspace, through the same path as
    /// `ahma sandbox grant`.
    GrantPath { path: PathBuf, read_only: bool },
    /// Allow a host for subprocess egress, as `ahma network allow`.
    NetworkAllow { host: String },
    /// Install or refresh an integration.
    Install(InstallItem),
    /// A doctor fix, applied exactly as `/doctor fix` applies it.
    DoctorFix(Fix),
}

/// One proposed change and the words that explain it.
#[derive(Debug, Clone)]
pub struct Proposal {
    /// What is proposed, in one line.
    pub title: String,
    /// Why — what in this project or this machine suggested it.
    pub why: String,
    /// What it allows, in plain words.
    pub allows: String,
    /// The change itself.
    pub change: Change,
    /// Who proposed it.
    pub source: Source,
}

/// How much care a proposal needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Risk {
    /// An ordinary change.
    Normal,
    /// A security-tier setting: it changes what the sandbox permits.
    Security,
    /// A grant with warnings the human must read first.
    High(Vec<String>),
}

/// Where setup reads and writes.
#[derive(Debug, Clone)]
pub struct Env {
    /// `~/.ahma/settings.toml`.
    pub user_file: PathBuf,
    /// This repository's `.ahma/settings.toml`, when it has a `.ahma` directory.
    pub project_file: Option<PathBuf>,
    /// The folder being set up.
    pub workspace: PathBuf,
}

impl Env {
    /// The real locations for `workspace`, or `None` without a home directory.
    pub fn for_workspace(workspace: &Path) -> Option<Self> {
        let project_dir = workspace.join(".ahma");
        Some(Self {
            user_file: ahma_common::config::settings_path()?,
            project_file: project_dir
                .is_dir()
                .then(|| ahma_common::config::project_settings_path(&project_dir)),
            workspace: workspace.to_path_buf(),
        })
    }

    fn file(&self, which: SettingsFile) -> Result<&Path, Refusal> {
        match which {
            SettingsFile::User => Ok(&self.user_file),
            SettingsFile::Project => self.project_file.as_deref().ok_or_else(|| {
                Refusal("this folder has no .ahma directory for a project settings file".into())
            }),
        }
    }
}

/// Why a proposal was refused, in words fit to show beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A proposal that passed [`validate`]: safe to show, and to apply on a `y`.
#[derive(Debug, Clone)]
pub struct Validated {
    /// The proposal, unchanged.
    pub proposal: Proposal,
    /// How much care it needs.
    pub risk: Risk,
    /// What applying it changes, e.g. `sandbox.allow_gpu: false -> true`.
    pub preview: String,
    /// The one command that undoes it.
    pub undo: String,
    /// Already in effect: nothing to do.
    pub already: bool,
}

fn refuse(msg: impl Into<String>) -> Refusal {
    Refusal(msg.into())
}

fn split_key(key: &str) -> Result<(&str, &str), Refusal> {
    key.split_once('.')
        .filter(|(t, k)| !t.is_empty() && !k.is_empty() && !k.contains('.'))
        .ok_or_else(|| refuse(format!("`{key}` is not a setting name (`<table>.<key>`)")))
}

/// The strings in the list `key` holds in `file`, or an error when the key does
/// not hold a list of text.
fn current_list(file: &Path, key: &str) -> Result<Vec<String>, Refusal> {
    let (table, field) = split_key(key)?;
    let settings = AhmaSettings::load_from_checked(file)
        .map_err(|e| refuse(format!("{}: {}", file.display(), e.summary())))?;
    let value = toml::Value::try_from(&settings)
        .ok()
        .and_then(|v| v.get(table)?.get(field).cloned());
    match value {
        None => Ok(Vec::new()),
        Some(toml::Value::Array(items)) => items
            .iter()
            .map(|v| v.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| refuse(format!("`{key}` is not a list of text"))),
        Some(_) => Err(refuse(format!("`{key}` is not a list of text"))),
    }
}

fn list_literal(items: &[String]) -> String {
    toml::Value::Array(items.iter().cloned().map(toml::Value::String).collect()).to_string()
}

/// `ahma settings set`/`unset` for `key`, the way a human would undo to `before`.
fn undo_command(file: SettingsFile, key: &str, before: &str) -> String {
    let scope = if file == SettingsFile::Project {
        " --project"
    } else {
        ""
    };
    // Back to the default is `unset`, which also leaves the file tidy.
    let is_default = ahma_common::config::setting_docs()
        .iter()
        .any(|d| d.dotted() == key && d.default == before);
    if before == "(unset)" || is_default {
        format!("ahma settings unset{scope} {key}")
    } else if before.contains('\'') {
        format!(
            "ahma settings set{scope} {key} \"{}\"",
            before.replace('"', "\\\"")
        )
    } else {
        format!("ahma settings set{scope} {key} '{before}'")
    }
}

fn setting_risk(key: &str) -> Risk {
    match split_key(key).ok().and_then(|(t, k)| settings_tier(t, k)) {
        Some(SettingsTier::Security) => Risk::Security,
        _ => Risk::Normal,
    }
}

/// Validate a settings edit by previewing the write it would make. Every check
/// `ahma settings set` makes applies, with its own words.
fn validate_set(
    env: &Env,
    file: SettingsFile,
    key: &str,
    raw: Option<&str>,
    proposal: &Proposal,
) -> Result<Validated, Refusal> {
    let path = env.file(file)?;
    let preview =
        AhmaSettings::preview_set_as(file, path, key, raw).map_err(|e| refuse(format!("{e:#}")))?;
    let scope = if file == SettingsFile::Project {
        "repository"
    } else {
        "your"
    };
    Ok(Validated {
        proposal: proposal.clone(),
        risk: setting_risk(key),
        preview: format!(
            "{key}: {} -> {} ({scope} settings)",
            preview.edit.before, preview.edit.after
        ),
        undo: undo_command(file, key, &preview.edit.before),
        already: preview.edit.before == preview.edit.after,
    })
}

/// The single gate (SPEC R-SETUP.3). Reads only; never writes.
pub fn validate(env: &Env, proposal: &Proposal) -> Result<Validated, Refusal> {
    match &proposal.change {
        Change::Set { file, key, value } => validate_set(env, *file, key, Some(value), proposal),
        Change::Unset { file, key } => validate_set(env, *file, key, None, proposal),
        Change::AddToList { file, key, item } | Change::RemoveFromList { file, key, item } => {
            let adding = matches!(proposal.change, Change::AddToList { .. });
            let path = env.file(*file)?;
            if let Some(command) = ledger_key_command(split_key(key)?.0, split_key(key)?.1) {
                return Err(refuse(format!(
                    "`{key}` holds grants, which setup does not write as a setting. Use {command}."
                )));
            }
            if item.trim().is_empty() {
                return Err(refuse("an empty entry".to_string()));
            }
            let mut list = current_list(path, key)?;
            let present = list.contains(item);
            if adding && !present {
                list.push(item.clone());
            } else if !adding && present {
                list.retain(|x| x != item);
            }
            let mut v = validate_set(env, *file, key, Some(&list_literal(&list)), proposal)?;
            v.already = if adding { present } else { !present };
            Ok(v)
        }
        Change::GrantPath { path, read_only } => validate_grant(env, path, *read_only, proposal),
        Change::NetworkAllow { host } => {
            use crate::egress::host_pattern::HostPattern;
            use crate::mcp_service::handlers::network_grant_tool::{
                NetGrantRisk, classify_network_risk,
            };
            let pattern = HostPattern::parse(host)
                .map_err(|e| refuse(format!("invalid host pattern '{host}': {e}")))?;
            // The same hard denylist the agent-facing `network_grant` applies:
            // setup may be driven by a model, and a blanket or local
            // destination is a decision for `ahma network allow` by hand.
            let risk = match classify_network_risk(&pattern, host) {
                NetGrantRisk::Refused(reason) => return Err(refuse(reason)),
                NetGrantRisk::High(reason) => Risk::High(vec![reason]),
                NetGrantRisk::Normal => Risk::Security,
            };
            let current = AhmaSettings::load_from_checked(&env.user_file)
                .map_err(|e| refuse(format!("{}: {}", env.user_file.display(), e.summary())))?;
            Ok(Validated {
                proposal: proposal.clone(),
                risk,
                preview: format!("[network].allow += `{host}`"),
                undo: format!("ahma network revoke {host}"),
                already: current
                    .network
                    .allow
                    .iter()
                    .any(|p| p.eq_ignore_ascii_case(host)),
            })
        }
        Change::Install(InstallItem::McpConfig) => {
            let drifts = crate::setup::detect_mcp_config_drifts();
            let names: Vec<&str> = drifts.iter().map(|d| d.platform_name).collect();
            Ok(Validated {
                proposal: proposal.clone(),
                risk: Risk::Normal,
                preview: if names.is_empty() {
                    "every installed client's MCP entry is current".into()
                } else {
                    format!("update the MCP entry for {}", names.join(", "))
                },
                undo: "ahma uninstall --mcp".into(),
                already: drifts.is_empty(),
            })
        }
        Change::DoctorFix(fix) => Ok(Validated {
            proposal: proposal.clone(),
            risk: Risk::Normal,
            preview: fix.describe(),
            undo: "none: a doctor fix is recorded in the audit log".into(),
            already: false,
        }),
    }
}

fn validate_grant(
    env: &Env,
    path: &Path,
    read_only: bool,
    proposal: &Proposal,
) -> Result<Validated, Refusal> {
    if !path.is_absolute() {
        return Err(refuse(format!(
            "`{}` is not an absolute path; setup grants a specific folder",
            path.display()
        )));
    }
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let home = ahma_common::config::ahma_home_dir();
    let risk = match classify_grant_risk(
        &canonical,
        home.as_deref(),
        std::slice::from_ref(&env.workspace),
    ) {
        GrantRisk::Refused(reason) => {
            return Err(refuse(format!(
                "{} cannot be granted: {reason}. This is a hard limit with no override.",
                canonical.display()
            )));
        }
        GrantRisk::High(reasons) => Risk::High(reasons),
        GrantRisk::Normal => Risk::Security,
    };
    let current = AhmaSettings::load_from_checked(&env.user_file)
        .map_err(|e| refuse(format!("{}: {}", env.user_file.display(), e.summary())))?;
    let access = if read_only { "read-only" } else { "read-write" };
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let workspace = canon(&env.workspace);
    let already = current.sandbox.persistent_scopes.iter().any(|s| {
        canon(&s.path) == canonical
            && s.workspace.as_deref().map(canon).as_ref() == Some(&workspace)
            && (s.access == ScopeAccess::Ro) == read_only
    });
    Ok(Validated {
        proposal: proposal.clone(),
        risk,
        preview: format!(
            "grant {} ({access}) to {}",
            canonical.display(),
            env.workspace.display()
        ),
        undo: format!("ahma sandbox revoke {}", path.display()),
        already,
    })
}

/// Apply one proposal. Validates again first (the files may have changed since
/// the plan was shown), then writes through the existing audited writer and
/// returns a one-line report. Call only after the human's `y` for **this**
/// proposal. `surface` is who asked (`cli`, `tui`); `now` is RFC 3339.
pub fn apply(env: &Env, proposal: &Proposal, surface: &str, now: &str) -> Result<String> {
    let v = validate(env, proposal).map_err(|r| anyhow::anyhow!("{r}"))?;
    if v.already {
        return Ok(format!("already in effect: {}", v.preview));
    }
    match &proposal.change {
        Change::Set { file, key, value } => {
            write_setting(env, *file, key, Some(value), surface, now)
        }
        Change::Unset { file, key } => write_setting(env, *file, key, None, surface, now),
        Change::AddToList { file, key, .. } | Change::RemoveFromList { file, key, .. } => {
            // The validated preview holds the merged list; recompute from the
            // file as it is now so two proposals to one list never overwrite
            // each other.
            let path = env.file(*file).map_err(|r| anyhow::anyhow!("{r}"))?;
            let mut list = current_list(path, key).map_err(|r| anyhow::anyhow!("{r}"))?;
            match &proposal.change {
                Change::AddToList { item, .. } => list.push(item.clone()),
                Change::RemoveFromList { item, .. } => list.retain(|x| x != item),
                _ => {}
            }
            write_setting(env, *file, key, Some(&list_literal(&list)), surface, now)
        }
        Change::GrantPath { path, read_only } => {
            persist_grant(
                &env.user_file,
                NewGrant {
                    path,
                    access: if *read_only {
                        ScopeAccess::Ro
                    } else {
                        ScopeAccess::Rw
                    },
                    granted_by: Some("setup".into()),
                    granted_at: Some(now.chars().take(10).collect()),
                    note: None,
                    surface,
                    live_scopes: std::slice::from_ref(&env.workspace),
                    workspace: Some(&env.workspace),
                    expires_at: None,
                },
            )?;
            Ok(v.preview)
        }
        Change::NetworkAllow { host } => {
            ahma_common::net_approval::persist_net_allow(&env.user_file, host, surface)?;
            Ok(v.preview)
        }
        Change::Install(InstallItem::McpConfig) => {
            for drift in crate::setup::detect_mcp_config_drifts() {
                drift
                    .apply_update()
                    .with_context(|| format!("updating {}", drift.config_path.display()))?;
            }
            Ok(v.preview)
        }
        Change::DoctorFix(fix) => fix.apply(now),
    }
}

fn write_setting(
    env: &Env,
    file: SettingsFile,
    key: &str,
    raw: Option<&str>,
    surface: &str,
    now: &str,
) -> Result<String> {
    let path = env.file(file).map_err(|r| anyhow::anyhow!("{r}"))?;
    let edit = AhmaSettings::set_in_file_as(file, path, key, raw)?;
    if edit.before != edit.after {
        ahma_common::permissions::append_audit(&ahma_common::permissions::setting_audit_entry(
            now,
            surface,
            file.label(),
            key,
            &edit.before,
            &edit.after,
        ));
    }
    Ok(format!("{key}: {} -> {}", edit.before, edit.after))
}

/// Turn a detector's finding into a proposal.
pub fn from_detected(d: Detected) -> Proposal {
    let change = match d.edit {
        Edit::Set { key, value } => Change::Set {
            file: d.file,
            key: key.to_string(),
            value,
        },
        Edit::AddToList { key, item } => Change::AddToList {
            file: d.file,
            key: key.to_string(),
            item,
        },
    };
    Proposal {
        title: d.title,
        why: d.why,
        allows: d.allows,
        change,
        source: Source::Detector(d.detector),
    }
}

/// The plan: what is proposed and still to do, what was refused, and how much
/// was already in effect.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// Proposals that passed validation and are not yet in effect.
    pub items: Vec<Validated>,
    /// Proposals the validator refused, with the reason.
    pub refused: Vec<(Proposal, Refusal)>,
    /// How many proposals were already in effect (and so are not listed).
    pub settled: usize,
}

impl Plan {
    /// Nothing left to do: the second run of setup says so.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Validate `proposals` into a plan. Reads only.
pub fn plan_from(env: &Env, proposals: Vec<Proposal>) -> Plan {
    let mut plan = Plan::default();
    for proposal in proposals {
        match validate(env, &proposal) {
            Ok(v) if v.already => plan.settled += 1,
            Ok(v) => plan.items.push(v),
            Err(r) => plan.refused.push((proposal, r)),
        }
    }
    plan
}

/// Every deterministic proposal for `env.workspace`: the detectors, then
/// `installs` if asked for, then a proposal per doctor fix in `fixes`.
pub fn build_plan(env: &Env, fixes: Vec<Fix>, installs: bool) -> Plan {
    let settings = AhmaSettings::load_from_checked(&env.user_file).unwrap_or_default();
    let mut proposals: Vec<Proposal> = ahma_common::setup_detect::detect(&env.workspace, &settings)
        .into_iter()
        .map(from_detected)
        .collect();
    if installs {
        proposals.push(Proposal {
            title: "Bring installed clients' MCP entries up to date".into(),
            why: "An installed client's ahma entry differs from the one this version recommends."
                .into(),
            allows: "Rewrites only ahma's own entry in that client's MCP config.".into(),
            change: Change::Install(InstallItem::McpConfig),
            source: Source::Install,
        });
    }
    proposals.extend(fixes.into_iter().map(|fix| Proposal {
        title: fix.describe(),
        why: "Found by `ahma doctor`.".into(),
        allows: "Only what the sentence above says.".into(),
        change: Change::DoctorFix(fix),
        source: Source::Doctor,
    }));
    plan_from(env, proposals)
}

/// The plan as text, for `ahma setup --plan` and for a model's briefing.
pub fn render_plan(plan: &Plan) -> String {
    let mut out = String::new();
    if plan.items.is_empty() && plan.refused.is_empty() {
        let _ = writeln!(
            out,
            "No changes proposed ({} already in effect).",
            plan.settled
        );
        return out;
    }
    let _ = writeln!(out, "{} change(s) proposed:", plan.items.len());
    for (i, v) in plan.items.iter().enumerate() {
        let marker = match &v.risk {
            Risk::Normal => "",
            Risk::Security => "  [changes the sandbox boundary]",
            Risk::High(_) => "  [HIGH RISK]",
        };
        let _ = writeln!(out, "\n{}. {}{marker}", i + 1, v.proposal.title);
        let _ = writeln!(out, "   source:  {}", v.proposal.source.label());
        let _ = writeln!(out, "   why:     {}", v.proposal.why);
        let _ = writeln!(out, "   allows:  {}", v.proposal.allows);
        let _ = writeln!(out, "   change:  {}", v.preview);
        if let Risk::High(reasons) = &v.risk {
            for r in reasons {
                let _ = writeln!(out, "   warning: {r}");
            }
        }
        let _ = writeln!(out, "   undo:    {}", v.undo);
    }
    for (p, reason) in &plan.refused {
        let _ = writeln!(
            out,
            "\nrefused: {} ({}): {reason}",
            p.title,
            p.source.label()
        );
    }
    if plan.settled > 0 {
        let _ = writeln!(out, "\n({} more already in effect.)", plan.settled);
    }
    out
}

/// `ahma setup --plan [--apply]`: print the plan for `path` (default: the
/// current directory). With `apply` and a terminal, offer each item in turn and
/// apply only on `y`; without a terminal, change nothing and say so.
pub fn run_plan_command(path: Option<PathBuf>, apply_each: bool) -> Result<()> {
    use std::io::{BufRead, IsTerminal, Write};

    let workspace = match path {
        Some(p) => p,
        None => std::env::current_dir().context("cannot read the current directory")?,
    };
    let workspace = workspace
        .canonicalize()
        .with_context(|| format!("cannot read {}", workspace.display()))?;
    let env = Env::for_workspace(&workspace)
        .context("cannot find the home directory for ~/.ahma/settings.toml")?;
    let mut input = ahma_common::doctor::DoctorInput::for_workspace(&workspace);
    input.confined_daemons = crate::sandbox::confinement::confined_daemons();
    let fixes = ahma_common::doctor::fixes(&ahma_common::doctor::run(&input));
    let plan = build_plan(&env, fixes, true);
    print!("{}", render_plan(&plan));
    if plan.is_empty() || !apply_each {
        if !plan.is_empty() {
            println!(
                "\nNothing was changed. `ahma setup --plan --apply` offers each one (nothing \
                 applies without a y), or run `ahma setup` for the guided view."
            );
        }
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        println!("\n--apply needs a terminal to ask you; nothing was changed.");
        return Ok(());
    }
    let stdin = std::io::stdin();
    for (i, item) in plan.items.iter().enumerate() {
        let extra = match &item.risk {
            Risk::High(_) => " This is HIGH RISK; read the warnings above.",
            Risk::Security => " This changes what the sandbox permits.",
            Risk::Normal => "",
        };
        print!("\n{}. {}{extra}\nApply? [y/N] ", i + 1, item.proposal.title);
        std::io::stdout().flush()?;
        let mut answer = String::new();
        stdin.lock().read_line(&mut answer)?;
        if answer.trim().eq_ignore_ascii_case("y") {
            let now = chrono::Local::now().to_rfc3339();
            match apply(&env, &item.proposal, "cli", &now) {
                Ok(done) => println!("Done: {done}"),
                Err(e) => println!("Could not apply: {e:#}"),
            }
        } else {
            println!("Skipped.");
        }
    }
    Ok(())
}

/// The audit-visible changes between two settings, for a TUI summary.
pub fn describe_changes(before: &AhmaSettings, after: &AhmaSettings) -> Vec<String> {
    setting_changes(before, after)
        .into_iter()
        .map(|(k, was, now)| format!("{k}: {was} -> {now}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _home: tempfile::TempDir,
        ws: tempfile::TempDir,
        env: Env,
    }

    fn fixture() -> Fixture {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join(".ahma")).unwrap();
        let env = Env {
            user_file: home.path().join(".ahma").join("settings.toml"),
            project_file: Some(ws.path().join(".ahma").join("settings.toml")),
            workspace: ws.path().to_path_buf(),
        };
        Fixture {
            _home: home,
            ws,
            env,
        }
    }

    fn proposal(change: Change) -> Proposal {
        Proposal {
            title: "t".into(),
            why: "w".into(),
            allows: "a".into(),
            change,
            source: Source::User,
        }
    }

    fn set(file: SettingsFile, key: &str, value: &str) -> Proposal {
        proposal(Change::Set {
            file,
            key: key.into(),
            value: value.into(),
        })
    }

    #[test]
    fn a_model_cannot_put_a_security_key_in_the_repository_file() {
        let f = fixture();
        let mut p = set(SettingsFile::Project, "sandbox.allow_gpu", "true");
        p.source = Source::Model("any".into());
        let err = validate(&f.env, &p).unwrap_err();
        assert!(err.0.contains("security setting"), "{err}");
        // The same key in the user's file is a proposal the human may accept.
        let ok = validate(
            &f.env,
            &set(SettingsFile::User, "sandbox.allow_gpu", "true"),
        )
        .unwrap();
        assert_eq!(ok.risk, Risk::Security);
        assert!(!ok.already);
        assert!(ok.preview.contains("false -> true"), "{}", ok.preview);
        assert_eq!(ok.undo, "ahma settings unset sandbox.allow_gpu");
    }

    #[test]
    fn validation_refuses_what_settings_set_refuses() {
        let f = fixture();
        for (key, value) in [
            ("sandbox.nonsense", "true"),
            ("tools.timeout_secs", "soon"),
            ("network.allow", "[\"a.com\"]"),
            ("sandbox.persistent_scopes", "[]"),
            ("nodots", "1"),
        ] {
            assert!(
                validate(&f.env, &set(SettingsFile::User, key, value)).is_err(),
                "{key} = {value} must be refused"
            );
        }
        // A list entry for a ledger key is refused too, by name.
        let add = proposal(Change::AddToList {
            file: SettingsFile::User,
            key: "web.always_allow".into(),
            item: "x.com".into(),
        });
        assert!(
            validate(&f.env, &add)
                .unwrap_err()
                .0
                .contains("ahma web allow")
        );
    }

    #[test]
    fn validating_and_planning_write_nothing() {
        let f = fixture();
        let p = set(SettingsFile::User, "sandbox.allow_gpu", "true");
        let _ = validate(&f.env, &p).unwrap();
        let plan = plan_from(&f.env, vec![p]);
        assert_eq!(plan.items.len(), 1);
        assert!(!f.env.user_file.exists());
    }

    #[test]
    fn list_proposals_merge_at_apply_time_and_never_overwrite_each_other() {
        let f = fixture();
        let add = |item: &str| {
            proposal(Change::AddToList {
                file: SettingsFile::User,
                key: "tools.source_readers".into(),
                item: item.into(),
            })
        };
        // Both are validated against the same (empty) file, as in one plan.
        let a = add("scripts/a.sh");
        let b = add("scripts/b.sh");
        assert!(!validate(&f.env, &a).unwrap().already);
        assert!(!validate(&f.env, &b).unwrap().already);
        apply(&f.env, &a, "cli", "2026-11-01T00:00:00+00:00").unwrap();
        apply(&f.env, &b, "cli", "2026-11-01T00:00:00+00:00").unwrap();
        let got = AhmaSettings::load_from_checked(&f.env.user_file).unwrap();
        assert_eq!(got.tools.source_readers, ["scripts/a.sh", "scripts/b.sh"]);
        // Applying again is a no-op, and a plan of the same proposals is empty.
        assert!(
            apply(&f.env, &a, "cli", "t")
                .unwrap()
                .contains("already in effect")
        );
        let plan = plan_from(&f.env, vec![a, b]);
        assert!(plan.is_empty());
        assert_eq!(plan.settled, 2);
        assert!(render_plan(&plan).starts_with("No changes proposed (2 already in effect)"));
    }

    #[test]
    fn a_hard_denylist_path_is_refused_whoever_proposes_it() {
        let f = fixture();
        let home = ahma_common::config::ahma_home_dir().expect("a home directory");
        for path in [home.join(".ssh"), home.clone(), PathBuf::from("/")] {
            let mut p = proposal(Change::GrantPath {
                path: path.clone(),
                read_only: true,
            });
            p.source = Source::Model("m".into());
            let err = validate(&f.env, &p).unwrap_err();
            assert!(err.0.contains("hard limit"), "{}: {err}", path.display());
        }
        let relative = proposal(Change::GrantPath {
            path: PathBuf::from("relative/dir"),
            read_only: false,
        });
        assert!(
            validate(&f.env, &relative)
                .unwrap_err()
                .0
                .contains("absolute")
        );
    }

    #[test]
    fn a_grant_is_written_through_the_audited_writer_for_this_workspace() {
        let f = fixture();
        let granted = tempfile::tempdir().unwrap();
        let p = proposal(Change::GrantPath {
            path: granted.path().to_path_buf(),
            read_only: true,
        });
        let v = validate(&f.env, &p).unwrap();
        assert!(v.preview.contains("read-only"), "{}", v.preview);
        apply(&f.env, &p, "cli", "2026-11-01T00:00:00+00:00").unwrap();
        let got = AhmaSettings::load_from_checked(&f.env.user_file).unwrap();
        let scope = got.sandbox.persistent_scopes.first().expect("a grant");
        assert_eq!(scope.access, ScopeAccess::Ro);
        assert_eq!(scope.granted_by.as_deref(), Some("setup"));
        assert!(
            scope.workspace.is_some(),
            "bound to the workspace, not global"
        );
        assert!(validate(&f.env, &p).unwrap().already);
    }

    #[test]
    fn a_bad_host_is_refused_and_a_good_one_is_not_yet_allowed() {
        let f = fixture();
        for host in ["*", "localhost", "10.0.0.1", ""] {
            assert!(
                validate(
                    &f.env,
                    &proposal(Change::NetworkAllow { host: host.into() })
                )
                .is_err(),
                "{host:?}"
            );
        }
        let ok = validate(
            &f.env,
            &proposal(Change::NetworkAllow {
                host: "crates.io".into(),
            }),
        )
        .unwrap();
        assert!(!ok.already);
        assert_eq!(ok.undo, "ahma network revoke crates.io");
    }

    #[test]
    fn the_plan_comes_from_the_project_and_a_second_run_is_empty() {
        let f = fixture();
        std::fs::write(f.ws.path().join("tiny.gguf"), "x").unwrap();
        let plan = build_plan(&f.env, Vec::new(), false);
        assert_eq!(plan.items.len(), 1, "{}", render_plan(&plan));
        let text = render_plan(&plan);
        assert!(text.contains("1 change(s) proposed"), "{text}");
        assert!(
            text.contains("detected: gpu") && text.contains("tiny.gguf"),
            "{text}"
        );
        assert!(text.contains("[changes the sandbox boundary]"), "{text}");
        assert!(text.contains("undo:"), "{text}");

        let item = plan.items[0].proposal.clone();
        apply(&f.env, &item, "cli", "2026-11-01T00:00:00+00:00").unwrap();
        let again = build_plan(&f.env, Vec::new(), false);
        assert!(again.is_empty(), "{}", render_plan(&again));
        assert_eq!(
            again.settled, 0,
            "the detector no longer fires once the GPU is on"
        );
        assert!(render_plan(&again).starts_with("No changes proposed"));
    }

    #[test]
    fn the_project_file_is_unavailable_without_a_dot_ahma_directory() {
        let mut f = fixture();
        f.env.project_file = None;
        let err = validate(
            &f.env,
            &set(SettingsFile::Project, "tools.timeout_secs", "90"),
        )
        .unwrap_err();
        assert!(err.0.contains("no .ahma directory"), "{err}");
    }
}
