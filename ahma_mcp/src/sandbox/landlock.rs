use anyhow::Result;
use std::path::PathBuf;

/// Apply Landlock sandbox restrictions to the current process.
#[cfg(target_os = "linux")]
pub fn enforce_landlock_sandbox(
    scopes: &[PathBuf],
    read_scopes: &[PathBuf],
    no_temp_files: bool,
    package_cache_write: bool,
) -> Result<()> {
    use anyhow::Context;
    use landlock::{
        ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
    };

    // Use V1 for maximum kernel compatibility — it includes all the core FS access
    // flags we actually enforce (ReadFile, WriteFile, Execute, MakeDir, …).
    // V5 only adds IoctlDev which we don't use, and requesting it causes a
    // `PartiallyEnforced` status on kernels < 6.10 (e.g. ubuntu-latest 5.15/6.5
    // GitHub Actions runners), making enforcement appear weaker than it is.
    let abi = ABI::V1;
    let access_all = AccessFs::from_all(abi);
    let access_read = AccessFs::from_read(abi);

    tracing::info!("Enforcing Landlock sandbox using ABI: {:?}", abi);

    let mut ruleset = Ruleset::default()
        .handle_access(access_all)
        .context("Failed to create Landlock ruleset")?
        .create()
        .context("Failed to create Landlock ruleset instance")?;

    // Add sandbox scopes
    for scope in scopes {
        ruleset = ruleset
            .add_rule(PathBeneath::new(
                PathFd::new(scope).context("Failed to open sandbox scope for Landlock")?,
                access_all,
            ))
            .context("Failed to add Landlock rule for sandbox scope")?;
    }

    // Add explicit read_scopes target files (for --livelog)
    for read_scope in read_scopes {
        if let Ok(fd) = PathFd::new(read_scope) {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, access_read))
                .context("Failed to add Landlock rule for read-only scope")?;
        }
    }

    add_landlock_system_rules(&mut ruleset, access_read)?;
    add_landlock_home_tool_rules(&mut ruleset, access_read, access_all, package_cache_write)?;

    if !no_temp_files {
        add_landlock_temp_rules(&mut ruleset, access_all)?;
    }

    let status = ruleset
        .restrict_self()
        .context("Failed to apply Landlock restrictions")?;

    match status.ruleset {
        landlock::RulesetStatus::NotEnforced => {
            return Err(anyhow::anyhow!(
                "Failed to enforce Landlock sandbox: enforcement was refused by kernel \
                 (status: {:?}). Ensure your kernel supports Landlock (5.13+) and the \
                 process has sufficient privileges.",
                status
            ));
        }
        landlock::RulesetStatus::PartiallyEnforced => {
            // This is unexpected with ABI::V1 — all V1 access flags should be
            // supported by any kernel that passes check_sandbox_prerequisites().
            // Log prominently so CI failures are diagnosable.
            tracing::warn!(
                "Landlock sandbox is PARTIALLY enforced for scopes: {:?} (status: {:?}). \
                 Some access flags were downgraded — the kernel may not fully support ABI V1. \
                 Consider verifying kernel version and Landlock LSM configuration.",
                scopes,
                status
            );
        }
        landlock::RulesetStatus::FullyEnforced => {
            tracing::info!("Landlock sandbox fully enforced for scopes: {:?}", scopes);
        }
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn add_landlock_system_rules(
    ruleset: &mut landlock::RulesetCreated,
    access_read: landlock::BitFlags<landlock::AccessFs>,
) -> Result<()> {
    use landlock::{AccessFs, PathBeneath, PathFd, RulesetCreatedAttr};
    let system_paths = [
        "/usr", "/bin", "/sbin", "/etc", "/lib", "/lib64", "/proc", "/dev", "/sys",
    ];
    let access_read_execute = access_read | AccessFs::Execute;
    for path in &system_paths {
        let path_obj = std::path::Path::new(path);
        if path_obj.exists()
            && let Ok(fd) = PathFd::new(path_obj)
        {
            let _ = ruleset.add_rule(PathBeneath::new(fd, access_read_execute));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn add_landlock_home_tool_rules(
    ruleset: &mut landlock::RulesetCreated,
    access_read: landlock::BitFlags<landlock::AccessFs>,
    access_all: landlock::BitFlags<landlock::AccessFs>,
    package_cache_write: bool,
) -> Result<()> {
    use landlock::{PathBeneath, PathFd, RulesetCreatedAttr};
    if let Ok(home) = std::env::var("HOME") {
        let home_path = std::path::Path::new(&home);
        let tool_paths = [".cargo", ".rustup", ".nvm", ".npm", ".go", ".cache"];
        for tool in &tool_paths {
            let path = home_path.join(tool);
            if path.exists()
                && let Ok(fd) = PathFd::new(&path)
            {
                let _ = ruleset.add_rule(PathBeneath::new(fd, access_read));
            }
        }
    }

    if package_cache_write {
        add_landlock_package_cache_write_rules(ruleset, access_all)?;
    }

    Ok(())
}

/// Add Landlock write rules for package-manager caches when `package_cache_write` is on.
#[cfg(target_os = "linux")]
fn add_landlock_package_cache_write_rules(
    ruleset: &mut landlock::RulesetCreated,
    access_all: landlock::BitFlags<landlock::AccessFs>,
) -> Result<()> {
    use super::pkg_cache::{all_writable_package_cache_paths, pre_create_package_cache_paths};
    use landlock::{PathBeneath, PathFd, RulesetCreatedAttr};

    for cache in all_writable_package_cache_paths() {
        // Ensure paths exist so PathFd::new succeeds.
        pre_create_package_cache_paths(&cache);

        for dir in &cache.writable_dirs {
            if dir.exists()
                && let Ok(fd) = PathFd::new(dir)
            {
                tracing::debug!(
                    "Landlock: granting write access to package cache dir: {:?}",
                    dir
                );
                let _ = ruleset.add_rule(PathBeneath::new(fd, access_all));
            }
        }

        for file in &cache.writable_files {
            if file.exists()
                && let Ok(fd) = PathFd::new(file)
            {
                tracing::debug!(
                    "Landlock: granting write access to package cache file: {:?}",
                    file
                );
                let _ = ruleset.add_rule(PathBeneath::new(fd, access_all));
            }
        }
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn add_landlock_temp_rules(
    ruleset: &mut landlock::RulesetCreated,
    access_all: landlock::BitFlags<landlock::AccessFs>,
) -> Result<()> {
    use landlock::{PathBeneath, PathFd, RulesetCreatedAttr};
    let tmp_path = std::path::Path::new("/tmp");
    if tmp_path.exists()
        && let Ok(fd) = PathFd::new(tmp_path)
    {
        let _ = ruleset.add_rule(PathBeneath::new(fd, access_all));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn enforce_landlock_sandbox(
    _scopes: &[PathBuf],
    _read_scopes: &[PathBuf],
    _no_temp_files: bool,
    _package_cache_write: bool,
) -> Result<()> {
    Ok(())
}
