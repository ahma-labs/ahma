//! # ahma binary entry point
//!
//! This crate is licensed under **AGPL-3.0-or-later**.

use anyhow::{Context, Result};
use clap::Parser as _;

use ahma_mcp::shell::cli::{
    Cli, ClusterCommand, LlmCommand, Subcommands, TlsCommand, VaultCommand, build_app_config,
    dispatch_subcommand,
};
use ahma_mcp::utils::logging::init_logging_with_observability;
#[tokio::main]
async fn main() -> Result<()> {
    let log_to_stderr = std::env::var("AHMA_LOG_TARGET")
        .map(|v| v.trim().eq_ignore_ascii_case("stderr"))
        .unwrap_or(false);

    let cli = Cli::parse();

    // --markdown-help: emit the full CLI reference as Markdown and exit.
    // Regenerate docs/cli-reference.md with:  ahma --markdown-help > docs/cli-reference.md
    if cli.markdown_help {
        print!("{}", clap_markdown::help_markdown::<Cli>());
        return Ok(());
    }

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
            ahma_tui::run_tui(tui_args.connect.as_deref()).await
        }
        Subcommands::Tls(tls_args) => {
            tracing::info!("TLS subcommand");
            dispatch_tls(tls_args)
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
            let models_url = format!("{}/models", resolved.base_url.trim_end_matches('/'));

            print!("Testing '{}' at {} ... ", test_args.name, models_url);

            let mut req = reqwest::Client::new().get(&models_url);
            if let Some(key) = &resolved.api_key {
                req = req.bearer_auth(key);
            }
            let resp = req
                .send()
                .await
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
    serde_json::from_str(&text).with_context(|| format!("Failed to parse {}", path.display()))
}

/// Write the peers list to disk (creates directory if needed).
fn write_peers(peers: &[ahma_cluster::PeerInfo]) -> Result<()> {
    let path = peers_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("Failed to create ~/.ahma/cluster/ directory")?;
    }
    let text = serde_json::to_string_pretty(peers).context("Failed to serialize peers to JSON")?;
    std::fs::write(&path, text).with_context(|| format!("Failed to write {}", path.display()))
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

        ClusterCommand::Discover => {
            println!("Browsing for ahma worker peers via mDNS (_ahma-worker._tcp.local.)…");
            println!("Press Ctrl-C to stop.\n");
            let registry = ahma_cluster::WorkerRegistry::new(120);
            registry.start_mdns_discovery().await;
            // Poll and print for 30 s then exit, or run until Ctrl-C.
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                for peer in registry.all_live() {
                    if seen.insert(peer.id.clone()) {
                        let models = if peer.models.is_empty() {
                            "no models listed".to_string()
                        } else {
                            peer.models.join(", ")
                        };
                        println!("  Found: {:<20} {}  [{}]", peer.id, peer.addr, models);
                    }
                }
                if std::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            if seen.is_empty() {
                println!("No ahma peers discovered on the local network.");
            } else {
                println!("\nDiscovery complete ({} peer(s) found).", seen.len());
            }
            Ok(())
        }

        ClusterCommand::Announce(ann_args) => {
            let id = ann_args.id.unwrap_or_else(|| {
                std::env::var("HOSTNAME")
                    .or_else(|_| std::env::var("COMPUTERNAME"))
                    .unwrap_or_else(|_| "ahma-worker".to_owned())
            });
            let models: Vec<String> = ann_args
                .models
                .into_iter()
                .filter(|m| !m.is_empty())
                .collect();
            let registry = ahma_cluster::WorkerRegistry::new(120);
            registry.announce_self(&id, ann_args.port, &models)?;
            println!(
                "Announcing '{}' on port {} via mDNS. Press Ctrl-C to stop.",
                id, ann_args.port
            );
            // Keep running until killed.
            tokio::signal::ctrl_c().await?;
            Ok(())
        }

        ClusterCommand::Cert(cert_cmd) => dispatch_cert(cert_cmd),
    }
}

fn dispatch_cert(cmd: ahma_mcp::shell::CertCommand) -> Result<()> {
    use ahma_mcp::shell::CertCommand;
    match cmd {
        CertCommand::Init { out_dir } => {
            let out_path = if out_dir.starts_with('~') {
                let home =
                    dirs::home_dir().context("Cannot determine home directory for cert init")?;
                home.join(&out_dir[2..])
            } else {
                std::path::PathBuf::from(&out_dir)
            };
            ahma_cluster::tls::generate_self_signed_cluster_certs(&out_path)?;
            println!("mTLS certificates written to {}", out_path.display());
            println!("  ca.pem    — CA certificate (share with all peers)");
            println!("  cert.pem  — leaf certificate for this peer");
            println!("  key.pem   — private key for this peer (keep secret)");
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
