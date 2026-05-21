//! # ahma binary entry point
//!
//! This crate is licensed under **GPL-3.0-or-later**.  It statically links
//! `ahma_cluster` (AGPL-3.0-or-later), so the compiled binary is effectively
//! AGPL-3.0-or-later for redistribution and network-service purposes.

use anyhow::{Context, Result};
use clap::Parser as _;

use ahma_mcp::shell::cli::{
    Cli, ClusterCommand, LlmCommand, Subcommands, VaultCommand, build_app_config,
    dispatch_subcommand,
};
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
        Subcommands::Llm(llm_args) => {
            tracing::info!("Dispatching llm subcommand");
            dispatch_llm(llm_args).await
        }
        Subcommands::Cluster(cluster_args) => {
            tracing::info!("Dispatching cluster subcommand");
            dispatch_cluster(cluster_args).await
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
                println!("Add one with: ahma llm add --name <name> --base-url <url> --model <model>");
            } else {
                println!("{} provider(s) in ~/.ahma/config.toml:", cfg.providers.len());
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
                base_url: add_args.base_url.clone(),
                default_model: add_args.model.clone(),
                api_key: add_args.api_key.clone(),
            });

            // Ensure the directory exists before writing
            if let Some(parent) = config_path.parent() {
                std::fs::create_dir_all(parent)
                    .context("Failed to create ~/.ahma/ directory")?;
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
            let entry = cfg.providers
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
            let models_url = format!(
                "{}/models",
                resolved.base_url.trim_end_matches('/')
            );

            print!("Testing '{}' at {} ... ", test_args.name, models_url);

            let mut req = reqwest::Client::new().get(&models_url);
            if let Some(key) = &resolved.api_key {
                req = req.bearer_auth(key);
            }
            let resp = req.send().await
                .with_context(|| format!("Failed to reach {models_url}"))?;

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

// ─────────────────────────────────────────────────────────────────────────────
// Cluster peer subcommand handlers
// ─────────────────────────────────────────────────────────────────────────────

/// Path to the static peers file.
fn peers_path() -> Result<std::path::PathBuf> {
    dirs::home_dir()
        .context("Cannot determine home directory for ~/.ahma/cluster/peers.json")
        .map(|h| h.join(".ahma").join("cluster").join("peers.json"))
}

/// Read the peers list from disk, returning an empty vec if the file is absent.
fn read_peers() -> Result<Vec<ahma_cluster::PeerInfo>> {
    let path = peers_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("Failed to parse {}", path.display()))
}

/// Write the peers list to disk (creates directory if needed).
fn write_peers(peers: &[ahma_cluster::PeerInfo]) -> Result<()> {
    let path = peers_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .context("Failed to create ~/.ahma/cluster/ directory")?;
    }
    let text = serde_json::to_string_pretty(peers)
        .context("Failed to serialize peers to JSON")?;
    std::fs::write(&path, text)
        .with_context(|| format!("Failed to write {}", path.display()))
}

async fn dispatch_cluster(args: ahma_mcp::shell::ClusterArgs) -> Result<()> {
    match args.command {
        ClusterCommand::List => {
            let peers = read_peers()?;
            if peers.is_empty() {
                println!("No peers configured.");
                println!("Add one with: ahma cluster add-peer --id <id> --addr <addr>");
            } else {
                println!("{} peer(s) in ~/.ahma/cluster/peers.json:", peers.len());
                for p in &peers {
                    let models = if p.models.is_empty() {
                        "no models listed".to_string()
                    } else {
                        p.models.join(", ")
                    };
                    println!("  {:<20} {}  [{}]", p.id, p.addr, models);
                }
            }
            Ok(())
        }

        ClusterCommand::AddPeer(add_args) => {
            let mut peers = read_peers()?;

            if peers.iter().any(|p| p.id == add_args.id) {
                anyhow::bail!(
                    "Peer '{}' already exists. Remove it first with: ahma cluster remove {}",
                    add_args.id,
                    add_args.id
                );
            }

            // Filter out empty strings from the default value
            let models: Vec<String> = add_args
                .models
                .into_iter()
                .filter(|m| !m.is_empty())
                .collect();

            peers.push(ahma_cluster::PeerInfo {
                id: add_args.id.clone(),
                addr: add_args.addr.clone(),
                models,
                active_ops: 0,
                reachable: true,
                capabilities: None,
            });

            write_peers(&peers)?;
            println!("Added peer '{}'.", add_args.id);
            Ok(())
        }

        ClusterCommand::Ping(ping_args) => {
            let peers = read_peers()?;
            let peer = peers
                .iter()
                .find(|p| p.id == ping_args.id)
                .with_context(|| {
                    format!(
                        "Peer '{}' not found. List peers with: ahma cluster list",
                        ping_args.id
                    )
                })?;

            let health_url = format!("{}/health", peer.addr.trim_end_matches('/'));
            print!("Pinging '{}' at {} ... ", ping_args.id, health_url);

            let resp = reqwest::Client::new()
                .get(&health_url)
                .send()
                .await
                .with_context(|| format!("Failed to reach {health_url}"))?;

            let status = resp.status();
            if status.is_success() {
                println!("OK ({status})");
            } else {
                println!("FAIL ({status})");
                anyhow::bail!("Peer returned HTTP {status}");
            }
            Ok(())
        }

        ClusterCommand::Status => {
            let peers = read_peers()?;
            if peers.is_empty() {
                println!("No peers configured.");
                return Ok(());
            }

            println!("{} peer(s):", peers.len());
            let client = reqwest::Client::new();
            for p in &peers {
                let health_url = format!("{}/health", p.addr.trim_end_matches('/'));
                let reachable = client
                    .get(&health_url)
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false);

                let models = if p.models.is_empty() {
                    "no models listed".to_string()
                } else {
                    p.models.join(", ")
                };
                let status_icon = if reachable { "UP  " } else { "DOWN" };
                println!("  [{}] {:<20} {}  [{}]", status_icon, p.id, p.addr, models);

                if let Some(caps) = &p.capabilities {
                    println!(
                        "        loaded: {}  active: {}  vram_free: {}",
                        caps.models_loaded.join(", "),
                        caps.active_ops,
                        caps.vram_free_mb
                            .map(|v| format!("{v} MiB"))
                            .unwrap_or_else(|| "unknown".to_string())
                    );
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
