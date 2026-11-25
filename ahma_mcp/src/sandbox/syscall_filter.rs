//! The Linux system-call baseline (SPEC R-HARDEN.3).
//!
//! A kernel privilege escalation escapes any Landlock sandbox: it never asks
//! Landlock. What a sandbox can do is keep the kernel's least-needed, most
//! exploited surface out of reach. 2026's escalations reached it through
//! `AF_ALG` sockets (CVE-2026-31431, "Copy Fail"), `io_uring` and keyrings.
//! Every command ahma runs gets a seccomp filter refusing system calls a build
//! or test never needs:
//!
//! - `io_uring_setup`, `io_uring_enter`, `io_uring_register`, `bpf`,
//!   `perf_event_open`, `userfaultfd`, `keyctl`, `add_key`, `request_key`:
//!   refused with `ENOSYS`, the answer a program takes as "not on this kernel"
//!   and falls back from (libuv probes `io_uring` and uses threads instead);
//! - `socket(AF_ALG, …)`: refused with `EAFNOSUPPORT`;
//! - with the egress proxy on (`--restrict-network`), UDP and raw IP sockets:
//!   Landlock's network rules cover TCP only, so a UDP datagram (a DNS query
//!   carrying data) would otherwise leave the sandbox unseen.
//!
//! `ptrace` is not listed: Landlock already refuses tracing a process outside
//! the command's own domain. `[sandbox] syscall_filter = false` turns the
//! filter off, disclosed at startup.

use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(true);

/// Install `[sandbox] syscall_filter` (on by default). Called once at startup.
pub fn set_syscall_filter(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
    if !enabled && cfg!(target_os = "linux") {
        tracing::warn!(
            "[sandbox] syscall_filter = false: commands can reach io_uring, bpf, keyrings and \
             AF_ALG sockets, the kernel surface most privilege escalations use (SPEC R-HARDEN.3)"
        );
    }
}

/// Whether commands get the filter.
pub fn syscall_filter_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

#[cfg(target_os = "linux")]
pub use linux::{apply_in_child, spawn_filter};

#[cfg(target_os = "linux")]
mod linux {
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule,
    };
    use std::collections::BTreeMap;

    /// The system calls refused outright, with `ENOSYS`.
    const NOT_AVAILABLE: &[libc::c_long] = &[
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
    ];

    /// The filter for one command, built before `fork` (the child may not
    /// allocate). `None` when the filter is off, or the architecture is one
    /// seccompiler does not know, which is disclosed.
    pub fn spawn_filter(restrict_network: bool) -> Option<Vec<BpfProgram>> {
        if !super::syscall_filter_enabled() {
            return None;
        }
        match build(restrict_network) {
            Ok(program) => Some(program),
            Err(e) => {
                tracing::warn!(
                    "seccomp baseline not applied (SPEC R-HARDEN.3): {e}. Commands can reach \
                     io_uring, bpf, keyrings and AF_ALG sockets."
                );
                None
            }
        }
    }

    fn build(restrict_network: bool) -> anyhow::Result<Vec<BpfProgram>> {
        let arch = std::env::consts::ARCH
            .try_into()
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        // One filter per action: seccompiler gives a filter one match action,
        // and the two answers differ (ENOSYS for a call, EAFNOSUPPORT for a
        // socket family). The kernel runs every installed filter and keeps the
        // strictest verdict, so two filters compose exactly.
        let calls: BTreeMap<i64, Vec<SeccompRule>> = NOT_AVAILABLE
            .iter()
            .map(|&n| (i64::from(n), Vec::new()))
            .collect();
        let mut programs: Vec<BpfProgram> = vec![
            SeccompFilter::new(
                calls,
                SeccompAction::Allow,
                SeccompAction::Errno(libc::ENOSYS as u32),
                arch,
            )?
            .try_into()?,
        ];

        let family = |f: libc::c_int| {
            SeccompCondition::new(
                0,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Eq,
                f as u32 as u64,
            )
        };
        // The type argument carries SOCK_CLOEXEC / SOCK_NONBLOCK; compare its
        // low four bits.
        let kind = |k: libc::c_int| {
            SeccompCondition::new(
                1,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::MaskedEq(0xf),
                k as u32 as u64,
            )
        };
        let mut socket_rules = vec![SeccompRule::new(vec![family(libc::AF_ALG)?])?];
        if restrict_network {
            for fam in [libc::AF_INET, libc::AF_INET6] {
                for k in [libc::SOCK_DGRAM, libc::SOCK_RAW] {
                    socket_rules.push(SeccompRule::new(vec![family(fam)?, kind(k)?])?);
                }
            }
        }
        let sockets: BTreeMap<i64, Vec<SeccompRule>> =
            [(i64::from(libc::SYS_socket), socket_rules)]
                .into_iter()
                .collect();
        programs.push(
            SeccompFilter::new(
                sockets,
                SeccompAction::Allow,
                SeccompAction::Errno(libc::EAFNOSUPPORT as u32),
                arch,
            )?
            .try_into()?,
        );
        #[cfg(target_arch = "x86_64")]
        programs.push(x32_refused());
        // Each program ends in a return, so they cannot be concatenated: each
        // is installed with its own seccomp(2) call.
        Ok(programs)
    }

    /// Refuse every x32-ABI system call with `ENOSYS`. An x32 call reports the
    /// x86_64 architecture with bit 30 set in its number, so seccompiler's
    /// exact-number rules would never match `io_uring_setup | 0x4000_0000`.
    /// No build tool targets x32. (A 32-bit i386 call reports another
    /// architecture, and seccompiler's own arch check kills the process.)
    ///
    /// No test can tell this program apart from a kernel built without x32,
    /// which answers `ENOSYS` too; it is checked by reading it.
    #[cfg(target_arch = "x86_64")]
    fn x32_refused() -> BpfProgram {
        use seccompiler::sock_filter;
        const LD_W_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
        const JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
        const JGE_K: u16 = 0x35; // BPF_JMP | BPF_JGE | BPF_K
        const RET_K: u16 = 0x06; // BPF_RET | BPF_K
        const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
        const X32_SYSCALL_BIT: u32 = 0x4000_0000;
        let op = |code, k, jt, jf| sock_filter { code, jt, jf, k };
        vec![
            op(LD_W_ABS, 4, 0, 0),              // seccomp_data.arch
            op(JEQ_K, AUDIT_ARCH_X86_64, 0, 3), // not x86_64: allow (another filter kills it)
            op(LD_W_ABS, 0, 0, 0),              // seccomp_data.nr
            op(JGE_K, X32_SYSCALL_BIT, 0, 1),
            op(RET_K, libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32, 0, 0),
            op(RET_K, libc::SECCOMP_RET_ALLOW, 0, 0),
        ]
    }

    /// Install `programs` in the calling (child) process. Called from
    /// `pre_exec`, after Landlock: only `prctl` and `seccomp`, and no
    /// allocation, even on the error path (the `io::Error` the kernel's errno
    /// produced is passed on as it is).
    pub fn apply_in_child(programs: &[BpfProgram]) -> std::io::Result<()> {
        for program in programs {
            if let Err(e) = seccompiler::apply_filter(program) {
                return Err(match e {
                    seccompiler::Error::Prctl(io) | seccompiler::Error::Seccomp(io) => io,
                    _ => std::io::Error::from_raw_os_error(libc::EINVAL),
                });
            }
        }
        Ok(())
    }
}
