//! CLI subcommand implementations.
//!
//! This module contains the handlers for `ahma settings`, `ahma prompts`,
//! `ahma bundle`, `ahma tool info`, and `ahma tool validate` commands.
//! Extracted from `cli/mod.rs` to keep each file focused and manageable.

use super::{
    AppConfig, BundleArgs, BundleAuditArgs, BundleCommand, BundleSignArgs, BundleVerifyArgs,
    InfoArgs, PromptsArgs, PromptsCommand, SettingsArgs, SettingsCommand,
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
