//! # ahma binary entry point
//!
//! This crate is licensed under **GPL-3.0-or-later**.  It statically links
//! `ahma_cluster` (AGPL-3.0-or-later), so the compiled binary is effectively
//! AGPL-3.0-or-later for redistribution and network-service purposes.

use anyhow::{Context, Result};
use clap::Parser as _;

use ahma_mcp::shell::cli::{Cli, Subcommands, VaultCommand, build_app_config, dispatch_subcommand};
use ahma_mcp::utils::logging::init_logging_with_observability;

#[tokio::main]
async fn main() -> Result<()> {
    let log_to_stderr = std::env::var("AHMA_LOG_TARGET")
        .map(|v| v.trim().eq_ignore_ascii_case("stderr"))
        .unwrap_or(false);

    let cli = Cli::parse();
    let cfg = build_app_config(&cli);
    let subcommand = cli.command;

    let _telemetry_guard =
        init_logging_with_observability("info", !log_to_stderr, Some(cfg.observability.clone()))?;

    #[cfg(target_os = "windows")]
    check_powershell_available();

    match subcommand {
        Subcommands::Vault(vault_args) => {
            tracing::info!("Dispatching vault subcommand");
            dispatch_vault(vault_args)
        }
        Subcommands::Tui(tui_args) => {
            tracing::info!("Starting TUI control plane");
            ahma_tui::run_tui(&tui_args.connect).await
        }
        other => dispatch_subcommand(other, cfg).await,
    }
}

fn dispatch_vault(args: ahma_mcp::shell::VaultArgs) -> Result<()> {
    match args.command {
        VaultCommand::Create(create_args) => {
            let vault = ahma_vault::TaskVault::create(&create_args.slug)
                .context("Failed to create task vault")?;
            println!("{}", vault.path().display());
            tracing::info!(
                "Task vault created: {} (sandbox scope: {})",
                vault.path().display(),
                vault.sandbox_scope().display()
            );
            Ok(())
        }
        VaultCommand::List => {
            let home = dirs::home_dir().context("Cannot determine home directory")?;
            let tasks_dir = home.join(".ahma").join("tasks");
            if !tasks_dir.exists() {
                println!("No vaults found ({})", tasks_dir.display());
                return Ok(());
            }
            let mut entries: Vec<_> = std::fs::read_dir(&tasks_dir)?
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .collect();
            entries.sort_by_key(|e| e.path());
            if entries.is_empty() {
                println!("No task vaults found.");
            } else {
                println!("Task vaults in {}:", tasks_dir.display());
                for entry in entries {
                    println!("  {}", entry.file_name().to_string_lossy());
                }
            }
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
