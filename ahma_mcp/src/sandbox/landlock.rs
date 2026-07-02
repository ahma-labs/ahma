use anyhow::Result;
use std::path::PathBuf;

/// Build the Landlock ruleset for the given scopes without applying it.
///
/// Shared by [`enforce_landlock_sandbox`] (process-level, `restrict_self`) and
/// [`landlock_ruleset_fd`] (spawn-time, applied in the child via `pre_exec`).
#[cfg(target_os = "linux")]
fn build_landlock_ruleset(
    scopes: &[PathBuf],
    read_scopes: &[PathBuf],
    no_temp_files: bool,
    package_cache_write: bool,
    connect_tcp_port: Option<u16>,
) -> Result<landlock::RulesetCreated> {
    use anyhow::Context;
    use landlock::{
        ABI, Access, AccessFs, AccessNet, NetPort, PathBeneath, PathFd, Ruleset, RulesetAttr,
        RulesetCreatedAttr,
    };

    // Use V1 for maximum kernel compatibility — it includes all the core FS access
    // flags we actually enforce (ReadFile, WriteFile, Execute, MakeDir, …).
    // V5 only adds IoctlDev which we don't use, and requesting it causes a
    // `PartiallyEnforced` status on kernels < 6.10 (e.g. ubuntu-latest 5.15/6.5
    // GitHub Actions runners), making enforcement appear weaker than it is.
    let abi = ABI::V1;
    let access_all = AccessFs::from_all(abi);
    let access_read = AccessFs::from_read(abi);

    let mut builder = Ruleset::default()
        .handle_access(access_all)
        .context("Failed to create Landlock ruleset")?;
    // R-NET (child only): additionally handle TCP `connect()` so the child can be
    // confined to the egress proxy port. Best-effort by default: on kernels < 6.7
    // (Landlock ABI < V4) this access is silently dropped and only the filesystem
    // rules enforce, so older kernels degrade to the advisory tier without error.
    if connect_tcp_port.is_some() {
        builder = builder
            .handle_access(AccessNet::ConnectTcp)
            .context("Failed to handle Landlock TCP-connect access")?;
    }
    let mut ruleset = builder
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

    // R-NET: allow outbound TCP *only* to the egress-proxy port. With this rule the
    // only `connect()` the child may make is to the local guarded proxy; every
    // other TCP connect is denied by the kernel, so a tool that ignores
    // `HTTP_PROXY` cannot reach the internet directly. Port-only (Landlock cannot
    // filter by address) and TCP-only — see the README network limits. A no-op on
    // kernels where the access was dropped above.
    if let Some(port) = connect_tcp_port {
        ruleset = ruleset
            .add_rule(NetPort::new(port, AccessNet::ConnectTcp))
            .context("Failed to add Landlock TCP-connect rule for the egress proxy")?;
    }

    Ok(ruleset)
}

/// Apply Landlock sandbox restrictions to the current thread.
///
/// SECURITY NOTE: `landlock_restrict_self(2)` restricts only the **calling
/// thread** and threads/processes created after it. Threads that already exist
/// (e.g. tokio runtime workers spawned by `#[tokio::main]` before this call)
/// remain unrestricted, and so do processes they spawn. Kernel-level
/// containment of executed commands therefore relies on the spawn-time
/// enforcement in [`landlock_ruleset_fd`] / [`apply_landlock_ruleset_in_child`];
/// this process-level call is defense-in-depth for the server itself.
#[cfg(target_os = "linux")]
pub fn enforce_landlock_sandbox(
    scopes: &[PathBuf],
    read_scopes: &[PathBuf],
    no_temp_files: bool,
    package_cache_write: bool,
) -> Result<()> {
    use anyhow::Context;

    tracing::info!("Enforcing Landlock sandbox (process level)");

    // The server process itself keeps full network: it *runs* the egress proxy,
    // which must connect out to real destinations. The TCP-connect restriction is
    // applied only to spawned children (see `landlock_ruleset_fd`).
    let ruleset = build_landlock_ruleset(
        scopes,
        read_scopes,
        no_temp_files,
        package_cache_write,
        None,
    )?;

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

/// Build a Landlock ruleset for the given scopes and return its file
/// descriptor, for spawn-time enforcement in a child process.
///
/// Because `landlock_restrict_self(2)` only restricts the calling thread,
/// process-level enforcement from inside an async runtime does not cover
/// commands spawned from pre-existing worker threads. The reliable pattern is
/// to build the ruleset in the parent (allocations are safe here) and apply it
/// in the child between `fork` and `exec` via
/// [`apply_landlock_ruleset_in_child`] — the freshly forked child is
/// single-threaded, so the restriction always covers it and everything it runs.
///
/// Returns `Ok(None)` when the kernel does not support Landlock (best-effort
/// ruleset creation yields no fd). Strict-mode server startup independently
/// fails closed via `check_sandbox_prerequisites`.
#[cfg(target_os = "linux")]
pub fn landlock_ruleset_fd(
    scopes: &[PathBuf],
    read_scopes: &[PathBuf],
    no_temp_files: bool,
    package_cache_write: bool,
    connect_tcp_port: Option<u16>,
) -> Result<Option<std::os::fd::OwnedFd>> {
    let ruleset = build_landlock_ruleset(
        scopes,
        read_scopes,
        no_temp_files,
        package_cache_write,
        connect_tcp_port,
    )?;
    Ok(ruleset.into())
}

/// Apply a previously built Landlock ruleset fd to the current (child) process.
///
/// Intended to be called from a `pre_exec` closure, after `fork` and before
/// `exec`. Only raw syscalls are used — `prctl(PR_SET_NO_NEW_PRIVS)` and
/// `landlock_restrict_self(2)` — because allocation between `fork` and `exec`
/// in a multithreaded parent is not async-signal-safe.
#[cfg(target_os = "linux")]
pub fn apply_landlock_ruleset_in_child(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    // SAFETY: prctl and the landlock syscall are async-signal-safe.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, fd, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
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
    use landlock::{AccessFs, PathBeneath, PathFd, RulesetCreatedAttr};
    if let Ok(home) = std::env::var("HOME") {
        let home_path = std::path::Path::new(&home);
        let tool_paths = [".cargo", ".rustup", ".nvm", ".npm", ".go", ".cache"];
        // Toolchain dirs hold the actual binaries (~/.cargo/bin/cargo,
        // ~/.rustup/toolchains/*/bin/rustc, ~/.nvm/versions/node/*/bin/node):
        // they need Execute in addition to read, but never write.
        let access_read_execute = access_read | AccessFs::Execute;
        for tool in &tool_paths {
            let path = home_path.join(tool);
            if path.exists()
                && let Ok(fd) = PathFd::new(&path)
            {
                let _ = ruleset.add_rule(PathBeneath::new(fd, access_read_execute));
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// The ruleset must build cleanly when a proxy port is supplied — the TCP
    /// `connect()` access is added best-effort, so on any kernel (net-capable or
    /// not) construction succeeds; it never turns into an error.
    #[test]
    fn ruleset_fd_builds_with_connect_tcp_port() {
        let dir = tempdir().unwrap();
        let scopes = vec![dir.path().to_path_buf()];
        // Some(port): the R-NET child restriction path.
        landlock_ruleset_fd(&scopes, &[], true, false, Some(34567))
            .expect("ruleset with a connect-tcp restriction must build");
        // None: the unrestricted path must keep working too.
        landlock_ruleset_fd(&scopes, &[], true, false, None)
            .expect("unrestricted ruleset must build");
    }
}
