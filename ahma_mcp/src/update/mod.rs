//! CLI subcommand and orchestration for `ahma update`.

mod install;
mod platform;
mod ref_mode;
mod release;
mod source;

use anyhow::{Context, Result};
use clap::Args;
use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

pub use install::{cargo_install_root, default_install_dir};
pub use ref_mode::{UpdateMode, classify_ref};
pub use source::build_cargo_install_command;

use install::{install_release_asset, read_installed_version};
use platform::detect_platform;
use release::{fetch_latest_asset, fetch_tagged_asset};
use source::install_from_git_ref;

/// Download or build and install ahma.
#[derive(Args, Debug)]
#[command(
    about = "Download or build and install ahma",
    long_about = "Update the installed ahma binary.\n\n\
        With no ref: install the latest published GitHub release.\n\
        With a semver ref (e.g. 0.6.7 or v0.6.7): install that release tag.\n\
        With a branch ref (e.g. main or feature/update): build from GitHub source via cargo install.\n\n\
        For an unpushed local checkout, use:\n\
          RUSTFLAGS='--cfg reqwest_unstable' cargo install --path ahma_bin --bin ahma --root ~/.local --locked --force",
    after_help = "EXAMPLES:
  # Install latest published release
  ahma update

  # Install a specific release
  ahma update 0.6.7

  # Build and install from a Git branch
  ahma update main
  ahma update feature/update

  # Custom install location
  ahma update --install-dir ~/.local/bin

    # Also install user-scoped terminal hooks
    ahma update --install-hooks

  # Preview actions without writing
  ahma update --dry-run"
)]
pub struct UpdateArgs {
    /// Release tag or Git branch (default: latest published release)
    #[arg(value_name = "REF")]
    pub reference: Option<String>,

    /// Install directory (default: ~/.local/bin, or AHMA_INSTALL_DIR)
    #[arg(long)]
    pub install_dir: Option<PathBuf>,

    /// Reinstall even when the installed version already matches
    #[arg(long)]
    pub force: bool,

    /// Install user-scoped terminal hooks after updating.
    #[arg(long)]
    pub install_hooks: bool,

    /// Print planned actions without downloading or installing
    #[arg(long)]
    pub dry_run: bool,
}

struct UpdateOutcome {
    binary_path: PathBuf,
    binary_changed: bool,
}

/// Entry point for `ahma update`.
pub async fn run(args: UpdateArgs) -> Result<()> {
    let install_dir = args
        .install_dir
        .clone()
        .or_else(|| std::env::var("AHMA_INSTALL_DIR").ok().map(PathBuf::from))
        .unwrap_or_else(|| default_install_dir().expect("home directory"));

    let mode = classify_ref(args.reference.as_deref());
    let platform = detect_platform().ok();

    warn_if_running_binary_differs(&install_dir).await;

    let outcome = match mode {
        UpdateMode::LatestRelease | UpdateMode::TaggedRelease { .. } => {
            let platform = platform.context(
                "Prebuilt releases are unavailable on this platform. \
                 Try: ahma update main",
            )?;
            run_release_update(&args, &platform, &install_dir, &mode).await
        }
        UpdateMode::GitRef { branch } => run_git_update(&branch, &install_dir, args.dry_run).await,
    }?;

    if outcome.binary_changed {
        print_post_install_details(&outcome.binary_path, &install_dir, args.dry_run).await;
    }
    maybe_run_setup_wizard(&args, &outcome.binary_path).await
}

async fn run_git_update(branch: &str, install_dir: &Path, dry_run: bool) -> Result<UpdateOutcome> {
    let installed = install_from_git_ref(branch, install_dir, dry_run).await?;
    Ok(UpdateOutcome {
        binary_path: installed,
        binary_changed: !dry_run,
    })
}

async fn run_release_update(
    args: &UpdateArgs,
    platform: &platform::Platform,
    install_dir: &Path,
    mode: &UpdateMode,
) -> Result<UpdateOutcome> {
    let client = reqwest::Client::builder()
        .user_agent("ahma-updater")
        .build()
        .context("Failed to create HTTP client")?;

    let asset = match mode {
        UpdateMode::LatestRelease => fetch_latest_asset(&client, platform).await?,
        UpdateMode::TaggedRelease { tag } => fetch_tagged_asset(&client, tag, platform).await?,
        UpdateMode::GitRef { .. } => unreachable!("release path only"),
    };

    let target = install_dir.join(platform.binary_name());
    if !args.force
        && let Some(installed) = read_installed_version(&target).await
    {
        if installed == asset.version {
            println!(
                "Ahma {installed} is already installed at {}",
                target.display()
            );
            if args.dry_run {
                println!("[dry-run] Would reinstall with --force");
            } else {
                println!("Use --force to reinstall anyway.");
            }
            return Ok(UpdateOutcome {
                binary_path: target,
                binary_changed: false,
            });
        }
        println!("Upgrading ahma from {installed} to {}...", asset.version);
    }

    println!("Installing Ahma {} for {}...", asset.version, platform.id);
    let installed =
        install_release_asset(&client, &asset, platform, install_dir, args.dry_run).await?;

    Ok(UpdateOutcome {
        binary_path: installed,
        binary_changed: !args.dry_run,
    })
}

async fn print_post_install_details(installed: &Path, install_dir: &Path, dry_run: bool) {
    if dry_run {
        return;
    }

    let version = read_installed_version(installed).await;
    println!("{}", format_install_success(installed, version.as_deref()));
    print_path_hint(install_dir);
    print_restart_hint();
}

fn format_install_success(installed: &std::path::Path, version: Option<&str>) -> String {
    match version {
        Some(version) => format!(
            "Success! ahma {version} installed to {}",
            installed.display()
        ),
        None => format!("Success! Installed {}", installed.display()),
    }
}

async fn warn_if_running_binary_differs(install_dir: &std::path::Path) {
    let Ok(current) = std::env::current_exe() else {
        return;
    };
    let target = install_dir.join(if cfg!(target_os = "windows") {
        "ahma.exe"
    } else {
        "ahma"
    });
    if target.exists()
        && let (Ok(a), Ok(b)) = (dunce::canonicalize(&current), dunce::canonicalize(&target))
        && a != b
    {
        eprintln!(
            "Note: running {} but updating {}.",
            a.display(),
            b.display()
        );
    }
}

async fn maybe_run_setup_wizard(args: &UpdateArgs, binary_path: &Path) -> Result<()> {
    if args.dry_run {
        if args.install_hooks {
            println!(
                "[dry-run] Would run {} setup --hooks --auto",
                binary_path.display()
            );
        }
        return Ok(());
    }

    if args.install_hooks {
        run_setup_hooks_only_auto(binary_path).await?;
        return Ok(());
    }

    if !can_prompt_for_setup() {
        run_setup_skills_only_auto(binary_path).await?;
        return Ok(());
    }

    println!();
    println!(
        "Optional: run the setup wizard to configure MCP servers, terminal hooks, TLS, and agent skills."
    );

    if !prompt_yes_no("Run the setup wizard now? [y/N]: ").await? {
        println!("Tip: run `ahma setup` later to configure your environment.");
        return Ok(());
    }

    if let Err(error) = run_setup_interactive(binary_path).await {
        eprintln!("Warning: updated ahma but failed to run setup: {error}");
        eprintln!("Run `ahma setup` later to retry.");
    }

    Ok(())
}

fn can_prompt_for_setup() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

async fn prompt_yes_no(prompt: &str) -> Result<bool> {
    let prompt = prompt.to_string();
    tokio::task::spawn_blocking(move || {
        print!("{prompt}");
        io::stdout().flush().context("Failed to flush prompt")?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("Failed to read prompt response")?;

        Ok(matches!(input.trim(), "y" | "Y" | "yes" | "Yes" | "YES"))
    })
    .await
    .context("Setup prompt task failed")?
}

async fn run_setup_hooks_only_auto(binary_path: &Path) -> Result<()> {
    println!();
    println!("Configuring terminal hooks automatically...");

    let status = tokio::process::Command::new(binary_path)
        .args(["setup", "--hooks", "--auto"])
        .status()
        .await
        .with_context(|| {
            format!(
                "Failed to run {} setup --hooks --auto",
                binary_path.display()
            )
        })?;

    if !status.success() {
        anyhow::bail!(
            "{} setup --hooks --auto exited with status {}",
            binary_path.display(),
            status
        );
    }
    Ok(())
}

async fn run_setup_skills_only_auto(binary_path: &Path) -> Result<()> {
    println!();
    println!("Configuring agent skills automatically...");

    let status = tokio::process::Command::new(binary_path)
        .args(["setup", "--skills", "--auto"])
        .status()
        .await
        .with_context(|| {
            format!(
                "Failed to run {} setup --skills --auto",
                binary_path.display()
            )
        })?;

    if !status.success() {
        anyhow::bail!(
            "{} setup --skills --auto exited with status {}",
            binary_path.display(),
            status
        );
    }
    Ok(())
}

async fn run_setup_interactive(binary_path: &Path) -> Result<()> {
    let status = tokio::process::Command::new(binary_path)
        .arg("setup")
        .status()
        .await
        .with_context(|| format!("Failed to run {} setup", binary_path.display()))?;

    if !status.success() {
        anyhow::bail!(
            "{} setup exited with status {}",
            binary_path.display(),
            status
        );
    }
    Ok(())
}

fn print_path_hint(install_dir: &std::path::Path) {
    println!();
    println!("Ensure {} is on your PATH.", install_dir.display());
    #[cfg(not(windows))]
    println!("  export PATH=\"{}:$PATH\"", install_dir.display());
    #[cfg(windows)]
    println!(
        "  [Environment]::SetEnvironmentVariable('PATH', \"$env:PATH;{}\", 'User')",
        install_dir.display()
    );
}

fn print_restart_hint() {
    println!();
    println!("Restart MCP clients or reload your IDE window to pick up the new binary.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_update_args_parse_defaults() {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[command(subcommand)]
            cmd: UpdateCmd,
        }
        #[derive(clap::Subcommand)]
        enum UpdateCmd {
            Update(UpdateArgs),
        }
        let cli = Cli::try_parse_from(["ahma", "update"]).unwrap();
        let UpdateCmd::Update(args) = cli.cmd;
        assert!(args.reference.is_none());
        assert!(!args.force);
        assert!(!args.install_hooks);
        assert!(!args.dry_run);
    }

    #[test]
    fn test_update_args_parse_install_hooks() {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[command(subcommand)]
            cmd: UpdateCmd,
        }
        #[derive(clap::Subcommand)]
        enum UpdateCmd {
            Update(UpdateArgs),
        }

        let cli = Cli::try_parse_from(["ahma", "update", "--install-hooks"]).unwrap();
        let UpdateCmd::Update(args) = cli.cmd;
        assert!(args.install_hooks);
    }

    #[test]
    fn test_format_install_success_with_version() {
        let message = format_install_success(std::path::Path::new("/tmp/ahma"), Some("0.7.0"));
        assert_eq!(message, "Success! ahma 0.7.0 installed to /tmp/ahma");
    }

    #[test]
    fn test_format_install_success_without_version() {
        let message = format_install_success(std::path::Path::new("/tmp/ahma"), None);
        assert_eq!(message, "Success! Installed /tmp/ahma");
    }
}
