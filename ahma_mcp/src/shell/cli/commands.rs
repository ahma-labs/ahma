//! CLI subcommand implementations.
//!
//! This module contains the handlers for `ahma settings`, `ahma prompts`,
//! `ahma bundle`, `ahma tool info`, and `ahma tool validate` commands.
//! Extracted from `cli/mod.rs` to keep each file focused and manageable.

use super::{
    AppConfig, BundleArgs, BundleAuditArgs, BundleCommand, BundleSignArgs, BundleVerifyArgs,
    InfoArgs, PromptsArgs, PromptsCommand, SandboxArgs, SandboxCommand, SettingsArgs,
    SettingsCommand, WebArgs, WebCommand,
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
    use ahma_common::config::{
        AhmaSettings, GrantOutcome, PersistentScope, ScopeAccess, settings_path,
    };

    let file = settings_path()
        .context("Cannot determine ~/.ahma/settings.toml (home directory not found)")?;

    // Strict load on the write paths: refuse to clobber a settings file we cannot
    // parse. A missing file is fine (returns defaults) — the first grant creates it.
    let load = |p: &std::path::Path| -> Result<AhmaSettings> {
        AhmaSettings::load_from_result(p).map_err(|e| anyhow::anyhow!(e))
    };

    match args.command {
        SandboxCommand::Grant {
            path: dir,
            read_only,
            by,
            note,
        } => {
            let access = if read_only {
                ScopeAccess::Ro
            } else {
                ScopeAccess::Rw
            };
            let mut settings = load(&file)?;
            let scope = PersistentScope {
                path: dir.clone(),
                access,
                granted_by: by,
                granted_at: Some(chrono::Local::now().format("%Y-%m-%d").to_string()),
                note,
            };
            let outcome = settings.sandbox.grant_scope(scope);
            settings
                .save_to(&file)
                .with_context(|| format!("Failed to write {}", file.display()))?;

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
            println!(
                "  after you confirm a previewed line — can change it. Edit it by hand, or run"
            );
            println!(
                "  `ahma sandbox revoke {}` to remove this grant.",
                dir.display()
            );
            println!();
            println!("Takes effect the next time an ahma server starts (restart your IDE's MCP");
            println!("connection, or `ahma serve …`, to apply it now).");
            Ok(())
        }
        SandboxCommand::List => {
            let settings = load(&file)?;
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
        SandboxCommand::Revoke { path: dir } => {
            let mut settings = load(&file)?;
            match settings.sandbox.revoke_scope(&dir) {
                Some(removed) => {
                    settings
                        .save_to(&file)
                        .with_context(|| format!("Failed to write {}", file.display()))?;
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
                None => {
                    println!("No persistent scope matching {} was found.", dir.display());
                    println!("Run `ahma sandbox list` to see current grants.");
                    Ok(())
                }
            }
        }
    }
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
        println!("All configurations are valid.");
        Ok(())
    } else {
        anyhow::bail!(
            "Validation failed: {}/{} files invalid.",
            result.files_failed,
            result.files_checked
        )
    }
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
}
