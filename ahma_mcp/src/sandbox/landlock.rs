use anyhow::Result;

/// Build the Landlock ruleset for `policy` without applying it.
///
/// Shared by [`enforce_landlock_sandbox`] (process-level, `restrict_self`) and
/// [`landlock_ruleset_fd`] (spawn-time, applied in the child via `pre_exec`).
/// A read-only workspace (`policy.access`) is granted **read and execute
/// only**, its git storage too: the kernel half of the read-only lane (SPEC
/// R2.7.4). Everything outside the workspace (system paths, toolchain
/// profiles, temp) is granted as the policy says.
#[cfg(target_os = "linux")]
fn build_landlock_ruleset(policy: &super::KernelPolicy) -> Result<landlock::RulesetCreated> {
    use anyhow::Context;
    use landlock::{
        ABI, Access, AccessFs, AccessNet, NetPort, PathBeneath, PathFd, Ruleset, RulesetAttr,
        RulesetCreatedAttr,
    };

    // V3: V1's core access flags, plus `Refer` (V2) and `Truncate` (V3).
    //
    // Without `Refer`, Landlock refuses every rename or link that changes a
    // file's directory with EXDEV, even inside the writable scope: `mv` hides
    // it with a copy fallback, but any program calling rename(2) across
    // directories failed (ahma's own trash tests did, run through ahma on
    // Linux).
    //
    // Without `Truncate` handled, truncate(2) is not a Landlock access at all,
    // so a sandboxed command could zero any file the user can write outside
    // the scope, without ever opening it (SPEC R-HARDEN.1). Handling it also
    // gates ftruncate(2) and open(O_TRUNC), which is why every rule that grants
    // writing carries it too (`access_all`, the `/dev` sinks).
    //
    // Both are granted only where writes are (`access_all`), so they reach
    // nothing new. Best effort: on a kernel without them (Refer: before 5.19,
    // Truncate: before 6.2) they are dropped, the ruleset reports
    // `PartiallyEnforced`, and the older behaviour remains. Not V5: its
    // `IoctlDev` is unused and would report `PartiallyEnforced` on every kernel
    // before 6.10.
    let abi = ABI::V3;
    let access_all = AccessFs::from_all(abi);
    let access_read = AccessFs::from_read(abi);

    let mut builder = Ruleset::default()
        .handle_access(access_all)
        .context("Failed to create Landlock ruleset")?;
    // R-NET (child only): additionally handle TCP `connect()` so the child can be
    // confined to the egress proxy port. Best-effort by default: on kernels < 6.7
    // (Landlock ABI < V4) this access is silently dropped and only the filesystem
    // rules enforce, so older kernels degrade to the advisory tier without error.
    if policy.connect_tcp_port.is_some() {
        builder = builder
            .handle_access(AccessNet::ConnectTcp)
            .context("Failed to handle Landlock TCP-connect access")?;
    }
    let mut ruleset = builder
        .create()
        .context("Failed to create Landlock ruleset instance")?;

    // Add sandbox scopes, plus any out-of-scope git storage that has *earned* a
    // grant (a linked worktree's `<main>/.git` and `<main>/.git/worktrees/<id>`).
    //
    // `grantable_git_dirs` — not `resolve_git_dirs` — because the `.git` pointer
    // file naming those directories lives inside the workspace and is therefore
    // agent-writable. The permissive resolver is correct for deny rules and a
    // sandbox escape for allow rules: `gitdir: /` would add `/` here with
    // `AccessFs::from_all`.
    //
    // R6.1.7 / R-HANDOFF.4: Landlock ABI V1 is additive-allow with no deny rule
    // and no ordering, so a granted git dir's `hooks/` is writable from
    // `run_terminal_command` on Linux. That is the platform's already-stated
    // limit — disclosed by `profiles::platform_enforcement` — and not something
    // this ruleset can carve out (the opt-in `sandbox::deny_tier` namespace
    // holds it with a read-only mount instead). Refusing unverified dirs is
    // what bounds it.
    let scopes = &policy.workspace;
    let mut all_scopes = scopes.clone();
    let grants = super::exec_config::grantable_git_dirs(scopes, scopes);
    super::exec_config::report_refusals(&grants.refused);
    let (scope_access, granted) = if policy.access.is_writable() {
        (access_all, "read/write")
    } else {
        (access_read, "read-only")
    };
    for git_dir in grants.rule_paths() {
        if !all_scopes.contains(&git_dir) {
            tracing::warn!(
                "sandbox: granting {granted} access to git storage outside the workspace scope: {}",
                git_dir.display()
            );
            all_scopes.push(git_dir);
        }
    }
    for scope in &all_scopes {
        ruleset = ruleset
            .add_rule(PathBeneath::new(
                PathFd::new(scope).context("Failed to open sandbox scope for Landlock")?,
                scope_access,
            ))
            .context("Failed to add Landlock rule for sandbox scope")?;
    }

    // Add explicit read_scopes target files (for --livelog)
    for read_scope in &policy.read_only {
        if let Ok(fd) = PathFd::new(read_scope) {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, access_read))
                .context("Failed to add Landlock rule for read-only scope")?;
        }
    }

    add_landlock_system_rules(&mut ruleset, access_read)?;
    add_landlock_profile_rules(
        &mut ruleset,
        access_read,
        access_all,
        policy.package_cache_write,
    )?;
    add_landlock_client_config_rules(&mut ruleset, access_read)?;
    add_landlock_own_binary_rule(&mut ruleset);

    if policy.temp_writable {
        add_landlock_temp_rules(&mut ruleset, access_all)?;
    }

    // R-NET: allow outbound TCP *only* to the egress-proxy port. With this rule the
    // only `connect()` the child may make is to the local guarded proxy; every
    // other TCP connect is denied by the kernel, so a tool that ignores
    // `HTTP_PROXY` cannot reach the internet directly. Port-only (Landlock cannot
    // filter by address) and TCP-only — see the README network limits. A no-op on
    // kernels where the access was dropped above.
    if let Some(port) = policy.connect_tcp_port {
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
pub fn enforce_landlock_sandbox(policy: &super::KernelPolicy) -> Result<()> {
    use anyhow::Context;

    tracing::info!("Enforcing Landlock sandbox (process level)");
    let scopes = &policy.workspace;

    // The server process itself keeps full network: it *runs* the egress proxy,
    // which must connect out to real destinations. The TCP-connect restriction is
    // applied only to spawned children (see `landlock_ruleset_fd`).
    let ruleset = build_landlock_ruleset(&super::KernelPolicy {
        connect_tcp_port: None,
        ..policy.clone()
    })?;

    // SPEC R6.1.7: a child inherits the Landlock domain of the thread that
    // forks it, and a task inside a domain may not mount — so restricting this
    // thread would silently drop the read-only deny tier for every command
    // later spawned from it. In `linux_deny_tier = "namespace"` mode the ruleset
    // is still built (the fail-fast check above stands) but not applied here;
    // commands are confined per spawn exactly as before.
    if super::deny_tier::namespace_prevention_active() {
        tracing::info!(
            "Landlock ruleset validated; not restricting this server thread, because \
             [sandbox] linux_deny_tier = \"namespace\" needs every command ahma spawns to be \
             able to mount its read-only deny tier. Each command is still Landlock-confined \
             at spawn."
        );
        return Ok(());
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
            // The kernel lacks an access right the ruleset handles: `Refer`
            // before 5.19, `Truncate` before 6.2, TCP connect before 6.7. Say
            // which guarantee is gone rather than that something is (SPEC
            // R-HARDEN.1, invariant 2).
            tracing::warn!(
                "Landlock sandbox is PARTIALLY enforced for scopes: {:?} (status: {:?}). \
                 This kernel's Landlock lacks a right ahma asks for: before Linux 6.2 a \
                 sandboxed command can still truncate(2) a file outside the scope, before \
                 5.19 cross-directory renames fail, and before 6.7 TCP connect is not \
                 confined to the egress proxy.",
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

/// Build the Landlock ruleset for `policy` and return its file descriptor,
/// for spawn-time enforcement in a child process.
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
pub fn landlock_ruleset_fd(policy: &super::KernelPolicy) -> Result<Option<std::os::fd::OwnedFd>> {
    Ok(build_landlock_ruleset(policy)?.into())
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
    // The data sinks every Unix program may write, as the Seatbelt profile
    // already allows on macOS. `/dev` itself is read-only above, which made
    // every `git` command die at startup (`could not open '/dev/null' for
    // reading and writing`: git opens it O_RDWR to sanitize its std fds).
    // `Truncate` too: `> /dev/null` opens with O_TRUNC, which Landlock gates as
    // truncation once the ruleset handles ABI 3.
    for sink in ["/dev/null", "/dev/zero"] {
        if let Ok(fd) = PathFd::new(sink) {
            let _ = ruleset.add_rule(PathBeneath::new(
                fd,
                AccessFs::ReadFile | AccessFs::WriteFile | AccessFs::Truncate,
            ));
        }
    }
    Ok(())
}

/// ahma's own executable, read and execute, as one file (SPEC R-LINK.1): a
/// command's `BROWSER` is the link helper, a symlink to it, and it may live
/// anywhere (`~/.local/bin`, a build directory) that no system rule or
/// profile covers. Running it inside the sandbox reaches nothing new: the
/// helper inherits the command's Landlock domain, and the broker it talks to
/// checks every link. Best effort: without it the helper fails to start, as
/// any browser launch already did.
#[cfg(target_os = "linux")]
fn add_landlock_own_binary_rule(ruleset: &mut landlock::RulesetCreated) {
    use landlock::{AccessFs, PathBeneath, PathFd, RulesetCreatedAttr};
    let Some(exe) = own_binary() else {
        return;
    };
    if let Ok(fd) = PathFd::new(&exe) {
        let _ = ruleset.add_rule(PathBeneath::new(fd, AccessFs::Execute | AccessFs::ReadFile));
    }
}

/// The canonical path of the running executable: what the helper symlink
/// resolves to, and so what Landlock checks the exec against.
#[cfg(target_os = "linux")]
fn own_binary() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(dunce::canonicalize(&exe).unwrap_or(exe))
}

/// Apply the enabled sandbox **profiles** (SPEC R-PERM.5).
///
/// This used to be a hard-coded array of toolchain directories. It is now driven
/// by the shipped profile data, so the same rules are visible in
/// `ahma permissions list`, disableable via `[sandbox] profiles`, and extensible
/// by anyone whose toolchain ahma has never heard of.
///
/// The access levels are not interchangeable: a toolchain directory needs
/// `Execute` (it holds `cargo`, `rustc`, `node` — binaries the sandboxed command
/// must be able to *run*), which Landlock's read set does not include.
#[cfg(target_os = "linux")]
fn add_landlock_profile_rules(
    ruleset: &mut landlock::RulesetCreated,
    access_read: landlock::BitFlags<landlock::AccessFs>,
    access_all: landlock::BitFlags<landlock::AccessFs>,
    package_cache_write: bool,
) -> Result<()> {
    use super::profiles::{ProfileAccess, RuleKind, applicable_rules, enabled_profile_names};
    use landlock::{ABI, AccessFs, PathBeneath, PathFd, RulesetCreatedAttr};

    // Loaded once per process (see `enabled_profile_names`): sandbox
    // configuration cannot change mid-session, and this runs per spawn.
    let enabled = enabled_profile_names();
    let access_read_execute = access_read | AccessFs::Execute;

    for rule in applicable_rules(enabled, package_cache_write) {
        let access = match rule.access {
            ProfileAccess::Ro => access_read,
            ProfileAccess::Rx => access_read_execute,
            ProfileAccess::Rw => access_all,
        };
        let access = match rule.kind {
            // Landlock grants a directory's rights to everything beneath it,
            // so "the folder itself" cannot be said; and Landlock does not
            // gate `access(W_OK)`, which is what such a rule is for.
            RuleKind::Folder => continue,
            // Directory rights on a file make the rule invalid: keep the
            // ones a file can carry.
            RuleKind::File if rule.path.is_file() => access & AccessFs::from_file(ABI::V3),
            _ => access,
        };
        if let Ok(fd) = PathFd::new(&rule.path) {
            tracing::debug!(
                "Landlock: profile '{}' grants {:?} on {}",
                rule.profile,
                rule.access,
                rule.path.display()
            );
            let _ = ruleset.add_rule(PathBeneath::new(fd, access));
        }
    }

    Ok(())
}

/// git's and ssh's own configuration in home, read-only (SPEC R6.2.3): the
/// sandbox reads nothing in home by default, so git ran with no identity and
/// no credential helper, and ssh without `known_hosts`. Files get `ReadFile`
/// alone (Landlock refuses directory rights on a file); the private keys
/// beside them in `~/.ssh` stay unreadable.
#[cfg(target_os = "linux")]
fn add_landlock_client_config_rules(
    ruleset: &mut landlock::RulesetCreated,
    access_read: landlock::BitFlags<landlock::AccessFs>,
) -> Result<()> {
    use landlock::{AccessFs, BitFlags, PathBeneath, PathFd, RulesetCreatedAttr};
    let Some(home) = dirs::home_dir() else {
        return Ok(());
    };
    for path in super::credential_reads::client_config_read_paths(&home) {
        let access = if path.is_dir() {
            access_read
        } else {
            BitFlags::from(AccessFs::ReadFile)
        };
        if let Ok(fd) = PathFd::new(&path) {
            let _ = ruleset.add_rule(PathBeneath::new(fd, access));
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
    for root in super::temp_write_roots() {
        if root.exists()
            && let Ok(fd) = PathFd::new(&root)
        {
            let _ = ruleset.add_rule(PathBeneath::new(fd, access_all));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn enforce_landlock_sandbox(_policy: &super::KernelPolicy) -> Result<()> {
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::LazyLock;
    use tempfile::tempdir;

    /// The ruleset must build cleanly when a proxy port is supplied — the TCP
    /// `connect()` access is added best-effort, so on any kernel (net-capable or
    /// not) construction succeeds; it never turns into an error.
    #[test]
    fn ruleset_fd_builds_with_connect_tcp_port() {
        let dir = tempdir().unwrap();
        let scopes = vec![dir.path().to_path_buf()];
        // Some(port): the R-NET child restriction path.
        landlock_ruleset_fd(&super::super::KernelPolicy {
            workspace: scopes.clone(),
            access: super::super::WorkspaceAccess::ReadWrite,
            read_only: Vec::new(),
            temp_writable: false,
            package_cache_write: false,
            connect_tcp_port: Some(34567),
        })
        .expect("ruleset with a connect-tcp restriction must build");
        // None: the unrestricted path must keep working too.
        landlock_ruleset_fd(&super::super::KernelPolicy {
            workspace: scopes.clone(),
            access: super::super::WorkspaceAccess::ReadWrite,
            read_only: Vec::new(),
            temp_writable: false,
            package_cache_write: false,
            connect_tcp_port: None,
        })
        .expect("unrestricted ruleset must build");
    }

    /// The `scopes` loop in `build_landlock_ruleset` must handle both the
    /// empty case (nothing granted beyond system/home rules) and multiple
    /// scopes (each gets its own `PathBeneath` rule) without erroring.
    #[test]
    fn ruleset_fd_builds_with_multiple_and_empty_scopes() {
        // Empty scopes: the `for scope in scopes` loop body never runs, but
        // construction must still succeed (system/home rules are unaffected).
        landlock_ruleset_fd(&super::super::KernelPolicy {
            workspace: Vec::new(),
            access: super::super::WorkspaceAccess::ReadWrite,
            read_only: Vec::new(),
            temp_writable: false,
            package_cache_write: false,
            connect_tcp_port: None,
        })
        .expect("empty scopes must still build a valid ruleset");

        // Multiple scopes: exercise more than one loop iteration.
        let dir_a = tempdir().unwrap();
        let dir_b = tempdir().unwrap();
        let scopes = vec![dir_a.path().to_path_buf(), dir_b.path().to_path_buf()];
        landlock_ruleset_fd(&super::super::KernelPolicy {
            workspace: scopes.clone(),
            access: super::super::WorkspaceAccess::ReadWrite,
            read_only: Vec::new(),
            temp_writable: false,
            package_cache_write: false,
            connect_tcp_port: None,
        })
        .expect("multiple scopes must build a valid ruleset");
    }

    /// Explicit `read_scopes` (used by `--livelog`) must get a read-only
    /// `PathBeneath` rule when the path exists, and must be silently skipped
    /// (not error the whole build) when `PathFd::new` fails for a missing path.
    #[test]
    fn ruleset_fd_grants_explicit_read_scope_and_skips_missing_ones() {
        let dir = tempdir().unwrap();
        let scopes = vec![dir.path().to_path_buf()];

        let readable_dir = tempdir().unwrap();
        let missing_path = readable_dir.path().join("does-not-exist-xyz");

        // One real read-only target plus one path that doesn't exist:
        // `PathFd::new` fails for the missing one and it must be skipped
        // rather than erroring the whole ruleset build.
        let read_scopes = vec![readable_dir.path().to_path_buf(), missing_path];

        landlock_ruleset_fd(&super::super::KernelPolicy {
            workspace: scopes.clone(),
            access: super::super::WorkspaceAccess::ReadWrite,
            read_only: read_scopes.clone(),
            temp_writable: false,
            package_cache_write: false,
            connect_tcp_port: None,
        })
        .expect("ruleset with a valid + missing read scope must still build");
    }

    /// When `no_temp_files` is false, `add_landlock_temp_rules` must run and
    /// grant `/tmp` access (it always exists on Linux) without erroring. The
    /// existing test above only ever passes `no_temp_files: true`, so this
    /// exercises the opposite branch of `build_landlock_ruleset`'s
    /// `if !no_temp_files { ... }` guard.
    #[test]
    fn ruleset_fd_grants_temp_access_when_not_disabled() {
        let dir = tempdir().unwrap();
        let scopes = vec![dir.path().to_path_buf()];

        landlock_ruleset_fd(&super::super::KernelPolicy {
            workspace: scopes.clone(),
            access: super::super::WorkspaceAccess::ReadWrite,
            read_only: Vec::new(),
            temp_writable: true,
            package_cache_write: false,
            connect_tcp_port: None,
        })
        .expect("ruleset with temp files enabled must build");
    }

    /// Serialize tests in this module that mutate process-wide `HOME` /
    /// `CARGO_HOME` env vars, mirroring the pattern in
    /// `sandbox::pkg_cache::tests` (see that module for rationale).
    static LANDLOCK_ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Sets an env var for the duration of a test and restores the previous
    /// value (or removes it) on drop, regardless of pass/fail/panic.
    struct EnvVarRestore {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvVarRestore {
        /// SAFETY: caller must hold `LANDLOCK_ENV_MUTEX` for the lifetime of
        /// the returned guard.
        fn set(key: &'static str, value: &std::path::Path) -> Self {
            let original = std::env::var(key).ok();
            // SAFETY: test-only; serialized by `LANDLOCK_ENV_MUTEX`.
            unsafe { std::env::set_var(key, value) };
            Self { key, original }
        }
    }

    impl Drop for EnvVarRestore {
        fn drop(&mut self) {
            // SAFETY: test-only; serialized by `LANDLOCK_ENV_MUTEX`.
            unsafe {
                match &self.original {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    /// `add_landlock_package_cache_write_rules` must pre-create the cargo
    /// cache dirs/files (so `PathFd::new` can succeed) and grant write rules
    /// for each, when `package_cache_write` is enabled and `CARGO_HOME`
    /// points at an existing directory.
    #[test]
    fn ruleset_fd_grants_package_cache_write_rules_when_cache_exists() {
        let _guard = LANDLOCK_ENV_MUTEX.lock();
        let cargo_home_dir = tempdir().unwrap();
        let _restore = EnvVarRestore::set("CARGO_HOME", cargo_home_dir.path());

        let dir = tempdir().unwrap();
        let scopes = vec![dir.path().to_path_buf()];

        landlock_ruleset_fd(&super::super::KernelPolicy {
            workspace: scopes.clone(),
            access: super::super::WorkspaceAccess::ReadWrite,
            read_only: Vec::new(),
            temp_writable: false,
            package_cache_write: true,
            connect_tcp_port: None,
        })
        .expect("ruleset with package_cache_write=true must build");

        // `pre_create_package_cache_paths` must have created the writable
        // dirs/files so the Landlock `PathFd::new()` calls could succeed.
        assert!(cargo_home_dir.path().join("registry").is_dir());
        assert!(cargo_home_dir.path().join("git").is_dir());
        assert!(cargo_home_dir.path().join(".package-cache").is_file());
        assert!(
            cargo_home_dir
                .path()
                .join(".package-cache-mutate")
                .is_file()
        );
    }

    /// `add_landlock_profile_rules` must grant read+execute access to the
    /// toolchain dirs an enabled profile names (e.g. `~/.cargo`) when they exist.
    ///
    /// Read+execute, not read: `~/.cargo/bin/cargo` is a binary the sandboxed
    /// command has to *run*, and Landlock's read set does not include `Execute`.
    #[test]
    fn ruleset_fd_grants_profile_rules_when_home_has_toolchain_dirs() {
        let _guard = LANDLOCK_ENV_MUTEX.lock();
        let home_dir = tempdir().unwrap();
        std::fs::create_dir_all(home_dir.path().join(".cargo")).unwrap();
        std::fs::create_dir_all(home_dir.path().join(".rustup")).unwrap();
        let _restore = EnvVarRestore::set("HOME", home_dir.path());

        let dir = tempdir().unwrap();
        let scopes = vec![dir.path().to_path_buf()];

        landlock_ruleset_fd(&super::super::KernelPolicy {
            workspace: scopes.clone(),
            access: super::super::WorkspaceAccess::ReadWrite,
            read_only: Vec::new(),
            temp_writable: false,
            package_cache_write: false,
            connect_tcp_port: None,
        })
        .expect("ruleset must build when HOME has toolchain dirs present");
    }

    /// Passing an invalid ruleset fd must fail, not silently succeed.
    /// `prctl(PR_SET_NO_NEW_PRIVS)` succeeds unconditionally (it only blocks
    /// future privilege escalation, it does not restrict filesystem access),
    /// but the `landlock_restrict_self` syscall must reject a bogus fd, so no
    /// enforcement is actually applied to this test's thread — this exercises
    /// the error path without depending on Landlock being available/working
    /// on the machine running the test.
    #[test]
    fn apply_landlock_ruleset_in_child_fails_with_invalid_fd() {
        let result = apply_landlock_ruleset_in_child(-1);
        assert!(
            result.is_err(),
            "applying an invalid ruleset fd must return an error, not succeed silently"
        );
    }

    /// SPEC R-LINK.1: a confined command may run ahma's own binary — the
    /// link helper `BROWSER` names — wherever it was installed. Run here as
    /// this test binary, which lives in the build directory, outside the
    /// scope and every system path: without the rule the exec is refused.
    #[test]
    fn a_confined_command_may_run_ahmas_own_binary() {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        let dir = tempdir().unwrap();
        let Some(fd) = landlock_ruleset_fd(&super::super::KernelPolicy {
            workspace: vec![dir.path().to_path_buf()],
            access: super::super::WorkspaceAccess::ReadWrite,
            read_only: Vec::new(),
            temp_writable: false,
            package_cache_write: false,
            connect_tcp_port: None,
        })
        .expect("ruleset builds") else {
            eprintln!("skipped: this kernel has no Landlock");
            return;
        };
        let raw = fd.as_raw_fd();
        let mut cmd = std::process::Command::new(own_binary().expect("this binary"));
        // libtest's `--list` prints the test names and exits 0.
        cmd.args(["--list", "--format", "terse"]);
        // SAFETY: `apply_landlock_ruleset_in_child` makes only raw syscalls.
        unsafe { cmd.pre_exec(move || apply_landlock_ruleset_in_child(raw)) };
        let out = cmd.output().expect("ahma's own binary runs confined");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
