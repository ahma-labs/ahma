//! # ahma binary entry point
//!
//! This crate is licensed under **AGPL-3.0-or-later**.

use anyhow::{Context, Result};
use clap::Parser as _;

use ahma_mcp::shell::cli::{
    Cli, LlmCommand, Subcommands, TlsCommand, build_app_config, dispatch_subcommand, load_settings,
};

use ahma_mcp::utils::logging::{
    detect_log_role_from_startup, init_logging_with_observability, set_log_role,
};
#[tokio::main]
async fn main() -> Result<()> {
    // Windows AppContainer launcher re-entry. Must come before *anything* else,
    // and specifically before clap: `ahma.exe` re-executes itself to spawn each
    // sandboxed command inside an AppContainer (the only way to attach
    // `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` on stable Rust — see
    // `ahma_mcp::sandbox::windows`), and the reserved marker argument it uses is
    // deliberately not a subcommand, so clap would reject it. A no-op on every
    // other platform and for every ordinary invocation; when it does fire it
    // never returns, exiting with the sandboxed program's exit code.
    ahma_mcp::sandbox::appcontainer_launcher_hook();

    // Parse via `try_parse` (not `Cli::parse`) so a malformed `ahma hooks exec …`
    // invocation never lets clap dump its top-level usage banner. An editor that
    // runs the hook treats the hook's stdout + exit code as the decision, so a
    // usage dump would surface as a hard "Hook blocked with message: <banner>".
    // Instead, emit a concise fail-open `allow` decision and exit cleanly. This
    // covers the version/flag skew where a hooks.json command written by one ahma
    // version is invoked against a different `ahma` resolved on PATH.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            use clap::error::ErrorKind;
            // Explicit `--help` / `--version` must still print normally.
            let is_help_or_version =
                matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion);
            if !is_help_or_version && ahma_mcp::hooks::try_emit_exec_parse_error_fallback() {
                std::process::exit(0);
            }
            e.exit();
        }
    };

    // --markdown-help: emit the full CLI reference as Markdown and exit.
    // Regenerate docs/cli-reference.md with:  ahma --markdown-help > docs/cli-reference.md
    if cli.markdown_help {
        print!("{}", clap_markdown::help_markdown::<Cli>());
        return Ok(());
    }

    // Settings-first log target: cli --log-to-stderr > settings.toml [logging.target] > "file".
    // `AHMA_LOG_TARGET` is RETIRED (R-CFG1.2) and already ignored here — but it said so
    // in its own words rather than through the one function that states the verdict
    // (R-CFG1.2.1). Two spellings of the same rule is how surfaces drift apart.
    let settings_for_log = load_settings(&cli);
    ahma_mcp::warn_retired_env("AHMA_LOG_TARGET");
    let log_to_stderr = cli.log_to_stderr || settings_for_log.log_to_stderr();

    set_log_role(detect_log_role_from_startup());

    let cfg = build_app_config(&cli);
    let subcommand = cli.command;

    let _telemetry_guard =
        init_logging_with_observability("info", !log_to_stderr, Some(cfg.observability.clone()))?;

    // Register the global prompt runner from ahma_core
    ahma_mcp::register_global_prompt_runner(std::sync::Arc::new(
        ahma_core::agent::CorePromptRunner,
    ));

    // Keep ~/.ahma/settings.toml in sync with this version's compiled-in
    // defaults: create it on first run, and on upgrade add any fields a newer
    // ahma introduced — preserving every value the user set. Skipped with
    // --no-settings and for `settings` subcommands (which manage the file
    // explicitly). Best-effort: a failure here is logged, never fatal.
    if !cli.no_settings && !matches!(subcommand, Subcommands::Settings(_)) {
        match ahma_common::config::AhmaSettings::ensure_current_default_path() {
            Ok(true) => tracing::info!("settings.toml synced with current defaults"),
            Ok(false) => {}
            Err(e) => tracing::warn!("could not sync settings.toml defaults: {e}"),
        }
        // Fold the retired ~/.config/ahma/approvals.json into the one ledger
        // (SPEC R-PERM.1). Idempotent and non-destructive: it runs once, renames
        // the legacy file aside rather than deleting it, and does nothing at all
        // on the overwhelming majority of startups where no legacy file exists.
        ahma_common::permissions::migrate_legacy_approvals_best_effort();
    }

    #[cfg(target_os = "windows")]
    check_powershell_available();

    match subcommand {
        Subcommands::Tui(tui_args) => {
            tracing::info!("Starting TUI control plane");
            // Resolve explicit on/off token-preference flags; None = fall back
            // to settings.toml and the deprecated env vars.
            let flag_pair = |on: bool, off: bool| -> Option<bool> {
                match (on, off) {
                    (true, _) => Some(true),
                    (_, true) => Some(false),
                    _ => None,
                }
            };
            let token_prefs = ahma_tui::TokenPrefs {
                minimize_tokens: flag_pair(cli.minimize_tokens, cli.no_minimize_tokens),
                small_model_harness: flag_pair(cli.small_model_harness, cli.no_small_model_harness),
                context_length: cli.context_length,
            };
            ahma_tui::run_tui(
                tui_args.connect.as_deref(),
                tui_args.profile.clone(),
                tui_args.path.clone(),
                token_prefs,
            )
            .await
        }
        Subcommands::Tls(tls_args) => {
            tracing::info!("TLS subcommand");
            dispatch_tls(tls_args)
        }
        Subcommands::Llm(llm_args) => {
            tracing::info!("Dispatching llm subcommand");
            dispatch_llm(llm_args).await
        }
        Subcommands::Daemon(_) => {
            tracing::info!("Starting TUI hub daemon");
            ahma_common::daemon_hub::run_daemon().await
        }
        other => dispatch_subcommand(other, cfg).await,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TLS subcommand handlers
// ─────────────────────────────────────────────────────────────────────────────

fn dispatch_tls(args: ahma_mcp::shell::TlsArgs) -> Result<()> {
    use ahma_common::local_tls::{
        LocalTlsConfig, check_rotation_needed, generate_and_save, rotate,
    };

    let config = LocalTlsConfig::from_env();
    match args.command {
        TlsCommand::Init => {
            if config.exists() {
                println!(
                    "Local TLS certificate already exists at {}",
                    config.cert_path().display()
                );
                println!("Use `ahma tls rotate` to replace it with a new certificate.");
                return Ok(());
            }
            generate_and_save(&config).context("Failed to generate local TLS certificate")?;
            println!("Local TLS certificate generated:");
            println!("  Certificate : {}", config.cert_path().display());
            println!("  Private key : {}", config.key_path().display());
            #[cfg(unix)]
            println!("  Key permissions : 0600");
        }
        TlsCommand::Rotate => {
            rotate(&config).context("Failed to rotate local TLS certificate")?;
            println!("Local TLS certificate rotated:");
            println!("  Certificate : {}", config.cert_path().display());
            println!("  Private key : {}", config.key_path().display());
        }
        TlsCommand::Status => {
            if !config.exists() {
                println!("No local TLS certificate found.");
                println!("  Expected location : {}", config.cert_path().display());
                println!("  Run `ahma tls init` to generate one.");
                return Ok(());
            }
            let meta = std::fs::metadata(config.cert_path())
                .context("Failed to read certificate metadata")?;
            let created = meta
                .created()
                .or_else(|_| meta.modified())
                .context("Failed to read certificate creation time")?;
            let age = std::time::SystemTime::now()
                .duration_since(created)
                .unwrap_or(std::time::Duration::ZERO);
            let age_days = age.as_secs() / 86_400;
            let rotation_needed = check_rotation_needed(&config);
            println!("Local TLS certificate status:");
            println!("  Certificate : {}", config.cert_path().display());
            println!("  Private key : {}", config.key_path().display());
            println!("  Age         : {} day(s)", age_days);
            if rotation_needed {
                println!("  Rotation    : RECOMMENDED (certificate is 30+ days old)");
                println!("  Run `ahma tls rotate` to regenerate it.");
            } else {
                println!("  Rotation    : not needed");
            }
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// LLM provider subcommand handlers
// ─────────────────────────────────────────────────────────────────────────────

async fn dispatch_llm(args: ahma_mcp::shell::LlmArgs) -> Result<()> {
    use ahma_common::config::{AhmaConfig, ProviderEntry, ahma_config_path};

    match args.command {
        LlmCommand::List => {
            let cfg = AhmaConfig::load();
            if cfg.providers.is_empty() {
                println!("No providers configured.");
                println!(
                    "Add one with: ahma llm add --name <name> --base-url <url> --model <model>"
                );
            } else {
                println!(
                    "{} provider(s) in ~/.ahma/config.toml:",
                    cfg.providers.len()
                );
                for p in &cfg.providers {
                    let key_hint = match &p.api_key {
                        None => "no key".to_string(),
                        Some(k) if k.starts_with("${") => format!("key: {k}"),
                        Some(_) => "key: <set>".to_string(),
                    };
                    println!(
                        "  {:<20} {}  ({})  [{}]",
                        p.name, p.base_url, p.default_model, key_hint
                    );
                }
            }
            Ok(())
        }

        LlmCommand::Add(add_args) => {
            let config_path = ahma_config_path()
                .context("Cannot determine home directory for ~/.ahma/config.toml")?;

            // Warn if the caller typed a literal key instead of ${VAR}
            if let Some(key) = &add_args.api_key {
                ahma_common::config::warn_if_looks_like_literal_secret(key);
            }

            let kind = match add_args.kind.to_ascii_lowercase().as_str() {
                "anthropic" => ahma_common::config::ProviderKind::Anthropic,
                "openai" | "" => ahma_common::config::ProviderKind::OpenAi,
                other => {
                    anyhow::bail!("Unknown provider kind '{other}'. Use 'openai' or 'anthropic'.")
                }
            };

            let mut cfg = AhmaConfig::load();

            // Reject duplicate names
            if cfg.providers.iter().any(|p| p.name == add_args.name) {
                anyhow::bail!(
                    "Provider '{}' already exists. Remove it first with: ahma llm remove {}",
                    add_args.name,
                    add_args.name
                );
            }

            cfg.providers.push(ProviderEntry {
                name: add_args.name.clone(),
                kind,
                base_url: add_args.base_url.clone(),
                default_model: add_args.model.clone(),
                api_key: add_args.api_key.clone(),
                num_ctx: add_args.num_ctx,
            });

            // Ensure the directory exists before writing
            if let Some(parent) = config_path.parent() {
                std::fs::create_dir_all(parent).context("Failed to create ~/.ahma/ directory")?;
            }
            let toml_text =
                toml::to_string_pretty(&cfg).context("Failed to serialize config to TOML")?;
            std::fs::write(&config_path, toml_text)
                .with_context(|| format!("Failed to write {}", config_path.display()))?;

            println!("Added provider '{}'.", add_args.name);
            Ok(())
        }

        LlmCommand::Test(test_args) => {
            let cfg = AhmaConfig::load();
            let entry = cfg
                .providers
                .iter()
                .find(|p| p.name == test_args.name)
                .with_context(|| {
                    format!(
                        "Provider '{}' not found. List providers with: ahma llm list",
                        test_args.name
                    )
                })?;

            // Resolve the key (expand ${VAR})
            let resolved = entry.resolve()?;
            let model_url = format!("{}/models", resolved.base_url.trim_end_matches('/'));

            print!("Testing '{}' at {} ... ", test_args.name, model_url);

            let mut req = reqwest::Client::new().get(&model_url);
            if let Some(key) = &resolved.api_key {
                req = req.bearer_auth(key);
            }
            let resp = req
                .send()
                .await
                .with_context(|| format!("Failed to reach {model_url}"))?;

            let status = resp.status();
            if status.is_success() {
                println!("OK ({status})");
            } else {
                println!("FAIL ({status})");
                anyhow::bail!("Provider returned HTTP {status}");
            }
            Ok(())
        }

        LlmCommand::Remove(rm_args) => {
            let config_path = ahma_config_path()
                .context("Cannot determine home directory for ~/.ahma/config.toml")?;

            let mut cfg = AhmaConfig::load();
            let before = cfg.providers.len();
            cfg.providers.retain(|p| p.name != rm_args.name);

            if cfg.providers.len() == before {
                anyhow::bail!(
                    "Provider '{}' not found. List providers with: ahma llm list",
                    rm_args.name
                );
            }

            let toml_text =
                toml::to_string_pretty(&cfg).context("Failed to serialize config to TOML")?;
            std::fs::write(&config_path, toml_text)
                .with_context(|| format!("Failed to write {}", config_path.display()))?;

            println!("Removed provider '{}'.", rm_args.name);
            Ok(())
        }
    }
}

#[cfg(target_os = "windows")]
fn check_powershell_available() {
    let ps_check = std::process::Command::new("powershell")
        .arg("-NoProfile")
        .arg("-Command")
        .arg("$PSVersionTable.PSVersion.ToString()")
        .output();
    match ps_check {
        Ok(out) if out.status.success() => {
            let ver = String::from_utf8_lossy(&out.stdout);
            tracing::info!("PowerShell detected: {}", ver.trim());
        }
        _ => {
            eprintln!(
                "\nFAIL Error: PowerShell was not found.\n\n\
                 ahma requires PowerShell (built into Windows 10/11) as its runtime shell.\n"
            );
            std::process::exit(1);
        }
    }
}
