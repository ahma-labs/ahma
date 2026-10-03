//! Kernel prevention of the trust-handoff deny tier on Linux, opt-in
//! (`[sandbox] linux_deny_tier = "namespace"`, SPEC R6.1.7, R-HANDOFF.4).
//!
//! The deny tier — every resolved `<git_dir>/hooks` and the workspace's own
//! `.ahma` ([`super::exec_config::deny_write_globs`]) — is a hole *inside* the
//! writable workspace. Landlock cannot subtract it: it grants per hierarchy and
//! has no deny rule. So by default ahma only **detects** a write to it
//! ([`super::handoff_watch`]).
//!
//! In `namespace` mode each command additionally runs in a user and mount
//! namespace of its own, in which every deny-tier path that exists when the
//! command starts is bind-mounted onto itself read-only. A write there fails
//! with `EROFS` ("Read-only file system"), and the rest of the workspace is
//! untouched. The steps run in the child between `fork` and `exec`, **before**
//! Landlock: a Landlock-restricted task may not change its mount topology, which
//! is also what keeps the child — root of nothing but its own namespace — from
//! unmounting the overlay afterwards.
//!
//! ## Availability, and honest fallback
//!
//! Unprivileged user namespaces are refused on many hosts: stock Ubuntu 23.10+
//! (`kernel.apparmor_restrict_unprivileged_userns = 1` creates the namespace but
//! grants nothing in it), Docker's default seccomp profile, and any process
//! already inside a Landlock domain (a nested ahma) cannot mount. `unshare` cannot
//! be undone, so trying per command and failing half-way would leave a child
//! running as the overflow uid. Instead [`availability`] forks one probe child
//! per process that performs the whole sequence on a temporary directory and
//! checks that a write really fails with `EROFS`; the verdict is cached. Where it
//! fails, nothing is attempted per command, the reason is logged once at `warn`
//! ([`disclose_at_startup`]) and shown on every R-PERM.5.1 surface through
//! [`super::profiles::platform_enforcement`], and detection carries on alone.
//!
//! Detection stays on in `namespace` mode too: a path created *during* a command
//! (a `git init`, a first `.ahma/`) is not mounted until the next one, and
//! renaming an ancestor (`mv .git .git.old`) sidesteps any path-based rule — on
//! macOS as here.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

pub use ahma_common::config::LinuxDenyTier;

static MODE: AtomicU8 = AtomicU8::new(0);

/// Install the resolved `[sandbox] linux_deny_tier`. Process-global like the
/// other sandbox toggles: set once at startup, read at every spawn.
pub fn set_linux_deny_tier(mode: LinuxDenyTier) {
    let v = match mode {
        LinuxDenyTier::Detect => 0,
        LinuxDenyTier::Namespace => 1,
    };
    MODE.store(v, Ordering::Relaxed);
}

/// The installed `[sandbox] linux_deny_tier`.
pub fn linux_deny_tier() -> LinuxDenyTier {
    match MODE.load(Ordering::Relaxed) {
        1 => LinuxDenyTier::Namespace,
        _ => LinuxDenyTier::Detect,
    }
}

/// Why kernel prevention of the deny tier is unavailable in this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// Not Linux: macOS already holds the tier at the kernel, Windows has no
    /// filesystem boundary.
    NotLinux,
    /// This process is already inside a Landlock domain (an outer ahma, or a
    /// command run through one), which forbids mounts.
    AlreadyConfined,
    /// `unshare(CLONE_NEWUSER)` itself was refused.
    UserNamespacesDenied,
    /// The namespace was created but holds no privilege: the id maps could not
    /// be written (Ubuntu's AppArmor restriction).
    IdMapDenied,
    /// The namespace and its id maps worked, a mount did not.
    MountDenied,
    /// Everything succeeded and a write still went through.
    NotEffective,
    /// The probe itself could not run (fork, pipe or temp dir failed).
    ProbeFailed,
}

impl Unavailable {
    /// One disclosure line: what is not enforced, why, and what to do.
    pub fn note(self) -> &'static str {
        match self {
            Self::NotLinux => {
                "[sandbox] linux_deny_tier = \"namespace\" has no effect on this platform: macOS \
                 Seatbelt already refuses writes to git hook directories and .ahma/ at the kernel, \
                 and Windows has no filesystem boundary to hold them with."
            }
            Self::AlreadyConfined => {
                "Linux: [sandbox] linux_deny_tier = \"namespace\" is set, but this ahma already runs \
                 inside a Landlock sandbox (an outer ahma, or a command run through one), which \
                 forbids the mounts it needs. Writes to git hook directories and .ahma/ are detected \
                 and reported as TRUST-HANDOFF WRITE, not prevented; run ahma outside the other \
                 sandbox to get prevention."
            }
            Self::UserNamespacesDenied => {
                "Linux: [sandbox] linux_deny_tier = \"namespace\" is set, but this kernel refuses \
                 unprivileged user namespaces (a container's seccomp profile, Docker's default \
                 among them; an AppArmor policy such as Ubuntu's \
                 kernel.apparmor_restrict_unprivileged_unconfined; or user.max_user_namespaces = \
                 0). Writes to git hook directories and \
                 .ahma/ are detected and reported as TRUST-HANDOFF WRITE, not prevented; allow user \
                 namespaces for the user running ahma to get prevention."
            }
            Self::IdMapDenied => {
                "Linux: [sandbox] linux_deny_tier = \"namespace\" is set, but user namespaces created \
                 here carry no privilege: Ubuntu 23.10+ does this to every program without an \
                 AppArmor profile that allows `userns` (kernel.apparmor_restrict_unprivileged_userns \
                 = 1). Writes to git hook directories and .ahma/ are detected and reported as \
                 TRUST-HANDOFF WRITE, not prevented; an administrator can install such a profile for \
                 ahma, or set that sysctl to 0, to get prevention."
            }
            Self::MountDenied => {
                "Linux: [sandbox] linux_deny_tier = \"namespace\" is set, but a mount inside a new user \
                 namespace was refused (an AppArmor or seccomp policy). Writes to git hook \
                 directories and .ahma/ are detected and reported as TRUST-HANDOFF WRITE, not \
                 prevented; `dmesg` names the policy that refused it (look for operation=\"mount\")."
            }
            Self::NotEffective => {
                "Linux: [sandbox] linux_deny_tier = \"namespace\" is set, but in ahma's startup probe a \
                 read-only bind mount did not stop a write, so ahma does not rely on it. Writes to git \
                 hook directories and .ahma/ are detected and reported as TRUST-HANDOFF WRITE, not \
                 prevented; please report this with your kernel version."
            }
            Self::ProbeFailed => {
                "Linux: [sandbox] linux_deny_tier = \"namespace\" is set, but ahma could not run the \
                 startup probe that decides whether it works here (fork, pipe or a temporary \
                 directory failed). Writes to git hook directories and .ahma/ are detected and \
                 reported as TRUST-HANDOFF WRITE, not prevented; restart ahma to probe again."
            }
        }
    }
}

/// Whether a per-command namespace can hold the deny tier in this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// The probe made a bind read-only and saw a write fail with `EROFS`.
    Available,
    /// It could not; `errno` is the failing step's, for the log.
    Unavailable { reason: Unavailable, errno: i32 },
}

/// How the deny tier is held for commands in this process: the installed mode
/// joined with the probe's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyTierState {
    /// `linux_deny_tier = "detect"`: detection only.
    Detect,
    /// `linux_deny_tier = "namespace"` and the probe passed: prevention for
    /// existing paths, detection for the rest.
    Namespace,
    /// `linux_deny_tier = "namespace"` asked for, unavailable: detection only.
    NamespaceUnavailable(Unavailable),
}

/// The probe's verdict, computed once per process.
///
/// Run it before this process restricts any thread of its own with Landlock
/// (a restricted thread cannot mount, so a probe forked from one reports
/// [`Unavailable::AlreadyConfined`]); the server does, from
/// [`disclose_at_startup`].
pub fn availability() -> Availability {
    static VERDICT: OnceLock<Availability> = OnceLock::new();
    *VERDICT.get_or_init(|| {
        #[cfg(target_os = "linux")]
        {
            linux::probe()
        }
        #[cfg(not(target_os = "linux"))]
        {
            Availability::Unavailable {
                reason: Unavailable::NotLinux,
                errno: 0,
            }
        }
    })
}

/// The installed mode joined with [`availability`]. Probes only when the mode
/// asks for namespaces.
pub fn state() -> DenyTierState {
    match linux_deny_tier() {
        LinuxDenyTier::Detect => DenyTierState::Detect,
        LinuxDenyTier::Namespace => match availability() {
            Availability::Available => DenyTierState::Namespace,
            Availability::Unavailable { reason, .. } => DenyTierState::NamespaceUnavailable(reason),
        },
    }
}

/// Whether commands spawned now get the read-only namespace.
pub fn namespace_prevention_active() -> bool {
    state() == DenyTierState::Namespace
}

/// Probe (when asked for) and say once, in the log, how the deny tier is held.
/// Call it before the process-level Landlock restriction.
pub fn disclose_at_startup() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| match linux_deny_tier() {
        LinuxDenyTier::Detect => tracing::debug!(
            "Trust-handoff deny tier: detection only ([sandbox] linux_deny_tier = \"detect\", SPEC R6.1.7)"
        ),
        LinuxDenyTier::Namespace => match availability() {
            Availability::Available => tracing::info!(
                "Trust-handoff deny tier: kernel prevention on ([sandbox] linux_deny_tier = \
                 \"namespace\"). Each command runs in its own user and mount namespace with every \
                 existing git hooks directory and the workspace's .ahma/ mounted read-only; \
                 detection stays on for paths created during a command (SPEC R6.1.7)."
            ),
            Availability::Unavailable {
                reason: Unavailable::NotLinux,
                ..
            } => tracing::info!("{}", Unavailable::NotLinux.note()),
            Availability::Unavailable { reason, errno } => {
                tracing::warn!(errno, "{}", reason.note())
            }
        },
    });
}

/// The deny-tier paths a command run from `roots` gets read-only: the same set,
/// from the same function, as the macOS kernel rules and the detection inventory
/// (`exec_config::deny_write_globs`, under the installed escape hatches), but
/// only the ones that **exist** — a bind needs a mount point, and a path created
/// during the command is left to detection. Canonical, sorted, de-duplicated.
pub fn mount_targets(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for root in roots {
        let root = dunce::canonicalize(root).unwrap_or_else(|_| root.clone());
        let git_dirs = super::exec_config::resolve_git_dirs(&root);
        for target in super::exec_config::deny_write_globs(&root, &git_dirs) {
            if let Ok(target) = dunce::canonicalize(&target)
                && !out.contains(&target)
            {
                out.push(target);
            }
        }
    }
    out.sort();
    out
}

/// The steps the child takes, in order. The discriminant travels from the
/// probe child to the parent over a pipe; `0` means every step held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
#[cfg_attr(
    not(target_os = "linux"),
    allow(
        dead_code,
        reason = "type-checked and unit-tested everywhere, used only on Linux"
    )
)]
enum Step {
    /// `open("/proc/self/uid_map", O_WRONLY)` before `unshare`: an inherited
    /// Landlock domain refuses it, and would refuse every mount after it.
    Precheck = 1,
    Unshare = 2,
    Setgroups = 3,
    UidMap = 4,
    GidMap = 5,
    MakePrivate = 6,
    Bind = 7,
    Remount = 8,
    /// `chdir` back into the working directory, so a cwd inside a mounted
    /// target resolves through the read-only mount, not the one it pinned.
    Rechdir = 9,
    /// Probe only: a write under the read-only bind must fail with `EROFS`.
    Verify = 10,
}

/// The verdict for a probe child's report: the failing step and its errno, or
/// `(0, _)` when everything held.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "unit-tested everywhere, used only on Linux")
)]
fn verdict(step: i32, errno: i32) -> Availability {
    let reason = match step {
        0 => return Availability::Available,
        s if s == Step::Precheck as i32 => Unavailable::AlreadyConfined,
        s if s == Step::Unshare as i32 => Unavailable::UserNamespacesDenied,
        s if s == Step::Setgroups as i32
            || s == Step::UidMap as i32
            || s == Step::GidMap as i32 =>
        {
            Unavailable::IdMapDenied
        }
        s if s == Step::MakePrivate as i32
            || s == Step::Bind as i32
            || s == Step::Remount as i32
            || s == Step::Rechdir as i32 =>
        {
            Unavailable::MountDenied
        }
        s if s == Step::Verify as i32 => Unavailable::NotEffective,
        _ => Unavailable::ProbeFailed,
    };
    Availability::Unavailable { reason, errno }
}

#[cfg(target_os = "linux")]
pub use linux::DenyTierMounts;

#[cfg(target_os = "linux")]
mod linux {
    use super::{Availability, Step, Unavailable, verdict};
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    const UID_MAP: &CStr = c"/proc/self/uid_map";
    const GID_MAP: &CStr = c"/proc/self/gid_map";
    const SETGROUPS: &CStr = c"/proc/self/setgroups";
    const ROOT: &CStr = c"/";

    /// A failed step and the errno it left.
    #[derive(Debug, Clone, Copy)]
    struct Failure {
        step: Step,
        errno: i32,
    }

    impl Failure {
        /// Capture `errno` for `step`. Reading errno allocates nothing, so this
        /// is safe between `fork` and `exec`.
        fn last(step: Step) -> Self {
            Self {
                step,
                errno: std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EIO),
            }
        }
    }

    /// One deny-tier path, ready to bind read-only.
    #[derive(Debug)]
    struct MountTarget {
        path: CString,
        /// `MS_BIND | MS_REMOUNT | MS_RDONLY | MS_NOSUID | MS_NODEV`, plus
        /// `MS_NOEXEC` only where the underlying mount already has it.
        ///
        /// A remount in a user namespace may not clear a flag the original
        /// mount was locked with (`nosuid`, `nodev`, `noexec`, the atime mode)
        /// or it fails with `EPERM`. Adding `nosuid`/`nodev` is always allowed
        /// and is what a hooks directory wants anyway; `noexec` is preserved,
        /// never added, because `git commit` run *by the command* must still be
        /// able to execute `.git/hooks/pre-commit` (with `noexec`, git skips a
        /// hook it cannot execute, so a repository's own guard would silently
        /// stop running). No atime flag is passed: a bind remount without one
        /// keeps the mount's own.
        remount_flags: libc::c_ulong,
    }

    impl MountTarget {
        /// `None` when the path does not exist (nothing to mount on) or cannot
        /// be named to the kernel.
        fn plan(path: &Path) -> Option<Self> {
            let c = CString::new(path.as_os_str().as_bytes()).ok()?;
            // SAFETY: zeroed is a valid bit pattern for this plain C struct.
            let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
            // SAFETY: `c` is NUL-terminated and `st` a valid out-pointer.
            if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
                return None;
            }
            let mut flags = libc::MS_BIND
                | libc::MS_REMOUNT
                | libc::MS_RDONLY
                | libc::MS_NOSUID
                | libc::MS_NODEV;
            if st.f_flag & libc::ST_NOEXEC != 0 {
                flags |= libc::MS_NOEXEC;
            }
            Some(Self {
                path: c,
                remount_flags: flags,
            })
        }
    }

    /// Everything a child needs to enter its read-only namespace, computed in
    /// the parent: the steps between `fork` and `exec` may not allocate.
    #[derive(Debug)]
    pub struct DenyTierMounts {
        uid_map: Vec<u8>,
        gid_map: Vec<u8>,
        targets: Vec<MountTarget>,
        cwd: Option<CString>,
    }

    impl DenyTierMounts {
        /// Plan the read-only binds for `paths` (see [`super::mount_targets`]).
        /// `None` when none of them exists — then there is nothing to protect
        /// and the command spawns exactly as in `detect` mode.
        pub fn plan(paths: &[PathBuf], cwd: Option<&Path>) -> Option<Self> {
            let targets: Vec<MountTarget> =
                paths.iter().filter_map(|p| MountTarget::plan(p)).collect();
            if targets.is_empty() {
                return None;
            }
            // SAFETY: geteuid/getegid cannot fail.
            let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
            Some(Self {
                // The one mapping an unprivileged process may write for itself:
                // its own effective id to itself. Everything else shows as the
                // overflow id ("nobody") inside the namespace.
                uid_map: format!("{uid} {uid} 1\n").into_bytes(),
                gid_map: format!("{gid} {gid} 1\n").into_bytes(),
                targets,
                cwd: cwd.and_then(|p| CString::new(p.as_os_str().as_bytes()).ok()),
            })
        }

        /// The paths this plan makes read-only, for logs and tests.
        pub fn paths(&self) -> Vec<PathBuf> {
            use std::os::unix::ffi::OsStringExt;
            self.targets
                .iter()
                .map(|t| PathBuf::from(std::ffi::OsString::from_vec(t.path.as_bytes().to_vec())))
                .collect()
        }

        /// Enter the namespace from a `pre_exec` closure, before Landlock.
        ///
        /// A child that cannot even start — already inside a Landlock domain, or
        /// `unshare` refused — runs **without** the namespace: the command must
        /// not fail because prevention is unavailable to it, and detection still
        /// reports what it writes. A failure *after* `unshare`, which the startup
        /// probe showed does not happen on this host, fails the spawn instead:
        /// the child would otherwise run as the overflow uid, or with a planned
        /// mount silently missing.
        ///
        /// Async-signal-safe: raw syscalls on buffers allocated before `fork`.
        pub fn enter_in_child(&self) -> std::io::Result<()> {
            match self.enter() {
                Ok(()) => Ok(()),
                Err(f) if matches!(f.step, Step::Precheck | Step::Unshare) => Ok(()),
                Err(f) => Err(std::io::Error::from_raw_os_error(f.errno)),
            }
        }

        fn enter(&self) -> Result<(), Failure> {
            // SAFETY (every block below): raw syscalls whose pointer arguments
            // are NUL-terminated `CStr`s or slices owned by `self`, allocated
            // before `fork`; nothing here allocates or takes a lock.
            let fd = unsafe { libc::open(UID_MAP.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
            if fd < 0 {
                return Err(Failure::last(Step::Precheck));
            }
            unsafe { libc::close(fd) };
            if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) } != 0 {
                return Err(Failure::last(Step::Unshare));
            }
            write_proc(SETGROUPS, b"deny", Step::Setgroups)?;
            write_proc(UID_MAP, &self.uid_map, Step::UidMap)?;
            write_proc(GID_MAP, &self.gid_map, Step::GidMap)?;
            // Nothing done below may propagate back to the parent namespace.
            let rc = unsafe {
                libc::mount(
                    std::ptr::null(),
                    ROOT.as_ptr(),
                    std::ptr::null(),
                    libc::MS_REC | libc::MS_PRIVATE,
                    std::ptr::null(),
                )
            };
            if rc != 0 {
                return Err(Failure::last(Step::MakePrivate));
            }
            for target in &self.targets {
                // Recursive: a non-recursive bind of a directory holding a mount
                // inherited from the parent namespace is refused (EINVAL).
                let rc = unsafe {
                    libc::mount(
                        target.path.as_ptr(),
                        target.path.as_ptr(),
                        std::ptr::null(),
                        libc::MS_BIND | libc::MS_REC,
                        std::ptr::null(),
                    )
                };
                if rc != 0 {
                    let failure = Failure::last(Step::Bind);
                    // Removed since the plan: nothing left to protect.
                    if failure.errno == libc::ENOENT {
                        continue;
                    }
                    return Err(failure);
                }
                let rc = unsafe {
                    libc::mount(
                        std::ptr::null(),
                        target.path.as_ptr(),
                        std::ptr::null(),
                        target.remount_flags,
                        std::ptr::null(),
                    )
                };
                if rc != 0 {
                    return Err(Failure::last(Step::Remount));
                }
            }
            if let Some(cwd) = &self.cwd
                && unsafe { libc::chdir(cwd.as_ptr()) } != 0
            {
                return Err(Failure::last(Step::Rechdir));
            }
            Ok(())
        }
    }

    /// Write `data` to a `/proc/self` control file in one `write(2)`, as the
    /// kernel requires for id maps.
    fn write_proc(path: &CStr, data: &[u8], step: Step) -> Result<(), Failure> {
        // SAFETY: NUL-terminated path; `data` is a live slice.
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(Failure::last(step));
        }
        let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        let result = if n < 0 {
            Err(Failure::last(step))
        } else if n as usize != data.len() {
            Err(Failure {
                step,
                errno: libc::EIO,
            })
        } else {
            Ok(())
        };
        unsafe { libc::close(fd) };
        result
    }

    /// Fork a child that runs the whole sequence on a temporary directory and
    /// tries to create a file under the read-only bind. Only `EROFS` from that
    /// create counts as available.
    pub(super) fn probe() -> Availability {
        probe_inner().unwrap_or(Availability::Unavailable {
            reason: Unavailable::ProbeFailed,
            errno: 0,
        })
    }

    fn probe_inner() -> std::io::Result<Availability> {
        let dir = tempfile::Builder::new()
            .prefix("ahma-deny-tier-probe-")
            .tempdir()?;
        let target = dir.path().join("hooks");
        std::fs::create_dir(&target)?;
        let target = dunce::canonicalize(&target)?;
        let Some(mounts) = DenyTierMounts::plan(std::slice::from_ref(&target), None) else {
            return Ok(verdict(-1, 0));
        };
        let probe_file = CString::new(target.join("probe").as_os_str().as_bytes())
            .map_err(std::io::Error::other)?;

        let mut fds: [libc::c_int; 2] = [0; 2];
        // SAFETY: `fds` is a valid two-element out-array.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let [read_end, write_end] = fds;

        // SAFETY: the child performs only async-signal-safe syscalls on memory
        // allocated above, then `_exit`s without running destructors.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            let err = std::io::Error::last_os_error();
            unsafe {
                libc::close(read_end);
                libc::close(write_end);
            }
            return Err(err);
        }
        if pid == 0 {
            let mode: libc::c_uint = 0o600;
            let (step, errno) = match mounts.enter() {
                Err(f) => (f.step as i32, f.errno),
                Ok(()) => {
                    let fd = unsafe {
                        libc::open(
                            probe_file.as_ptr(),
                            libc::O_WRONLY | libc::O_CREAT | libc::O_CLOEXEC,
                            mode,
                        )
                    };
                    if fd >= 0 {
                        unsafe { libc::close(fd) };
                        (Step::Verify as i32, 0)
                    } else {
                        let errno = Failure::last(Step::Verify).errno;
                        if errno == libc::EROFS {
                            (0, 0)
                        } else {
                            (Step::Verify as i32, errno)
                        }
                    }
                }
            };
            let mut report = [0u8; 8];
            report[..4].copy_from_slice(&step.to_ne_bytes());
            report[4..].copy_from_slice(&errno.to_ne_bytes());
            unsafe {
                libc::write(write_end, report.as_ptr().cast(), report.len());
                libc::_exit(0);
            }
        }

        unsafe { libc::close(write_end) };
        let mut report = [0u8; 8];
        let mut got = 0usize;
        while got < report.len() {
            // SAFETY: writing into the unfilled tail of a live buffer.
            let n = unsafe {
                libc::read(
                    read_end,
                    report[got..].as_mut_ptr().cast(),
                    report.len() - got,
                )
            };
            if n > 0 {
                got += n as usize;
            } else if n < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            } else {
                break;
            }
        }
        unsafe { libc::close(read_end) };
        let mut status: libc::c_int = 0;
        // SAFETY: reaping our own child.
        while unsafe { libc::waitpid(pid, &mut status, 0) } < 0 {
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                break;
            }
        }
        if got != report.len() {
            return Ok(verdict(-1, 0));
        }
        let step = i32::from_ne_bytes([report[0], report[1], report[2], report[3]]);
        let errno = i32::from_ne_bytes([report[4], report[5], report[6], report[7]]);
        Ok(verdict(step, errno))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Restores `detect` however the test ends: the mode is process-global.
    struct ModeGuard;

    impl ModeGuard {
        fn set(mode: LinuxDenyTier) -> Self {
            set_linux_deny_tier(mode);
            Self
        }
    }

    impl Drop for ModeGuard {
        fn drop(&mut self) {
            set_linux_deny_tier(LinuxDenyTier::Detect);
        }
    }

    #[test]
    fn deny_tier_defaults_to_detection_and_never_probes_for_it() {
        let _mode = ModeGuard::set(LinuxDenyTier::Detect);
        assert_eq!(linux_deny_tier(), LinuxDenyTier::Detect);
        assert_eq!(state(), DenyTierState::Detect);
        assert!(!namespace_prevention_active());
    }

    #[test]
    fn deny_tier_mode_round_trips_through_the_process_global() {
        let _mode = ModeGuard::set(LinuxDenyTier::Namespace);
        assert_eq!(linux_deny_tier(), LinuxDenyTier::Namespace);
        set_linux_deny_tier(LinuxDenyTier::Detect);
        assert_eq!(linux_deny_tier(), LinuxDenyTier::Detect);
    }

    /// Off Linux the setting can be written but never takes effect, and the
    /// state says so instead of claiming prevention.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn deny_tier_namespace_is_unavailable_off_linux() {
        let _mode = ModeGuard::set(LinuxDenyTier::Namespace);
        assert_eq!(
            state(),
            DenyTierState::NamespaceUnavailable(Unavailable::NotLinux)
        );
        assert!(!namespace_prevention_active());
    }

    /// Each step of the sequence maps to the reason a human can act on; a
    /// clean report is the only way to "available".
    #[test]
    fn deny_tier_probe_steps_map_to_actionable_reasons() {
        assert_eq!(verdict(0, 0), Availability::Available);
        // EPERM's value on every Unix; a literal so this runs on Windows too.
        const EPERM: i32 = 1;
        let reason = |step: Step| match verdict(step as i32, EPERM) {
            Availability::Unavailable { reason, errno } => {
                assert_eq!(errno, EPERM, "the errno travels with the reason");
                reason
            }
            Availability::Available => panic!("{step:?} failed, so not available"),
        };
        assert_eq!(reason(Step::Precheck), Unavailable::AlreadyConfined);
        assert_eq!(reason(Step::Unshare), Unavailable::UserNamespacesDenied);
        for step in [Step::Setgroups, Step::UidMap, Step::GidMap] {
            assert_eq!(reason(step), Unavailable::IdMapDenied, "{step:?}");
        }
        for step in [Step::MakePrivate, Step::Bind, Step::Remount, Step::Rechdir] {
            assert_eq!(reason(step), Unavailable::MountDenied, "{step:?}");
        }
        assert_eq!(reason(Step::Verify), Unavailable::NotEffective);
        assert!(matches!(
            verdict(-1, 0),
            Availability::Unavailable {
                reason: Unavailable::ProbeFailed,
                ..
            }
        ));
    }

    /// R-PERM.5.1: a disclosure says what is not enforced and what to do. Every
    /// Linux reason names the fallback the user is actually getting.
    #[test]
    fn deny_tier_unavailable_notes_name_the_fallback_and_a_remedy() {
        for reason in [
            Unavailable::AlreadyConfined,
            Unavailable::UserNamespacesDenied,
            Unavailable::IdMapDenied,
            Unavailable::MountDenied,
            Unavailable::NotEffective,
            Unavailable::ProbeFailed,
        ] {
            let note = reason.note();
            assert!(note.starts_with("Linux:"), "{reason:?}: {note}");
            assert!(note.contains("linux_deny_tier"), "{reason:?}: {note}");
            assert!(
                note.contains("TRUST-HANDOFF WRITE") && note.contains("not prevented"),
                "{reason:?} must say detection is what remains: {note}"
            );
        }
        assert!(
            Unavailable::IdMapDenied
                .note()
                .contains("apparmor_restrict_unprivileged_userns"),
            "the common Ubuntu case must name the knob"
        );
    }

    /// The bind targets are the deny tier's *existing* paths: a missing
    /// `.ahma/` is left to detection rather than created.
    #[test]
    fn deny_tier_mount_targets_are_the_existing_deny_tier_paths() {
        let ws = tempfile::tempdir().unwrap();
        let hooks = ws.path().join(".git").join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let roots = vec![ws.path().to_path_buf()];

        let hooks = dunce::canonicalize(&hooks).unwrap();
        assert_eq!(mount_targets(&roots), vec![hooks.clone()]);
        assert!(
            !ws.path().join(".ahma").exists(),
            "planning must not create the directory it would protect"
        );

        std::fs::create_dir(ws.path().join(".ahma")).unwrap();
        let ahma = dunce::canonicalize(ws.path().join(".ahma")).unwrap();
        let mut want = vec![hooks, ahma];
        want.sort();
        assert_eq!(mount_targets(&roots), want);
    }

    #[cfg(target_os = "linux")]
    mod linux {
        use super::*;
        use crate::sandbox::{Sandbox, SandboxMode};
        use std::os::fd::AsRawFd;

        /// Set by test runs that must *prove* prevention rather than skip where
        /// the host cannot provide it (the dedicated CI step).
        const REQUIRE_NAMESPACE_ENV: &str = "AHMA_TEST_REQUIRE_DENY_TIER_NAMESPACE";

        /// `true` when this host can run per-command namespaces. Otherwise says
        /// why and skips — unless the run must prove prevention (the dedicated
        /// CI step sets [`REQUIRE_NAMESPACE_ENV`]), where a skip would pass a
        /// step that proved nothing.
        fn namespace_available_or_skip(test: &str) -> bool {
            match availability() {
                Availability::Available => true,
                Availability::Unavailable { reason, errno } => {
                    let msg = format!(
                        "{test}: per-command namespaces unavailable here ({reason:?}, errno \
                         {errno}): {}",
                        reason.note()
                    );
                    assert!(
                        std::env::var_os(REQUIRE_NAMESPACE_ENV).is_none(),
                        "{msg}\n{REQUIRE_NAMESPACE_ENV} is set, so this run must prove \
                         prevention, not skip it"
                    );
                    eprintln!("Skipping test: {msg}");
                    false
                }
            }
        }

        /// A workspace with a git hook in it, as `git init` leaves one.
        fn workspace_with_hook() -> (tempfile::TempDir, PathBuf) {
            let ws = tempfile::tempdir().unwrap();
            let hooks = ws.path().join(".git").join("hooks");
            std::fs::create_dir_all(&hooks).unwrap();
            let hook = hooks.join("pre-commit");
            std::fs::write(&hook, "original\n").unwrap();
            (ws, hook)
        }

        fn strict_sandbox(ws: &std::path::Path) -> Sandbox {
            Sandbox::new(
                vec![ws.to_path_buf()],
                SandboxMode::Strict,
                false,
                false,
                false,
            )
            .unwrap()
        }

        /// The probe always reaches a verdict, and an unavailable one names a
        /// Linux reason — never "not Linux", never silence.
        #[test]
        fn deny_tier_namespace_probe_names_why_when_unavailable() {
            match availability() {
                Availability::Available => {}
                Availability::Unavailable { reason, errno } => {
                    assert_ne!(reason, Unavailable::NotLinux);
                    eprintln!("deny-tier namespace probe: {reason:?} (errno {errno})");
                    let _ = namespace_available_or_skip(
                        "deny_tier_namespace_probe_names_why_when_unavailable",
                    );
                }
            }
        }

        /// The point of the mode: `echo x > .git/hooks/pre-commit` from a
        /// sandboxed command fails with EROFS and the hook is untouched, while
        /// the rest of the workspace stays writable and the hook readable.
        #[tokio::test]
        async fn deny_tier_namespace_refuses_a_hook_write_from_a_sandboxed_command() {
            if !namespace_available_or_skip(
                "deny_tier_namespace_refuses_a_hook_write_from_a_sandboxed_command",
            ) {
                return;
            }
            let (ws, hook) = workspace_with_hook();
            let _mode = ModeGuard::set(LinuxDenyTier::Namespace);
            let sandbox = strict_sandbox(ws.path());
            if sandbox.spawn_landlock_ruleset_fd().unwrap().is_none() {
                assert!(
                    std::env::var_os(REQUIRE_NAMESPACE_ENV).is_none(),
                    "Landlock is unavailable, so nothing is spawned under the namespace"
                );
                eprintln!("Skipping test: Landlock unavailable on this kernel");
                return;
            }
            let planned = sandbox
                .spawn_deny_tier_mounts(ws.path())
                .expect("an existing hooks directory must be planned for a read-only bind");
            assert_eq!(
                planned.paths(),
                vec![dunce::canonicalize(hook.parent().unwrap()).unwrap()]
            );

            let out = sandbox
                .create_shell_command("sh", "echo x > .git/hooks/pre-commit", ws.path())
                .unwrap()
                .output()
                .await
                .unwrap();
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(!out.status.success(), "the hook write must fail: {stderr}");
            assert!(
                stderr.contains("Read-only file system"),
                "the read-only mount, not a permission rule, must refuse it: {stderr}"
            );
            assert_eq!(std::fs::read_to_string(&hook).unwrap(), "original\n");

            let out = sandbox
                .create_shell_command(
                    "sh",
                    "echo ok > notes.txt && cat .git/hooks/pre-commit",
                    ws.path(),
                )
                .unwrap()
                .output()
                .await
                .unwrap();
            assert!(
                out.status.success(),
                "the rest of the workspace stays writable and the hook readable: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(String::from_utf8_lossy(&out.stdout), "original\n");
            assert_eq!(
                std::fs::read_to_string(ws.path().join("notes.txt")).unwrap(),
                "ok\n"
            );
        }

        /// A child forked from a thread that is already inside a Landlock
        /// domain cannot mount. It must run anyway — without the namespace, its
        /// write landing for detection to report — never fail to spawn.
        #[tokio::test] // current-thread: the spawn below happens on this thread
        async fn deny_tier_namespace_child_falls_back_inside_an_inherited_landlock_domain() {
            if !namespace_available_or_skip(
                "deny_tier_namespace_child_falls_back_inside_an_inherited_landlock_domain",
            ) {
                return;
            }
            let (ws, hook) = workspace_with_hook();
            let _mode = ModeGuard::set(LinuxDenyTier::Namespace);
            let sandbox = strict_sandbox(ws.path());
            let scope = dunce::canonicalize(ws.path()).unwrap();
            let Some(fd) =
                crate::sandbox::landlock_ruleset_fd(&[scope], &[], false, false, None).unwrap()
            else {
                eprintln!("Skipping test: Landlock unavailable on this kernel");
                return;
            };
            // Confine this thread the way ahma's process-level enforcement
            // confines the thread that calls it.
            crate::sandbox::apply_landlock_ruleset_in_child(fd.as_raw_fd()).unwrap();

            let out = sandbox
                .create_shell_command("sh", "echo x > .git/hooks/pre-commit", ws.path())
                .unwrap()
                .output()
                .await
                .unwrap();
            assert!(
                out.status.success(),
                "a child that cannot enter the namespace must still run: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(
                std::fs::read_to_string(&hook).unwrap(),
                "x\n",
                "the write lands: detection, not prevention, is what remains"
            );
        }
    }
}
