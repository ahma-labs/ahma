//! CLI subcommand implementations.
//!
//! This module contains the handlers for `ahma settings`, `ahma prompts`,
//! `ahma bundle`, `ahma tool info`, and `ahma tool validate` commands.
//! Extracted from `cli/mod.rs` to keep each file focused and manageable.

use super::{
    AppConfig, BundleArgs, BundleAuditArgs, BundleCommand, BundleSignArgs, BundleVerifyArgs,
    InfoArgs, LogsArgs, LogsCommand, PermissionsArgs, PermissionsCommand, PromptsArgs,
    PromptsCommand, SandboxArgs, SandboxCommand, SettingsArgs, SettingsCommand, WebArgs,
    WebCommand,
};
use crate::shell::{list_tools, resolution};
use anyhow::{Context, Result};
use std::path::PathBuf;

// ─────────────────────────────────────────────────────────────────────────────
// Settings command
// ─────────────────────────────────────────────────────────────────────────────

pub(crate) fn run_settings_command(args: SettingsArgs) -> Result<()> {
    use ahma_common::config::{AhmaSettings, settings_path};

    match args.command {
        SettingsCommand::Init { force, path } => {
            let target = path.unwrap_or_else(|| {
                settings_path().unwrap_or_else(|| PathBuf::from(".ahma/settings.toml"))
            });
            AhmaSettings::write_defaults(&target, force)?;
            println!("Settings file written to: {}", target.display());
            println!();
            println!("Edit the file to override defaults.");
            println!("Run `ahma settings show` to see the effective configuration.");
            Ok(())
        }
        SettingsCommand::Show => {
            // Load from the standard location and print each field with its source.
            let s = AhmaSettings::load();
            let d = AhmaSettings::default();

            println!("# Effective Ahma settings");
            println!(
                "# Sources: [file] = ~/.ahma/settings.toml  [env] = AHMA_* (deprecated)  [default] = compiled-in"
            );
            println!();

            macro_rules! show_field {
                ($label:expr, $val:expr, $def:expr) => {
                    let source = if $val != $def { "[file]" } else { "[default]" };
                    println!("{:<45} = {:?}  # {}", $label, $val, source);
                };
            }

            println!("[features]");
            show_field!("simplify", s.features.simplify, d.features.simplify);
            show_field!("vault", s.features.vault, d.features.vault);
            show_field!("cluster", s.features.cluster, d.features.cluster);
            show_field!("egress", s.features.egress, d.features.egress);
            show_field!("artifact", s.features.artifact, d.features.artifact);
            show_field!("decompose", s.features.decompose, d.features.decompose);
            println!();
            println!("[lmstudio]");
            show_field!("base_url", &s.lmstudio.base_url, &d.lmstudio.base_url);
            show_field!("model", &s.lmstudio.model, &d.lmstudio.model);
            println!();
            println!("[tools]");
            show_field!("timeout_secs", s.tools.timeout_secs, d.tools.timeout_secs);
            show_field!("force_sync", s.tools.force_sync, d.tools.force_sync);
            show_field!("hot_reload", s.tools.hot_reload, d.tools.hot_reload);
            show_field!("skip_probes", s.tools.skip_probes, d.tools.skip_probes);
            println!();
            println!("[sandbox]");
            show_field!("disable", s.sandbox.disable, d.sandbox.disable);
            show_field!("tmp_access", s.sandbox.tmp_access, d.sandbox.tmp_access);
            show_field!(
                "disable_temp",
                s.sandbox.disable_temp,
                d.sandbox.disable_temp
            );
            show_field!("defer", s.sandbox.defer, d.sandbox.defer);
            show_field!(
                "sandbox_directory",
                &s.sandbox.sandbox_directory,
                &d.sandbox.sandbox_directory
            );
            show_field!(
                "use_sandbox_directory",
                s.sandbox.use_sandbox_directory,
                d.sandbox.use_sandbox_directory
            );
            println!();
            println!("[logging]");
            show_field!("target", &s.logging.target, &d.logging.target);
            show_field!("log_monitor", s.logging.log_monitor, d.logging.log_monitor);
            show_field!(
                "monitor_rate_limit_secs",
                s.logging.monitor_rate_limit_secs,
                d.logging.monitor_rate_limit_secs
            );
            println!();

            println!("[http]");
            show_field!(
                "handshake_timeout_secs",
                s.http.handshake_timeout_secs,
                d.http.handshake_timeout_secs
            );
            show_field!("disable_quic", s.http.disable_quic, d.http.disable_quic);
            show_field!(
                "disable_http1_1",
                s.http.disable_http1_1,
                d.http.disable_http1_1
            );
            println!();
            println!("[auth]");
            show_field!(
                "require_token_path",
                &s.auth.require_token_path,
                &d.auth.require_token_path
            );
            show_field!(
                "rate_limit_rps",
                s.auth.rate_limit_rps,
                d.auth.rate_limit_rps
            );
            show_field!(
                "rate_limit_burst",
                s.auth.rate_limit_burst,
                d.auth.rate_limit_burst
            );
            println!();
            println!("[instance]");
            show_field!("label", &s.instance.label, &d.instance.label);

            if let Some(p) = settings_path() {
                println!();
                if p.exists() {
                    println!("# Settings file: {}", p.display());
                } else {
                    println!("# Settings file not found: {}", p.display());
                    println!("# Run `ahma settings init` to create it.");
                }
            }

            Ok(())
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Prompts commands
// ─────────────────────────────────────────────────────────────────────────────

fn handle_prompts_init(force: bool, project: bool, path: Option<PathBuf>) -> Result<()> {
    use ahma_common::prompts::{AhmaPrompts, global_prompts_path};
    use std::fs;

    let target = if let Some(p) = path {
        p
    } else if project {
        PathBuf::from(".ahma").join("prompts.toml")
    } else {
        global_prompts_path()
            .ok_or_else(|| anyhow::anyhow!("Could not determine home directory for prompts.toml"))?
    };

    if target.exists() && !force {
        anyhow::bail!(
            "Prompts file already exists at {}. Use --force to overwrite.",
            target.display()
        );
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }

    let template = AhmaPrompts::generate_template();
    fs::write(&target, template)?;
    println!("Prompts file written to: {}", target.display());
    println!();
    println!("Edit the file to override LLM prompt templates.");
    Ok(())
}

fn load_prompts_from_path(path: &std::path::Path) -> Option<ahma_common::prompts::AhmaPrompts> {
    if !path.exists() {
        return None;
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|c| toml::from_str(&c).ok())
}

fn handle_prompts_show() -> Result<()> {
    use ahma_common::prompts::{AhmaPrompts, global_prompts_path};

    let s = AhmaPrompts::load();

    let global_p = global_prompts_path()
        .as_deref()
        .and_then(load_prompts_from_path)
        .unwrap_or_default();

    let local_path = PathBuf::from(".ahma").join("prompts.toml");
    let local_p = load_prompts_from_path(&local_path).unwrap_or_default();

    println!("# Effective Ahma LLM Prompts");
    println!(
        "# Sources: [project-local] = .ahma/prompts.toml  [global] = ~/.ahma/prompts.toml  [default] = compiled-in"
    );
    println!();

    let show_prompt_info = |name: &str, is_local_some: bool, is_global_some: bool| {
        let source = if is_local_some {
            "[project-local override]"
        } else if is_global_some {
            "[global override]"
        } else {
            "[compiled-in default]"
        };
        println!("## {} -- {}", name, source);
    };

    let local_tt = local_p.task_tree.as_ref();
    let global_tt = global_p.task_tree.as_ref();
    let local_dec = local_p.decompose.as_ref();
    let global_dec = global_p.decompose.as_ref();

    show_prompt_info(
        "task_tree.planning",
        local_tt.and_then(|t| t.planning.as_ref()).is_some(),
        global_tt.and_then(|t| t.planning.as_ref()).is_some(),
    );
    println!("{}", s.planning_prompt());
    println!();

    show_prompt_info(
        "task_tree.summarisation",
        local_tt.and_then(|t| t.summarisation.as_ref()).is_some(),
        global_tt.and_then(|t| t.summarisation.as_ref()).is_some(),
    );
    println!("{}", s.summarisation_prompt());
    println!();

    show_prompt_info(
        "task_tree.recovery",
        local_tt.and_then(|t| t.recovery.as_ref()).is_some(),
        global_tt.and_then(|t| t.recovery.as_ref()).is_some(),
    );
    println!("{}", s.recovery_prompt());
    println!();

    show_prompt_info(
        "decompose.split",
        local_dec.and_then(|d| d.split.as_ref()).is_some(),
        global_dec.and_then(|d| d.split.as_ref()).is_some(),
    );
    println!("{}", s.split_prompt());
    println!();

    Ok(())
}

fn handle_prompts_validate() -> Result<()> {
    use ahma_common::prompts::AhmaPrompts;

    let s = AhmaPrompts::load();
    let warnings = s.validate();
    if warnings.is_empty() {
        println!("✓ Prompts configuration is valid.");
    } else {
        println!("Warnings found in prompts configuration:");
        for w in warnings {
            println!("  - {}", w);
        }
    }
    Ok(())
}

fn handle_prompts_update() -> Result<()> {
    use ahma_common::prompts::{AhmaPrompts, global_prompts_path};
    use std::fs;

    let Some(path) = global_prompts_path() else {
        anyhow::bail!("Could not determine home directory for ~/.ahma/prompts.toml");
    };

    let template = AhmaPrompts::generate_template();
    if path.exists() {
        let backup_path = path.with_extension("toml.bak");
        if backup_path.exists() {
            let _ = fs::remove_file(&backup_path);
        }
        fs::rename(&path, &backup_path)?;
        println!(
            "✓ Backed up existing prompts file to {}",
            backup_path.display()
        );
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, template)?;
    println!("✓ Wrote latest prompt defaults to {}", path.display());
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Sandbox scope grants
// ─────────────────────────────────────────────────────────────────────────────

pub(crate) fn run_sandbox_command(args: SandboxArgs) -> Result<()> {
    use ahma_common::config::settings_path;

    let file = settings_path()
        .context("Cannot determine ~/.ahma/settings.toml (home directory not found)")?;

    match args.command {
        SandboxCommand::Grant {
            path: dir,
            read_only,
            by,
            note,
        } => run_sandbox_grant(&file, dir, read_only, by, note),
        SandboxCommand::List => run_sandbox_list(&file),
        SandboxCommand::Revoke { path: dir } => run_sandbox_revoke(&file, dir),
    }
}

/// Strict load on the write paths: refuse to clobber a settings file we cannot
/// parse. A missing file is fine (returns defaults) — the first grant creates it.
fn load_sandbox_settings(file: &std::path::Path) -> Result<ahma_common::config::AhmaSettings> {
    ahma_common::config::AhmaSettings::load_from_result(file).map_err(|e| anyhow::anyhow!(e))
}

fn run_sandbox_grant(
    file: &std::path::Path,
    dir: PathBuf,
    read_only: bool,
    by: Option<String>,
    note: Option<String>,
) -> Result<()> {
    use ahma_common::config::{GrantOutcome, PersistentScope, ScopeAccess};

    let access = if read_only {
        ScopeAccess::Ro
    } else {
        ScopeAccess::Rw
    };
    let mut settings = load_sandbox_settings(file)?;

    // The catastrophic-path denylist gates *every* write into the ledger,
    // not just the `sandbox_grant` MCP tool (SPEC R-PERM.2). The CLI used
    // to skip it, which meant the safest surface (a human at a terminal)
    // had the weakest guardrail — precisely backwards.
    refuse_denylisted_grant(&dir, &settings)?;

    let scope = PersistentScope {
        path: dir.clone(),
        access,
        granted_by: by,
        granted_at: Some(chrono::Local::now().format("%Y-%m-%d").to_string()),
        note,
    };
    let outcome = settings.sandbox.grant_scope(scope);
    settings
        .save_to(file)
        .with_context(|| format!("Failed to write {}", file.display()))?;

    audit(
        AuditAction::Grant,
        GrantKind::FsScope,
        dir.display().to_string(),
        Some(if access.is_write() { "rw" } else { "ro" }.to_string()),
    );

    match outcome {
        GrantOutcome::Added => {
            println!("✓ Granted {} access to {}", access.label(), dir.display());
        }
        GrantOutcome::Updated(old) => {
            println!(
                "✓ Updated grant for {}: {} → {}",
                dir.display(),
                old.access.label(),
                access.label()
            );
        }
    }
    println!();
    println!("Recorded in: {}", file.display());
    println!("  This file lives outside every sandbox scope, so a sandboxed *command*");
    println!("  cannot touch it. Only you — or the AI's `sandbox_grant` tool, and only");
    println!("  after you confirm a previewed line — can change it. Edit it by hand, or run");
    println!(
        "  `ahma sandbox revoke {}` to remove this grant.",
        dir.display()
    );
    println!();
    println!("Takes effect the next time an ahma server starts (restart your IDE's MCP");
    println!("connection, or `ahma serve …`, to apply it now).");
    Ok(())
}

fn run_sandbox_list(file: &std::path::Path) -> Result<()> {
    let settings = load_sandbox_settings(file)?;
    let scopes = &settings.sandbox.persistent_scopes;
    println!("# Persistent sandbox scopes");
    println!("# File: {}", file.display());
    println!();

    if scopes.is_empty() {
        println!("(none granted)");
        println!();
        println!(
            "Grant one with:  ahma sandbox grant <PATH> [--read-only] [--by WHO] [--note TEXT]"
        );
        return Ok(());
    }
    for s in scopes {
        println!("• {}  ({})", s.path.display(), s.access.label());
        if let Some(by) = &s.granted_by {
            println!("    granted by: {by}");
        }
        if let Some(at) = &s.granted_at {
            println!("    granted on: {at}");
        }
        if let Some(note) = &s.note {
            println!("    note:       {note}");
        }
    }
    println!();
    println!("These survive every roots/list update. Use `ahma sandbox grant|revoke` (or");
    println!("edit the file directly) to change them; restart the server to apply.");
    Ok(())
}

fn run_sandbox_revoke(file: &std::path::Path, dir: PathBuf) -> Result<()> {
    let mut settings = load_sandbox_settings(file)?;
    let Some(removed) = settings.sandbox.revoke_scope(&dir) else {
        println!("No persistent scope matching {} was found.", dir.display());
        println!("Run `ahma sandbox list` to see current grants.");
        return Ok(());
    };

    settings
        .save_to(file)
        .with_context(|| format!("Failed to write {}", file.display()))?;
    audit(
        AuditAction::Revoke,
        GrantKind::FsScope,
        removed.path.display().to_string(),
        Some(
            if removed.access.is_write() {
                "rw"
            } else {
                "ro"
            }
            .to_string(),
        ),
    );
    println!(
        "✓ Revoked {} access to {}",
        removed.access.label(),
        removed.path.display()
    );
    println!();
    println!("Updated: {}", file.display());
    println!("Takes effect the next time an ahma server starts.");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// The unified permission ledger (SPEC R-PERM)
// ─────────────────────────────────────────────────────────────────────────────

use ahma_common::permissions::{
    AuditAction, GrantKind, GrantTier, append_audit, audit_entry, records, workspace_key,
};

/// Record a ledger change in the audit trail. Never fails the operation it is
/// recording: the grant was already confirmed by a human and is safely written;
/// losing the *record* of it is the lesser harm.
fn audit(action: AuditAction, kind: GrantKind, subject: String, access: Option<String>) {
    append_audit(&audit_entry(
        chrono::Local::now().to_rfc3339(),
        action,
        kind,
        subject,
        access,
        GrantTier::Always,
        Some("cli".to_string()),
    ));
}

/// Refuse a filesystem grant that the hard denylist forbids (SPEC R5.4.5,
/// generalized to every write path by R-PERM.2).
///
/// The denylist is not advice, and no surface may skip it: not the MCP tool, not
/// an elicitation answer, and not the CLI. A grant of `$HOME`, a filesystem root,
/// a parent of the live scope, a credential directory, or an OS system directory
/// is refused outright — there is no `--force`, because every legitimate use of
/// such a grant is better served by naming the specific subdirectory.
fn refuse_denylisted_grant(
    path: &std::path::Path,
    settings: &ahma_common::config::AhmaSettings,
) -> Result<()> {
    use crate::mcp_service::handlers::sandbox_grant_tool::{GrantRisk, classify_grant_risk};

    let expanded = ahma_common::config::expand_home(path);
    let canonical = dunce::canonicalize(&expanded).unwrap_or(expanded);
    let home = ahma_common::config::ahma_home_dir();
    let live_scopes: Vec<PathBuf> = settings
        .sandbox
        .scopes
        .iter()
        .map(|p| ahma_common::config::expand_home(p))
        .collect();

    match classify_grant_risk(&canonical, home.as_deref(), &live_scopes) {
        GrantRisk::Refused(reason) => anyhow::bail!(
            "Refusing to grant {}:\n  {reason}\n\n\
             This is a hard limit, not a warning — there is no override flag. If a tool \
             genuinely needs something under that path, grant the specific subdirectory it \
             needs instead.",
            canonical.display()
        ),
        GrantRisk::High(warnings) => {
            eprintln!("⚠ Elevated-risk grant for {}:", canonical.display());
            for w in &warnings {
                eprintln!("    • {w}");
            }
            eprintln!();
            Ok(())
        }
        GrantRisk::Normal => Ok(()),
    }
}

pub(crate) fn run_permissions_command(args: PermissionsArgs) -> Result<()> {
    use ahma_common::config::{AhmaSettings, settings_path};

    let file = settings_path()
        .context("Cannot determine ~/.ahma/settings.toml (home directory not found)")?;
    // Fold any retired ~/.config/ahma/approvals.json into the ledger first, so
    // `list` shows the truth rather than a partial picture.
    if let Err(e) = ahma_common::permissions::migrate_legacy_approvals(&file) {
        eprintln!("warning: could not migrate legacy approvals: {e:#}");
    }
    let load = || -> Result<AhmaSettings> {
        AhmaSettings::load_from_result(&file).map_err(|e| anyhow::anyhow!(e))
    };

    match args.command {
        PermissionsCommand::List { kind } => {
            let settings = load()?;
            print_permissions(&settings, kind.as_deref(), &file)
        }
        PermissionsCommand::Revoke {
            kind,
            subject,
            workspace,
            yes,
        } => revoke_permission(&file, load()?, &kind, &subject, workspace, yes),
    }
}

/// Parse a kind filter/selector, naming the valid options on failure rather than
/// silently ignoring a typo.
fn parse_kind(s: &str) -> Result<GrantKind> {
    match s {
        "fs-scope" | "fs" | "scope" => Ok(GrantKind::FsScope),
        "web-domain" | "web" | "domain" => Ok(GrantKind::WebDomain),
        "tool" => Ok(GrantKind::Tool),
        other => anyhow::bail!(
            "unknown permission kind '{other}' (expected: fs-scope, web-domain, or tool)"
        ),
    }
}

fn print_permissions(
    settings: &ahma_common::config::AhmaSettings,
    kind_filter: Option<&str>,
    file: &std::path::Path,
) -> Result<()> {
    let filter = kind_filter.map(parse_kind).transpose()?;
    let all = records(settings);
    let rows: Vec<_> = all
        .iter()
        .filter(|r| filter.is_none_or(|k| r.kind == k))
        .collect();

    println!("# Permissions granted to ahma");
    println!("# File: {}", file.display());
    println!();

    if rows.is_empty() {
        println!("(none granted)");
        println!();
        println!("ahma asks for a permission when — and only when — the sandbox actually blocks");
        println!("something. Nothing here means nothing has needed one yet.");
        println!();
        print_profiles(settings);
        return Ok(());
    }

    for kind in [GrantKind::FsScope, GrantKind::WebDomain, GrantKind::Tool] {
        let of_kind: Vec<_> = rows.iter().filter(|r| r.kind == kind).collect();
        if of_kind.is_empty() {
            continue;
        }
        println!("{}:", kind.label());
        for r in of_kind {
            print_permission_record(r);
        }
        println!();
    }

    print_profiles(settings);

    println!("This file lives outside every sandbox scope, so a sandboxed command can neither");
    println!(
        "read it nor add itself to it. Revoke with `ahma permissions revoke <KIND> <SUBJECT>`."
    );
    Ok(())
}

/// Print a single permission record's subject line plus its indented detail
/// lines (workspace, provenance, note) — the body of the per-kind loop in
/// [`print_permissions`], pulled out so that loop reads as "for each record,
/// print it" rather than a wall of formatting.
fn print_permission_record(r: &ahma_common::permissions::GrantRecord) {
    let access = r
        .access
        .as_deref()
        .map(|a| format!("  ({a})"))
        .unwrap_or_default();
    println!("  • {}{access}", r.subject);
    if let Some(ws) = &r.scope_note {
        println!("      workspace:  {ws}");
    }
    let provenance = [
        r.granted_by.as_deref().map(|b| format!("by {b}")),
        r.granted_at.as_deref().map(|a| format!("on {a}")),
        r.surface.as_deref().map(|s| format!("via {s}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(", ");
    if !provenance.is_empty() {
        println!("      granted:    {provenance}");
    }
    if let Some(note) = &r.note {
        println!("      note:       {note}");
    }
}

/// Show the sandbox profiles in effect — the toolchain carve-outs ahma applies on
/// the user's behalf (SPEC R-PERM.5).
///
/// These used to be hard-coded in the sandbox backends, which meant nobody could
/// see them. Listing them here is the whole point of making them data: a grant you
/// cannot see is a grant you cannot evaluate, and one you cannot refuse.
fn print_profiles(settings: &ahma_common::config::AhmaSettings) {
    use crate::sandbox::profiles::{ProfileAccess, builtin_profiles, resolved_rules};

    let enabled = &settings.sandbox.profiles;
    println!("sandbox profiles (built-in toolchain carve-outs):");
    if enabled.is_empty() {
        println!("  (none — every toolchain path must be granted explicitly)");
        println!();
        return;
    }

    let rules = resolved_rules(enabled, settings.sandbox.package_cache_write);
    for profile in builtin_profiles() {
        if !enabled.iter().any(|n| n == &profile.name) {
            continue;
        }
        println!("  • {} — {}", profile.name, profile.description);
        for r in rules.iter().filter(|r| r.profile == profile.name) {
            let access = match r.access {
                ProfileAccess::Ro => "read",
                ProfileAccess::Rx => "read+execute",
                ProfileAccess::Rw => "read+write",
            };
            println!("      {}  ({access})", r.path.display());
        }
    }
    println!();
    println!("  Disable any of these with `[sandbox] profiles` in the settings file.");
    println!();

    if let Some(note) = crate::sandbox::profiles::macos_read_disclosure() {
        println!("  ⚠ {note}");
        println!();
    }
}

/// The deferred half of a revoke: applies the change and reports whether it did
/// anything. Built *after* the preview so the same closure both describes and
/// performs the edit — the preview cannot drift from what `--yes` actually does.
type RevokeFn = Box<dyn FnOnce(&mut ahma_common::config::AhmaSettings) -> bool>;

/// Preview an fs-scope revoke: `None` means "not found" (message already
/// printed to the user); `Some` carries the description and the deferred edit.
fn preview_revoke_fs_scope(
    settings: &ahma_common::config::AhmaSettings,
    subject: &str,
) -> Option<(String, RevokeFn)> {
    let path = PathBuf::from(subject);
    if settings.sandbox.find_scope(&path).is_none() {
        println!("No filesystem scope matching {subject} is granted.");
        println!("Run `ahma permissions list` to see what is.");
        return None;
    }
    Some((
        format!("remove the filesystem scope {subject}"),
        Box::new(move |s: &mut ahma_common::config::AhmaSettings| {
            s.sandbox.revoke_scope(&path).is_some()
        }),
    ))
}

/// Preview a web-domain revoke; see [`preview_revoke_fs_scope`] for the `None` contract.
fn preview_revoke_web_domain(
    settings: &ahma_common::config::AhmaSettings,
    subject: &str,
) -> Option<(String, RevokeFn)> {
    let pattern = subject.to_string();
    if !settings.web.always_allow.iter().any(|p| p == &pattern) {
        println!("No web domain matching {subject} is in the allow list.");
        println!("Run `ahma permissions list` to see what is.");
        return None;
    }
    Some((
        format!("remove the web domain {subject} from [web].always_allow"),
        Box::new(move |s: &mut ahma_common::config::AhmaSettings| {
            let before = s.web.always_allow.len();
            s.web.always_allow.retain(|p| p != &pattern);
            s.web.always_allow.len() != before
        }),
    ))
}

/// Preview a tool-approval revoke; see [`preview_revoke_fs_scope`] for the `None` contract.
fn preview_revoke_tool(
    settings: &ahma_common::config::AhmaSettings,
    subject: &str,
    workspace: Option<PathBuf>,
) -> Option<(String, RevokeFn)> {
    let ws = workspace
        .map(|w| workspace_key(&w))
        .unwrap_or_else(|| workspace_key(&std::env::current_dir().unwrap_or_default()));
    if !settings.permissions.is_tool_approved(&ws, subject) {
        println!("Tool '{subject}' is not approved in {}.", ws.display());
        println!(
            "Approvals are per-workspace; pass --workspace to target another one, or \
                 run `ahma permissions list`."
        );
        return None;
    }
    let tool = subject.to_string();
    Some((
        format!(
            "remove the tool approval '{subject}' for workspace {}",
            ws.display()
        ),
        Box::new(move |s: &mut ahma_common::config::AhmaSettings| {
            s.permissions.revoke_tool(&ws, &tool)
        }),
    ))
}

fn revoke_permission(
    file: &std::path::Path,
    mut settings: ahma_common::config::AhmaSettings,
    kind: &str,
    subject: &str,
    workspace: Option<PathBuf>,
    yes: bool,
) -> Result<()> {
    let kind = parse_kind(kind)?;

    // Preview first, always. A revoke is less dangerous than a grant, but the
    // user should still never be surprised by what a command wrote (R-PERM.2).
    let preview = match kind {
        GrantKind::FsScope => preview_revoke_fs_scope(&settings, subject),
        GrantKind::WebDomain => preview_revoke_web_domain(&settings, subject),
        GrantKind::Tool => preview_revoke_tool(&settings, subject, workspace),
        GrantKind::HookUnsandboxed => anyhow::bail!(
            "hook consent is session-scoped and never persisted; revoke it with \
                 `ahma hooks revoke`"
        ),
    };
    let Some((description, apply)) = preview else {
        return Ok(());
    };

    if !yes {
        println!("Would {description}.");
        println!();
        println!("File: {}", file.display());
        println!();
        println!("Nothing was changed. Re-run with --yes to apply.");
        return Ok(());
    }

    if apply(&mut settings) {
        settings
            .save_to(file)
            .with_context(|| format!("Failed to write {}", file.display()))?;
        // Audit the *expanded* subject, so a path's grant and its revoke carry the
        // same string. An audit log where `~/cache` and `/home/me/cache` are two
        // different entries cannot answer "what happened to this path?" — which is
        // the only question anyone opens it to ask.
        let audited = match kind {
            GrantKind::FsScope => ahma_common::config::expand_home(std::path::Path::new(subject))
                .display()
                .to_string(),
            _ => subject.to_string(),
        };
        audit(AuditAction::Revoke, kind, audited, None);
        println!("✓ Revoked: {description}");
        println!();
        println!("Updated: {}", file.display());
        if kind == GrantKind::FsScope {
            println!("Takes effect the next time an ahma server starts.");
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Web-egress policy (SPEC R-WEB.10)
// ─────────────────────────────────────────────────────────────────────────────

pub(crate) fn run_web_command(args: WebArgs) -> Result<()> {
    use ahma_common::config::settings_path;
    let file = settings_path()
        .context("Cannot determine ~/.ahma/settings.toml (home directory not found)")?;
    web_command_at(&file, args.command)
}

/// Which list a pattern lives in.
enum WebList {
    Allow,
    Deny,
}

/// Core of `ahma web`, operating on an explicit settings file so it is unit
/// testable without touching the real home directory.
fn web_command_at(file: &std::path::Path, command: WebCommand) -> Result<()> {
    use ahma_common::config::AhmaSettings;
    use ahma_common::web_policy::{WebPolicy, url_coordinates};

    // Strict load on write paths: never clobber an unparseable settings file.
    let load = || -> Result<AhmaSettings> {
        AhmaSettings::load_from_result(file).map_err(|e| anyhow::anyhow!(e))
    };

    match command {
        WebCommand::Allow { pattern } => web_add(file, &load()?, pattern, WebList::Allow),
        WebCommand::Deny { pattern } => web_add(file, &load()?, pattern, WebList::Deny),
        WebCommand::Revoke { pattern } => {
            let mut settings = load()?;
            let before = settings.web.always_allow.len() + settings.web.never_allow.len();
            settings.web.always_allow.retain(|p| p != &pattern);
            settings.web.never_allow.retain(|p| p != &pattern);
            let removed =
                before - (settings.web.always_allow.len() + settings.web.never_allow.len());
            if removed == 0 {
                println!("No `{pattern}` entry found in always_allow or never_allow.");
                println!("Run `ahma web list` to see current entries.");
                return Ok(());
            }
            settings
                .save_to(file)
                .with_context(|| format!("Failed to write {}", file.display()))?;
            println!("✓ Removed `{pattern}` from the web policy");
            println!();
            println!("Updated: {}", file.display());
            Ok(())
        }
        WebCommand::List => {
            let settings = load()?;
            let (_, errors) = WebPolicy::from_settings(&settings.web);
            println!("# Web-egress policy");
            println!("# File: {}", file.display());
            println!();
            println!("default_policy       = {:?}", settings.web.default_policy);
            println!(
                "block_private_ranges = {}",
                settings.web.block_private_ranges
            );
            println!(
                "on_redirect          = {:?}",
                settings.web.on_redirect_to_new_domain
            );
            println!();
            print_web_list("always_allow (permitted)", &settings.web.always_allow);
            print_web_list("never_allow (blocked)", &settings.web.never_allow);
            for e in &errors {
                println!("⚠ invalid pattern ignored: {e}");
            }
            println!();
            println!("Manage with: ahma web allow|deny|revoke <PATTERN>, or ahma web check <URL>.");
            Ok(())
        }
        WebCommand::Check { url } => {
            let settings = load()?;
            let (policy, _) = WebPolicy::from_settings(&settings.web);
            let domain =
                url_coordinates(&url).map_or_else(|| "(unparseable)".to_string(), |(_, h, _)| h);
            let decision = policy.decide(&url, &[], &[]);
            println!("URL:      {url}");
            println!("Domain:   {domain}");
            println!("Decision: {}", format_decision(&decision));
            println!();
            println!(
                "Note: the SSRF/private-range guard also applies at connect time \
                 (block_private_ranges = {}).",
                settings.web.block_private_ranges
            );
            Ok(())
        }
    }
}

fn web_add(
    file: &std::path::Path,
    loaded: &ahma_common::config::AhmaSettings,
    pattern: String,
    list: WebList,
) -> Result<()> {
    use ahma_common::web_policy::WebPattern;
    // R-WEB.10.1: reject invalid patterns (bare `*`, TLD wildcard, IP/localhost).
    let parsed = WebPattern::parse(&pattern)
        .map_err(|e| anyhow::anyhow!("invalid pattern '{pattern}': {e}"))?;
    if parsed.is_cleartext() {
        eprintln!("⚠ '{pattern}' uses cleartext http:// — traffic is unencrypted.");
    }

    let mut settings = loaded.clone();
    let (target, other, label, other_label) = match list {
        WebList::Allow => (
            &mut settings.web.always_allow,
            &settings.web.never_allow,
            "always_allow",
            "never_allow",
        ),
        WebList::Deny => (
            &mut settings.web.never_allow,
            &settings.web.always_allow,
            "never_allow",
            "always_allow",
        ),
    };

    if target.iter().any(|p| p == &pattern) {
        println!("`{pattern}` is already in {label}; nothing to do.");
        return Ok(());
    }
    let conflicts = other.contains(&pattern);
    target.push(pattern.clone());
    settings
        .save_to(file)
        .with_context(|| format!("Failed to write {}", file.display()))?;

    println!("✓ Added `{pattern}` to [web].{label}");
    if conflicts {
        println!(
            "⚠ `{pattern}` is also in {other_label}; note never_allow always wins over always_allow."
        );
    }
    println!();
    println!("Recorded in: {}", file.display());
    println!("  This file lives outside every sandbox scope, so a sandboxed tool cannot");
    println!("  edit it. Takes effect immediately for new fetches (the policy is reloaded");
    println!("  per request).");
    Ok(())
}

fn print_web_list(title: &str, entries: &[String]) {
    println!("{title}:");
    if entries.is_empty() {
        println!("  (none)");
    } else {
        for e in entries {
            println!("  • {e}");
        }
    }
    println!();
}

fn format_decision(decision: &ahma_common::web_policy::WebDecision) -> String {
    use ahma_common::web_policy::WebDecision;
    match decision {
        WebDecision::Allow { matched } => format!("ALLOW (matched: {matched})"),
        WebDecision::Deny { reason } => format!("DENY ({reason})"),
        WebDecision::Prompt { domain } => {
            format!("PROMPT — '{domain}' would require approval (default_policy = deny)")
        }
    }
}

pub(crate) fn run_logs_command(args: LogsArgs) -> Result<()> {
    match args.command {
        LogsCommand::Gitignore => {
            let log_dir = crate::utils::logging::project_log_dir();
            match crate::utils::logging::ensure_gitignore_entry() {
                Ok(true) => {
                    println!(
                        "✓ Added an ignore rule for {} to .gitignore",
                        log_dir.display()
                    );
                    Ok(())
                }
                Ok(false) => {
                    println!(
                        "{} is already covered by .gitignore — nothing to do.",
                        log_dir.display()
                    );
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
    }
}

pub(crate) fn run_prompts_command(args: PromptsArgs) -> Result<()> {
    match args.command {
        PromptsCommand::Init {
            force,
            project,
            path,
        } => handle_prompts_init(force, project, path),
        PromptsCommand::Show => handle_prompts_show(),
        PromptsCommand::Validate => handle_prompts_validate(),
        PromptsCommand::Update => handle_prompts_update(),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Bundle commands
// ─────────────────────────────────────────────────────────────────────────────

fn audit_bundle_command(audit_args: BundleAuditArgs) -> Result<()> {
    println!("Auditing bundle: {}", audit_args.path.display());
    let result =
        crate::bundle::signing::audit_bundle(&audit_args.path).context("Bundle audit failed")?;

    println!(
        "Files checked: {} | Findings: {}",
        result.files_checked,
        result.findings.len()
    );

    for finding in &result.findings {
        let sev = match finding.severity {
            crate::bundle::BundleAuditSeverity::Info => "INFO    ",
            crate::bundle::BundleAuditSeverity::Warning => "WARNING ",
            crate::bundle::BundleAuditSeverity::Critical => "CRITICAL",
        };
        println!("[{sev}] {}: {}", finding.file, finding.description);
        println!("         Recommendation: {}", finding.recommendation);
    }

    if result.passed {
        println!("PASS Bundle audit passed.");
        return Ok(());
    }
    if audit_args.strict && !result.findings.is_empty() {
        anyhow::bail!("Bundle audit found issues (--strict mode).")
    }
    anyhow::bail!("Bundle audit found critical issues.")
}

fn verify_bundle_command(verify_args: BundleVerifyArgs) -> Result<()> {
    println!("Verifying bundle: {}", verify_args.path.display());
    let verifier = crate::bundle::BundleVerifier::new(
        dirs::home_dir()
            .unwrap_or_default()
            .join(".ahma")
            .join("keys")
            .join("trusted"),
    );
    match verifier.verify(&verify_args.path)? {
        true => {
            println!("PASS Bundle verification passed.");
            Ok(())
        }
        false => anyhow::bail!("FAIL Bundle verification failed."),
    }
}

fn sign_bundle_command(sign_args: BundleSignArgs) -> Result<()> {
    println!("Signing bundle: {}", sign_args.path.display());
    let digests =
        crate::bundle::BundleSigner::sign(&sign_args.path).context("Bundle signing failed")?;
    println!("Manifest written with {} file hashes.", digests.len());
    Ok(())
}

pub(crate) fn dispatch_bundle_command(args: BundleArgs) -> Result<()> {
    match args.command {
        BundleCommand::Audit(audit_args) => audit_bundle_command(audit_args),
        BundleCommand::Verify(verify_args) => verify_bundle_command(verify_args),
        BundleCommand::Sign(sign_args) => sign_bundle_command(sign_args),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Validation command
// ─────────────────────────────────────────────────────────────────────────────

pub(crate) fn run_validation_mode(target: &str) -> Result<()> {
    let result = crate::validation::run_validation(target)?;
    if result.all_valid {
        println!(
            "All configurations are valid ({} file(s) checked).",
            result.files_checked
        );
        return Ok(());
    }

    // Print the full per-failure detail to stdout so the user sees *what* is
    // wrong and *where* — not just a count. (The same detail is also logged.)
    for failure in &result.failures {
        println!("\n✗ {}\n{}", failure.path, failure.detail);
    }

    // Summarize with an honest denominator. `files_checked` is the number of
    // readable JSON files; missing targets are reported separately so we never
    // print a nonsensical "1/0 files invalid".
    let missing = result.missing_targets.len();
    let schema_failures = result.files_failed.saturating_sub(missing);
    let summary = match (schema_failures, missing) {
        (s, 0) => format!("{s}/{} file(s) failed validation", result.files_checked),
        (0, m) => format!("{m} target(s) not found"),
        (s, m) => format!(
            "{s}/{} file(s) failed validation and {m} target(s) not found",
            result.files_checked
        ),
    };
    anyhow::bail!("Validation failed: {summary}.")
}

// ─────────────────────────────────────────────────────────────────────────────
// Tool info command
// ─────────────────────────────────────────────────────────────────────────────

pub(crate) async fn run_tool_info_mode(args: InfoArgs) -> Result<()> {
    use crate::config;

    // Build a minimal AppConfig with the requested bundles + tools_dir.
    // AHMA_TOOLS_DIR is retired (R-CFG1.2); only use the CLI arg.
    let tools_dir = resolution::normalize_tools_dir(args.tools_dir);

    let mini_cfg = AppConfig {
        tool_bundles: args.tool_bundles,
        tools_dir: tools_dir.clone(),
        ..AppConfig::default()
    };

    let configs = config::load_tool_configs(&mini_cfg, tools_dir.as_deref()).await?;

    // Optionally filter to a single tool
    let mut tools: Vec<(&String, &config::ToolConfig)> = configs.iter().collect();
    if let Some(ref filter) = args.filter {
        tools.retain(|(name, _)| name.as_str() == filter.as_str());
        if tools.is_empty() {
            anyhow::bail!(
                "Tool '{}' not found. Run without a filter to see all available tools.",
                filter
            );
        }
    }
    tools.sort_by_key(|(name, _)| (*name).clone());

    match args.format {
        list_tools::OutputFormat::Text => print_tool_info_text(&tools),
        list_tools::OutputFormat::Json => print_tool_info_json(&tools)?,
    }

    Ok(())
}

fn print_command_arg_flag(opt: &crate::config::CommandOption) {
    let req = if opt.required.unwrap_or(false) {
        "required"
    } else {
        "optional"
    };
    print!("        --{} ({}, {})", opt.name, opt.option_type, req);
    if let Some(ref desc) = opt.description {
        print!(": {}", desc);
    }
    println!();
}

fn print_command_arg_positional(arg: &crate::config::CommandOption) {
    let req = if arg.required.unwrap_or(false) {
        "required"
    } else {
        "optional"
    };
    print!("        <{}> ({}, {})", arg.name, arg.option_type, req);
    if let Some(ref desc) = arg.description {
        print!(": {}", desc);
    }
    println!();
}

fn print_optional_command_args(
    args: Option<&[crate::config::CommandOption]>,
    printer: fn(&crate::config::CommandOption),
) {
    if let Some(args) = args {
        for arg in args {
            printer(arg);
        }
    }
}

fn print_subcommand(sub: &crate::config::SubcommandConfig) {
    let status = if sub.enabled { "" } else { " (disabled)" };
    println!("    - {}{}: {}", sub.name, status, sub.description);
    print_optional_command_args(sub.options.as_deref(), print_command_arg_flag);
    print_optional_command_args(sub.positional_args.as_deref(), print_command_arg_positional);
}

fn print_subcommands(subs: &[crate::config::SubcommandConfig]) {
    println!("  Subcommands:");
    for sub in subs {
        print_subcommand(sub);
    }
}

fn print_hint_line(label: &str, value: &str) {
    println!("    {}: {}", label, value);
}

fn print_hints(h: &crate::config::ToolHints) {
    let standard_hints = [
        ("build", h.build.as_deref()),
        ("test", h.test.as_deref()),
        ("dependencies", h.dependencies.as_deref()),
        ("clean", h.clean.as_deref()),
        ("run", h.run.as_deref()),
    ];
    let custom_hints = h.custom.as_ref().filter(|custom| !custom.is_empty());

    if !standard_hints.iter().any(|(_, value)| value.is_some()) && custom_hints.is_none() {
        return;
    }

    println!("  Hints:");

    for (label, value) in standard_hints {
        if let Some(value) = value {
            print_hint_line(label, value);
        }
    }

    if let Some(custom) = custom_hints {
        for (k, v) in custom {
            print_hint_line(k, v);
        }
    }
}

fn print_availability_check(ac: &crate::config::AvailabilityCheck) {
    print!("  Availability check:");
    if let Some(ref cmd) = ac.command {
        print!(" {}", cmd);
    }
    if !ac.args.is_empty() {
        print!(" {}", ac.args.join(" "));
    }
    println!();
}

fn print_tool_info_header(total_tools: usize) {
    println!("Local Tool Configurations");
    println!("=========================");
    println!();
    println!("Total tools: {}", total_tools);
    println!();
}

#[allow(deprecated)]
fn print_tool_info_entry(name: &str, config: &crate::config::ToolConfig) {
    println!("Tool: {}", name);
    println!("  Description: {}", config.description);
    println!("  Command:     {}", config.command);
    println!("  Enabled:     {}", config.enabled);
    if let Some(timeout) = config.timeout_seconds {
        println!("  Timeout:     {}s", timeout);
    }
    if let Some(sync) = config.synchronous {
        println!("  Synchronous: {}", sync);
    }
    if let Some(ref subs) = config.subcommand {
        print_subcommands(subs);
    }
    print_hints(&config.hints);
    if let Some(ref ac) = config.availability_check {
        print_availability_check(ac);
    }
    if let Some(ref inst) = config.install_instructions {
        println!("  Install: {}", inst);
    }
    println!();
}

fn print_tool_info_text(tools: &[(&String, &crate::config::ToolConfig)]) {
    print_tool_info_header(tools.len());

    for (name, config) in tools {
        print_tool_info_entry(name, config);
    }
}

#[allow(deprecated)]
fn print_tool_info_json(tools: &[(&String, &crate::config::ToolConfig)]) -> Result<()> {
    let output: Vec<_> = tools
        .iter()
        .map(|(name, config)| {
            serde_json::json!({
                "name": name,
                "description": config.description,
                "command": config.command,
                "enabled": config.enabled,
                "timeout_seconds": config.timeout_seconds,
                "synchronous": config.synchronous,
                "subcommands": config.subcommand.as_ref().map(|subs| {
                    subs.iter().map(|s| {
                        serde_json::json!({
                            "name": s.name,
                            "description": s.description,
                            "enabled": s.enabled,
                            "options": s.options.as_ref().map(|opts| {
                                opts.iter().map(|o| serde_json::json!({
                                    "name": o.name,
                                    "type": o.option_type,
                                    "required": o.required.unwrap_or(false),
                                    "description": o.description,
                                })).collect::<Vec<_>>()
                            }),
                            "positional_args": s.positional_args.as_ref().map(|args| {
                                args.iter().map(|a| serde_json::json!({
                                    "name": a.name,
                                    "type": a.option_type,
                                    "required": a.required.unwrap_or(false),
                                    "description": a.description,
                                })).collect::<Vec<_>>()
                            }),
                        })
                    }).collect::<Vec<_>>()
                }),
                "install_instructions": config.install_instructions,
            })
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Stdio check
// ─────────────────────────────────────────────────────────────────────────────

pub(crate) fn check_stdio_not_interactive() -> Result<()> {
    use std::io::IsTerminal;

    if !std::io::stdin().is_terminal() {
        return Ok(());
    }

    eprintln!(
        "\nFAIL Error: ahma_mcp is an MCP server designed for JSON-RPC communication over stdio.\n"
    );
    eprintln!("It cannot be run directly from an interactive terminal.\n");
    eprintln!("Usage options:");
    eprintln!("  1. Run as stdio MCP server (requires MCP client):");
    eprintln!("     ahma serve stdio\n");
    eprintln!("  2. Run as HTTP bridge server:");
    eprintln!("     ahma serve http --port 3000\n");
    eprintln!("  3. Execute a single tool command:");
    eprintln!("     ahma tool run <tool_name> [-- tool_arguments...]\n");
    eprintln!("For more information, run: ahma --help\n");
    std::process::exit(1);
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::list_tools::OutputFormat;
    use std::sync::{LazyLock, Mutex, MutexGuard};
    use tempfile::TempDir;

    // Serialise every test that mutates process-global env (AHMA_TEST_HOME) or
    // the current working directory. `unsafe { std::env::set_var }` requires no
    // concurrent readers; nextest also isolates each test binary in its own
    // process.
    static ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// RAII guard: overrides the home directory (so `ahma_common::config::ahma_home_dir()`
    /// → `tempdir`) for the duration of a test and restores the previous value on
    /// drop. Uses the `AHMA_TEST_HOME` override rather than `HOME` because
    /// `dirs::home_dir()` ignores `HOME`/`USERPROFILE` on Windows — see
    /// [`ahma_common::config::ahma_home_dir`].
    struct HomeGuard<'a> {
        _lock: MutexGuard<'a, ()>,
        prev: Option<std::ffi::OsString>,
    }

    impl<'a> HomeGuard<'a> {
        fn new(home: &std::path::Path) -> Self {
            let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var_os("AHMA_TEST_HOME");
            // SAFETY: guarded by ENV_MUTEX; the only env mutators in this module
            // hold the same lock, and nextest isolates each binary in a process.
            unsafe {
                std::env::set_var("AHMA_TEST_HOME", home);
            }
            Self { _lock: lock, prev }
        }
    }

    impl Drop for HomeGuard<'_> {
        fn drop(&mut self) {
            // SAFETY: still holding ENV_MUTEX for the lifetime of this guard.
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("AHMA_TEST_HOME", v),
                    None => std::env::remove_var("AHMA_TEST_HOME"),
                }
            }
        }
    }

    /// A rich, valid MTDF tool definition exercising every `print_*` branch:
    /// timeout, synchronous, enabled+disabled subcommands, flag options (with and
    /// without description/required), positional args, standard + custom hints,
    /// an availability check, and install instructions.
    const RICH_TOOL_JSON: &str = r#"{
        "name": "mytool",
        "description": "A rich test tool",
        "command": "echo",
        "enabled": true,
        "timeout_seconds": 42,
        "synchronous": false,
        "subcommand": [
            {
                "name": "build",
                "description": "build subcommand",
                "enabled": true,
                "options": [
                    {"name": "release", "type": "boolean", "description": "release mode", "required": true},
                    {"name": "jobs", "type": "string"}
                ],
                "positional_args": [
                    {"name": "target", "type": "string", "description": "the target", "required": true},
                    {"name": "extra", "type": "string"}
                ]
            },
            {
                "name": "legacy",
                "description": "disabled sub",
                "enabled": false
            }
        ],
        "hints": {
            "build": "cargo build",
            "test": "cargo test",
            "custom": {"usage": "use it well"}
        },
        "availability_check": {"command": "echo", "args": ["--version"]},
        "install_instructions": "brew install mytool"
    }"#;

    /// A minimal tool exercising the negative branches (no timeout/sync/subs/hints/
    /// availability/install).
    const PLAIN_TOOL_JSON: &str = r#"{
        "name": "plaintool",
        "description": "A plain test tool",
        "command": "true"
    }"#;

    // ── settings ────────────────────────────────────────────────────────────

    #[test]
    fn settings_init_writes_file_to_explicit_path() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("settings.toml");
        let args = SettingsArgs {
            command: SettingsCommand::Init {
                force: false,
                path: Some(target.clone()),
            },
        };
        run_settings_command(args).unwrap();
        assert!(target.exists(), "settings file should be written");
        let body = std::fs::read_to_string(&target).unwrap();
        assert!(!body.is_empty());
    }

    #[test]
    fn settings_init_refuses_existing_without_force() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("settings.toml");
        std::fs::write(&target, "pre-existing").unwrap();
        let args = SettingsArgs {
            command: SettingsCommand::Init {
                force: false,
                path: Some(target.clone()),
            },
        };
        let err = run_settings_command(args).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        // Original content preserved.
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "pre-existing");
    }

    #[test]
    fn settings_init_force_overwrites_existing() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("settings.toml");
        std::fs::write(&target, "old").unwrap();
        let args = SettingsArgs {
            command: SettingsCommand::Init {
                force: true,
                path: Some(target.clone()),
            },
        };
        run_settings_command(args).unwrap();
        let body = std::fs::read_to_string(&target).unwrap();
        assert_ne!(body, "old");
        assert!(body.contains("settings.toml"));
    }

    #[test]
    fn settings_init_defaults_to_home_when_no_path() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::new(tmp.path());
        let args = SettingsArgs {
            command: SettingsCommand::Init {
                force: false,
                path: None,
            },
        };
        run_settings_command(args).unwrap();
        assert!(tmp.path().join(".ahma").join("settings.toml").exists());
    }

    #[test]
    fn settings_show_reports_default_and_file_sources() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::new(tmp.path());

        // First with no settings file → "[default]" sources + "not found" footer.
        run_settings_command(SettingsArgs {
            command: SettingsCommand::Show,
        })
        .unwrap();

        // Now write a settings file with a non-default value so the "[file]"
        // branch of the show_field! macro is exercised, plus the "exists" footer.
        let dir = tmp.path().join(".ahma");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("settings.toml"), "[tools]\ntimeout_secs = 999\n").unwrap();
        run_settings_command(SettingsArgs {
            command: SettingsCommand::Show,
        })
        .unwrap();
    }

    // ── prompts ─────────────────────────────────────────────────────────────

    #[test]
    fn prompts_init_writes_explicit_path_and_refuses_existing() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("sub").join("prompts.toml");
        run_prompts_command(PromptsArgs {
            command: PromptsCommand::Init {
                force: false,
                project: false,
                path: Some(target.clone()),
            },
        })
        .unwrap();
        assert!(target.exists());

        // Second call without --force must fail.
        let err = run_prompts_command(PromptsArgs {
            command: PromptsCommand::Init {
                force: false,
                project: false,
                path: Some(target.clone()),
            },
        })
        .unwrap_err();
        assert!(err.to_string().contains("already exists"));

        // With --force it overwrites.
        run_prompts_command(PromptsArgs {
            command: PromptsCommand::Init {
                force: true,
                project: false,
                path: Some(target.clone()),
            },
        })
        .unwrap();
    }

    #[test]
    fn prompts_init_global_writes_to_home() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::new(tmp.path());
        run_prompts_command(PromptsArgs {
            command: PromptsCommand::Init {
                force: false,
                project: false,
                path: None,
            },
        })
        .unwrap();
        assert!(tmp.path().join(".ahma").join("prompts.toml").exists());
    }

    #[test]
    fn prompts_init_project_writes_relative_dir() {
        let tmp = TempDir::new().unwrap();
        // Serialise on the env mutex because set_current_dir is process-global.
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prev_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let result = run_prompts_command(PromptsArgs {
            command: PromptsCommand::Init {
                force: false,
                project: true,
                path: None,
            },
        });
        std::env::set_current_dir(&prev_cwd).unwrap();
        result.unwrap();
        assert!(tmp.path().join(".ahma").join("prompts.toml").exists());
    }

    #[test]
    fn prompts_show_loads_local_and_global_overrides() {
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        // Global prompts file (valid override) under the test home.
        let prev_home = std::env::var_os("AHMA_TEST_HOME");
        // SAFETY: guarded by ENV_MUTEX.
        unsafe {
            std::env::set_var("AHMA_TEST_HOME", home.path());
        }
        let global_dir = home.path().join(".ahma");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::write(
            global_dir.join("prompts.toml"),
            "[task_tree]\nplanning = \"global planning\"\n",
        )
        .unwrap();

        // Local project override under CWD.
        let prev_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(cwd.path()).unwrap();
        let local_dir = cwd.path().join(".ahma");
        std::fs::create_dir_all(&local_dir).unwrap();
        std::fs::write(
            local_dir.join("prompts.toml"),
            "[task_tree]\nsummarisation = \"local summarise\"\n",
        )
        .unwrap();

        let result = run_prompts_command(PromptsArgs {
            command: PromptsCommand::Show,
        });

        std::env::set_current_dir(&prev_cwd).unwrap();
        // SAFETY: guarded by ENV_MUTEX.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("AHMA_TEST_HOME", v),
                None => std::env::remove_var("AHMA_TEST_HOME"),
            }
        }
        result.unwrap();
    }

    #[test]
    fn prompts_validate_returns_ok() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::new(tmp.path());
        run_prompts_command(PromptsArgs {
            command: PromptsCommand::Validate,
        })
        .unwrap();
    }

    #[test]
    fn prompts_update_writes_and_backs_up() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::new(tmp.path());
        let path = tmp.path().join(".ahma").join("prompts.toml");

        // First run: no existing file → no backup branch.
        run_prompts_command(PromptsArgs {
            command: PromptsCommand::Update,
        })
        .unwrap();
        assert!(path.exists());

        // Second run: existing file → backup created.
        run_prompts_command(PromptsArgs {
            command: PromptsCommand::Update,
        })
        .unwrap();
        let backup = path.with_extension("toml.bak");
        assert!(backup.exists());

        // Third run: existing .bak is removed first, then re-created.
        run_prompts_command(PromptsArgs {
            command: PromptsCommand::Update,
        })
        .unwrap();
        assert!(backup.exists());
    }

    // ── sandbox ─────────────────────────────────────────────────────────────

    #[test]
    fn sandbox_grant_list_revoke_lifecycle() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::new(tmp.path());
        let settings_file = tmp.path().join(".ahma").join("settings.toml");
        let scope_dir = tmp.path().join("granted-scope");

        // List with no grants → "(none granted)" early return.
        run_sandbox_command(SandboxArgs {
            command: SandboxCommand::List,
        })
        .unwrap();

        // Grant (read+write) a new scope → GrantOutcome::Added.
        run_sandbox_command(SandboxArgs {
            command: SandboxCommand::Grant {
                path: scope_dir.clone(),
                read_only: false,
                by: Some("tester".into()),
                note: Some("a note".into()),
            },
        })
        .unwrap();
        assert!(settings_file.exists());
        let body = std::fs::read_to_string(&settings_file).unwrap();
        assert!(body.contains("granted-scope"));

        // Grant the same path read-only → GrantOutcome::Updated.
        run_sandbox_command(SandboxArgs {
            command: SandboxCommand::Grant {
                path: scope_dir.clone(),
                read_only: true,
                by: None,
                note: None,
            },
        })
        .unwrap();

        // List now prints the grant (path/by/at/note loop).
        run_sandbox_command(SandboxArgs {
            command: SandboxCommand::List,
        })
        .unwrap();

        // Revoke the existing scope → Some(removed) branch.
        run_sandbox_command(SandboxArgs {
            command: SandboxCommand::Revoke {
                path: scope_dir.clone(),
            },
        })
        .unwrap();

        // Revoke a non-existent scope → None branch (still Ok).
        run_sandbox_command(SandboxArgs {
            command: SandboxCommand::Revoke {
                path: tmp.path().join("never-granted"),
            },
        })
        .unwrap();
    }

    #[test]
    fn web_cli_lifecycle() {
        use ahma_common::config::AhmaSettings;
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("settings.toml");
        let load = || AhmaSettings::load_from(&file);

        // allow → always_allow; deny → never_allow.
        web_command_at(
            &file,
            WebCommand::Allow {
                pattern: "api.github.com".into(),
            },
        )
        .unwrap();
        web_command_at(
            &file,
            WebCommand::Deny {
                pattern: "bad.example".into(),
            },
        )
        .unwrap();
        let s = load();
        assert!(s.web.always_allow.contains(&"api.github.com".to_string()));
        assert!(s.web.never_allow.contains(&"bad.example".to_string()));

        // Invalid patterns are rejected and write nothing.
        assert!(
            web_command_at(
                &file,
                WebCommand::Allow {
                    pattern: "*.com".into()
                }
            )
            .is_err()
        );
        assert!(
            web_command_at(
                &file,
                WebCommand::Deny {
                    pattern: "127.0.0.1".into()
                }
            )
            .is_err()
        );

        // Duplicate allow is a no-op (no error, no duplicate entry).
        web_command_at(
            &file,
            WebCommand::Allow {
                pattern: "api.github.com".into(),
            },
        )
        .unwrap();
        assert_eq!(
            load()
                .web
                .always_allow
                .iter()
                .filter(|p| *p == "api.github.com")
                .count(),
            1
        );

        // list + check must not error.
        web_command_at(&file, WebCommand::List).unwrap();
        web_command_at(
            &file,
            WebCommand::Check {
                url: "https://api.github.com/x".into(),
            },
        )
        .unwrap();

        // revoke removes only the named entry.
        web_command_at(
            &file,
            WebCommand::Revoke {
                pattern: "api.github.com".into(),
            },
        )
        .unwrap();
        let s = load();
        assert!(!s.web.always_allow.contains(&"api.github.com".to_string()));
        assert!(s.web.never_allow.contains(&"bad.example".to_string()));
    }

    // ── bundle: sign / verify ────────────────────────────────────────────────

    #[test]
    fn bundle_sign_then_verify_roundtrip() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("tool.json"),
            r#"{"name":"t","description":"d","command":"echo"}"#,
        )
        .unwrap();

        // Sign produces a manifest.
        dispatch_bundle_command(BundleArgs {
            command: BundleCommand::Sign(BundleSignArgs {
                path: tmp.path().to_path_buf(),
            }),
        })
        .unwrap();
        assert!(tmp.path().join("bundle.manifest.json").exists());

        // Verify passes against the freshly-written manifest.
        dispatch_bundle_command(BundleArgs {
            command: BundleCommand::Verify(BundleVerifyArgs {
                path: tmp.path().to_path_buf(),
            }),
        })
        .unwrap();
    }

    #[test]
    fn bundle_sign_missing_dir_errors() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let err = dispatch_bundle_command(BundleArgs {
            command: BundleCommand::Sign(BundleSignArgs { path: missing }),
        })
        .unwrap_err();
        assert!(err.to_string().contains("signing failed"));
    }

    #[test]
    fn bundle_verify_no_manifest_fails() {
        let tmp = TempDir::new().unwrap();
        // No manifest present → verify() returns Ok(false) → command bails.
        let err = dispatch_bundle_command(BundleArgs {
            command: BundleCommand::Verify(BundleVerifyArgs {
                path: tmp.path().to_path_buf(),
            }),
        })
        .unwrap_err();
        assert!(err.to_string().contains("verification failed"));
    }

    #[test]
    fn bundle_verify_tampered_file_fails() {
        let tmp = TempDir::new().unwrap();
        let tool = tmp.path().join("tool.json");
        std::fs::write(&tool, r#"{"name":"t","description":"d","command":"echo"}"#).unwrap();
        dispatch_bundle_command(BundleArgs {
            command: BundleCommand::Sign(BundleSignArgs {
                path: tmp.path().to_path_buf(),
            }),
        })
        .unwrap();
        // Mutate the file after signing so the hash no longer matches.
        std::fs::write(
            &tool,
            r#"{"name":"t","description":"changed","command":"echo"}"#,
        )
        .unwrap();
        let err = dispatch_bundle_command(BundleArgs {
            command: BundleCommand::Verify(BundleVerifyArgs {
                path: tmp.path().to_path_buf(),
            }),
        })
        .unwrap_err();
        assert!(err.to_string().contains("verification failed"));
    }

    // ── bundle: audit ────────────────────────────────────────────────────────

    #[test]
    fn bundle_audit_clean_passes() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("clean.json"),
            r#"{"name":"t","description":"a harmless description","command":"echo"}"#,
        )
        .unwrap();
        dispatch_bundle_command(BundleArgs {
            command: BundleCommand::Audit(BundleAuditArgs {
                path: tmp.path().to_path_buf(),
                strict: false,
            }),
        })
        .unwrap();
    }

    #[test]
    fn bundle_audit_warning_only_passes_without_strict() {
        let tmp = TempDir::new().unwrap();
        // A path-like arg without `format: "path"` → Warning (not Critical), so
        // result.passed stays true and the command returns Ok even with findings.
        std::fs::write(
            tmp.path().join("warn.json"),
            r#"{"name":"t","description":"d","command":"echo","path":"/some/dir"}"#,
        )
        .unwrap();
        dispatch_bundle_command(BundleArgs {
            command: BundleCommand::Audit(BundleAuditArgs {
                path: tmp.path().to_path_buf(),
                strict: false,
            }),
        })
        .unwrap();
    }

    #[test]
    fn bundle_audit_critical_secret_fails() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("secret.json"),
            r#"{"name":"t","description":"key sk-abc123","command":"echo"}"#,
        )
        .unwrap();
        // Without strict → "critical issues" bail.
        let err = audit_bundle_command(BundleAuditArgs {
            path: tmp.path().to_path_buf(),
            strict: false,
        })
        .unwrap_err();
        assert!(err.to_string().contains("critical issues"));

        // With strict and findings present → "--strict mode" bail.
        let err = audit_bundle_command(BundleAuditArgs {
            path: tmp.path().to_path_buf(),
            strict: true,
        })
        .unwrap_err();
        assert!(err.to_string().contains("strict mode"));
    }

    #[test]
    fn bundle_audit_missing_dir_errors() {
        let tmp = TempDir::new().unwrap();
        let err = audit_bundle_command(BundleAuditArgs {
            path: tmp.path().join("nope"),
            strict: false,
        })
        .unwrap_err();
        assert!(err.to_string().contains("audit failed"));
    }

    // ── validation ────────────────────────────────────────────────────────────

    #[test]
    fn validation_mode_valid_and_invalid() {
        let tmp = TempDir::new().unwrap();
        let good = tmp.path().join("good.json");
        std::fs::write(
            &good,
            r#"{"name":"good","description":"ok","command":"echo"}"#,
        )
        .unwrap();
        run_validation_mode(good.to_str().unwrap()).unwrap();

        let bad = tmp.path().join("bad.json");
        std::fs::write(&bad, "{ not valid json }").unwrap();
        let err = run_validation_mode(bad.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("Validation failed"));
    }

    // ── tool info ──────────────────────────────────────────────────────────────

    fn write_tool_dir() -> TempDir {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("mytool.json"), RICH_TOOL_JSON).unwrap();
        std::fs::write(tmp.path().join("plaintool.json"), PLAIN_TOOL_JSON).unwrap();
        tmp
    }

    #[tokio::test]
    async fn tool_info_text_format_covers_all_printers() {
        let tmp = write_tool_dir();
        run_tool_info_mode(InfoArgs {
            tool_bundles: vec![],
            tools_dir: Some(tmp.path().to_path_buf()),
            format: OutputFormat::Text,
            filter: None,
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn tool_info_json_format() {
        let tmp = write_tool_dir();
        run_tool_info_mode(InfoArgs {
            tool_bundles: vec![],
            tools_dir: Some(tmp.path().to_path_buf()),
            format: OutputFormat::Json,
            filter: None,
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn tool_info_filter_match_and_miss() {
        let tmp = write_tool_dir();
        // Matching filter → Ok.
        run_tool_info_mode(InfoArgs {
            tool_bundles: vec![],
            tools_dir: Some(tmp.path().to_path_buf()),
            format: OutputFormat::Text,
            filter: Some("mytool".into()),
        })
        .await
        .unwrap();

        // Non-existent filter → Err.
        let err = run_tool_info_mode(InfoArgs {
            tool_bundles: vec![],
            tools_dir: Some(tmp.path().to_path_buf()),
            format: OutputFormat::Text,
            filter: Some("no-such-tool".into()),
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    // ── stdio guard ────────────────────────────────────────────────────────────

    #[test]
    fn stdio_guard_ok_when_not_a_terminal() {
        // Under the test harness stdin is not a TTY, so this returns Ok and never
        // hits the interactive-error exit path.
        check_stdio_not_interactive().unwrap();
    }

    // ── The unified permission ledger (SPEC R-PERM) ──────────────────────────

    /// The `settings.toml` under a guarded temp home.
    fn ledger(home: &std::path::Path) -> std::path::PathBuf {
        home.join(".ahma").join("settings.toml")
    }

    #[test]
    fn sandbox_grant_cli_refuses_a_denylisted_path() {
        let home = TempDir::new().unwrap();
        let _guard = HomeGuard::new(home.path());

        // The CLI used to skip the denylist that the `sandbox_grant` MCP tool
        // enforced, so a human at a terminal — the *safest* surface — had the
        // weakest guardrail. Granting $HOME exposes every dotfile, key, and
        // credential; it must be refused outright, with no override flag.
        let err = run_sandbox_command(SandboxArgs {
            command: SandboxCommand::Grant {
                path: home.path().to_path_buf(),
                read_only: false,
                by: None,
                note: None,
            },
        })
        .expect_err("granting $HOME must be refused");

        let msg = format!("{err:#}");
        assert!(
            msg.contains("home directory"),
            "the refusal must say *why*, not just 'no': {msg}"
        );
        assert!(
            !ledger(home.path()).exists(),
            "a refused grant must write nothing at all"
        );
    }

    #[test]
    fn sandbox_grant_cli_allows_a_normal_external_dir_and_lists_it() {
        let home = TempDir::new().unwrap();
        let _guard = HomeGuard::new(home.path());
        let cache = home.path().join("caches").join("sccache");
        std::fs::create_dir_all(&cache).unwrap();

        run_sandbox_command(SandboxArgs {
            command: SandboxCommand::Grant {
                path: cache.clone(),
                read_only: true,
                by: Some("sccache".into()),
                note: None,
            },
        })
        .expect("an ordinary external cache dir is a legitimate grant");

        let settings =
            ahma_common::config::AhmaSettings::load_from_result(&ledger(home.path())).unwrap();
        let scope = settings
            .sandbox
            .find_scope(&cache)
            .expect("the grant is persisted");
        assert_eq!(scope.access, ahma_common::config::ScopeAccess::Ro);

        // The unified list renders it as one row of the one ledger.
        let rows = ahma_common::permissions::records(&settings);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, ahma_common::permissions::GrantKind::FsScope);
        assert_eq!(rows[0].access.as_deref(), Some("ro"));

        // And the audit trail records that it happened.
        let audit = home.path().join(".ahma").join("permissions-audit.jsonl");
        let text = std::fs::read_to_string(&audit).expect("a grant is audited");
        assert!(
            text.contains("fs-scope"),
            "audit line names the kind: {text}"
        );
        assert!(
            text.contains("\"cli\""),
            "audit line names the surface: {text}"
        );
    }

    #[test]
    fn permissions_revoke_previews_before_it_writes() {
        let home = TempDir::new().unwrap();
        let _guard = HomeGuard::new(home.path());
        let cache = home.path().join("caches").join("sccache");
        std::fs::create_dir_all(&cache).unwrap();

        run_sandbox_command(SandboxArgs {
            command: SandboxCommand::Grant {
                path: cache.clone(),
                read_only: false,
                by: None,
                note: None,
            },
        })
        .unwrap();

        // Without --yes, a revoke is a preview: it must change nothing on disk.
        run_permissions_command(PermissionsArgs {
            command: PermissionsCommand::Revoke {
                kind: "fs-scope".into(),
                subject: cache.display().to_string(),
                workspace: None,
                yes: false,
            },
        })
        .unwrap();
        let settings =
            ahma_common::config::AhmaSettings::load_from_result(&ledger(home.path())).unwrap();
        assert!(
            settings.sandbox.find_scope(&cache).is_some(),
            "a preview must not write — the user has not confirmed yet"
        );

        // With --yes, it applies.
        run_permissions_command(PermissionsArgs {
            command: PermissionsCommand::Revoke {
                kind: "fs-scope".into(),
                subject: cache.display().to_string(),
                workspace: None,
                yes: true,
            },
        })
        .unwrap();
        let settings =
            ahma_common::config::AhmaSettings::load_from_result(&ledger(home.path())).unwrap();
        assert!(
            settings.sandbox.find_scope(&cache).is_none(),
            "a confirmed revoke removes the grant"
        );
    }

    #[test]
    fn permissions_revoke_rejects_an_unknown_kind_instead_of_ignoring_it() {
        let home = TempDir::new().unwrap();
        let _guard = HomeGuard::new(home.path());

        let err = run_permissions_command(PermissionsArgs {
            command: PermissionsCommand::Revoke {
                kind: "flesscope".into(),
                subject: "/tmp/x".into(),
                workspace: None,
                yes: true,
            },
        })
        .expect_err("a typo'd kind must be an error, never a silent no-op");
        assert!(format!("{err:#}").contains("unknown permission kind"));
    }

    #[test]
    fn logs_gitignore_adds_entry_and_reports_success() {
        let temp = TempDir::new().unwrap();
        let repo_root = dunce::canonicalize(temp.path()).unwrap();
        std::fs::create_dir_all(repo_root.join(".git")).unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&repo_root).unwrap();

        let result = run_logs_command(LogsArgs {
            command: LogsCommand::Gitignore,
        });

        let _ = std::env::set_current_dir(prev);

        result.expect("gitignore command should succeed inside a git repo");
        let contents = std::fs::read_to_string(repo_root.join(".gitignore")).unwrap();
        assert!(contents.contains("logs/"), "got: {contents:?}");
    }

    #[test]
    fn logs_gitignore_errors_outside_a_git_repo() {
        let temp = TempDir::new().unwrap();
        let dir = dunce::canonicalize(temp.path()).unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();

        let result = run_logs_command(LogsArgs {
            command: LogsCommand::Gitignore,
        });

        let _ = std::env::set_current_dir(prev);

        assert!(result.is_err(), "must error outside a git repository");
    }

    #[test]
    fn permissions_list_runs_on_an_empty_ledger() {
        let home = TempDir::new().unwrap();
        let _guard = HomeGuard::new(home.path());
        // "Nothing granted" is a normal, healthy state — not an error, and not a
        // reason to create a file.
        run_permissions_command(PermissionsArgs {
            command: PermissionsCommand::List { kind: None },
        })
        .expect("listing an empty ledger succeeds");
    }
}
