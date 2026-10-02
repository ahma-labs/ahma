//! Active probe for whether an OUTER host sandbox is confining this ahma process.
//!
//! [`super::host_detect`] reads environment markers — cheap, but env presence does
//! **not** prove the host's sandbox actually confines ahma. An IDE (Cursor, VS
//! Code, Claude Code) sets `CURSOR_SANDBOX` / `CLAUDECODE` / `VSCODE_*` in the
//! environment of the MCP server it launches, yet it does **not** wrap that
//! server's own executions (SPEC R7). So a bare env check would false-positive on
//! essentially every IDE-launched MCP server and wrongly advise the user to
//! "disable your host sandbox" when they are not nested at all.
//!
//! To disclose the "ahma is enforcing **on top of** a host sandbox" (intersection)
//! case honestly, we need positive proof of confinement. We get it by actively
//! testing: attempt a write to a path **outside** every plausible sandbox scope (a
//! file directly under `$HOME`). If an outer sandbox blocks it, ahma is genuinely
//! confined and the effective policy for its subprocesses is the *intersection* of
//! both sandboxes; if the write succeeds, ahma is authoritative and we stay silent.
//!
//! Runs once (cached). macOS-only: the intersection problem is Seatbelt-specific —
//! on Linux reads are already scope-gated by Landlock and a Docker container is a
//! deliberate, supported compose rather than a surprise. Returns `None` elsewhere.

use super::host_detect::{HostSandbox, detect_host_sandbox};
use std::sync::OnceLock;

static CONFINEMENT: OnceLock<Option<HostSandbox>> = OnceLock::new();

/// Whether an outer host sandbox is confining this process, computed once and
/// cached. `Some(host)` means a write outside every plausible scope was blocked
/// (positive proof) — named via env markers when possible, else `Unidentified`.
/// `None` means ahma is not confined (or the probe does not apply on this OS).
pub fn outer_confinement() -> Option<HostSandbox> {
    *CONFINEMENT.get_or_init(|| {
        // Only spend a probe write when env markers suggest a host at all: this
        // avoids a needless $HOME write on every plain-terminal startup and gives
        // us the host's name. The write-probe is then the tie-breaker for "markers
        // present, but is the host actually confining us?" (the IDE-launched-but-
        // unconfined MCP-server case answers no). An outer sandbox that sets no
        // markers is not reported here (rare); a nesting-denied outer sandbox is
        // instead caught up front by the sandbox-exec check (see cli startup).
        let host = detect_host_sandbox();
        let write_blocked = host.is_some() && outer_write_blocked();
        classify_confinement(write_blocked, host)
    })
}

/// Pure decision: given whether an out-of-scope write was blocked and any host
/// named from env markers, decide whether to report confinement. Kept separate
/// from the I/O so it is unit-testable without touching the filesystem.
fn classify_confinement(write_blocked: bool, host: Option<HostSandbox>) -> Option<HostSandbox> {
    if write_blocked {
        Some(host.unwrap_or(HostSandbox::Unidentified))
    } else {
        None
    }
}

/// Attempt a write directly under `$HOME` (outside any workspace/temp scope a host
/// sandbox would allow). `PermissionDenied` => an outer sandbox is confining us.
/// Any other outcome (success, or an unrelated error like a read-only home) is
/// treated as "not confined" so we never *claim* confinement we cannot prove.
#[cfg(target_os = "macos")]
fn outer_write_blocked() -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let probe = home.join(format!(".ahma-confine-probe-{}", std::process::id()));
    let blocked = matches!(
        std::fs::write(&probe, b""),
        Err(ref e) if e.kind() == std::io::ErrorKind::PermissionDenied
    );
    // Best-effort cleanup on the success path (nothing was created when blocked).
    let _ = std::fs::remove_file(&probe);
    blocked
}

#[cfg(not(target_os = "macos"))]
fn outer_write_blocked() -> bool {
    false
}

// ─────────────────────────────────────────────────────────────────────────────
// Daemons left behind inside a sandbox (SPEC R-DOCTOR.7)
// ─────────────────────────────────────────────────────────────────────────────

/// Whether `pid` runs inside a Seatbelt sandbox, asked of the kernel. Any
/// pid may be asked; a daemon a sandboxed command left behind inherits the
/// command's profile and answers `true` for the rest of its life.
#[cfg(target_os = "macos")]
pub fn pid_is_seatbelt_confined(pid: u32) -> bool {
    const SANDBOX_CHECK_NO_REPORT: libc::c_int = 0x0002;
    unsafe extern "C" {
        fn sandbox_check(
            pid: libc::pid_t,
            operation: *const libc::c_char,
            type_: libc::c_int,
            ...
        ) -> libc::c_int;
    }
    // SAFETY: a NULL operation only asks whether the pid is sandboxed at all.
    unsafe {
        sandbox_check(
            pid as libc::pid_t,
            std::ptr::null(),
            SANDBOX_CHECK_NO_REPORT,
        ) == 1
    }
}

#[cfg(not(target_os = "macos"))]
pub fn pid_is_seatbelt_confined(_pid: u32) -> bool {
    false
}

/// Long-lived helpers running *inside* a sandbox that no longer has a parent:
/// a build daemon (sccache, a Gradle or Kotlin daemon) started by a sandboxed
/// command, reparented to launchd when that command ended, serving every
/// session on the machine but able to write only the checkout it was born
/// in. The rule is structural, not a list of programs: confined, orphaned,
/// and the user's own executable (under `$HOME`, so an App-Sandboxed Apple
/// application or a system helper never matches).
pub fn confined_daemons() -> Vec<ahma_common::doctor::ConfinedDaemon> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let Some(home) = ahma_common::config::ahma_home_dir() else {
        return Vec::new();
    };
    let home = dunce::canonicalize(&home).unwrap_or(home);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_exe(UpdateKind::OnlyIfNotSet)
            .with_user(UpdateKind::OnlyIfNotSet),
    );
    let me = std::process::id();
    let mut out: Vec<ahma_common::doctor::ConfinedDaemon> = sys
        .processes()
        .iter()
        .filter_map(|(pid, p)| {
            let pid = pid.as_u32();
            if pid == me || p.parent().map(|pp| pp.as_u32()) != Some(1) {
                return None;
            }
            let exe = p.exe()?;
            if !exe.starts_with(&home) || exe.starts_with(home.join("Applications")) {
                return None;
            }
            let name = exe.file_name()?.to_string_lossy().into_owned();
            if name.starts_with("ahma") {
                return None;
            }
            if !pid_is_seatbelt_confined(pid) {
                return None;
            }
            Some(ahma_common::doctor::ConfinedDaemon {
                name,
                pid,
                confined_to: None,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name).then(a.pid.cmp(&b.pid)));
    out
}

/// At startup of an *unconfined* ahma process: if a confined sccache server is
/// serving this machine, restart it so no checkout's build fails on it. Only
/// sccache, whose `--stop-server`/`--start-server` protocol is known; other
/// confined daemons are reported by `ahma doctor`, not touched.
pub fn restart_confined_sccache_if_any() {
    if super::prerequisites::process_is_seatbelt_confined() {
        return;
    }
    let confined: Vec<_> = confined_daemons()
        .into_iter()
        .filter(|d| d.name == "sccache")
        .collect();
    if confined.is_empty() {
        return;
    }
    let pids: Vec<String> = confined.iter().map(|d| d.pid.to_string()).collect();
    tracing::warn!(
        "a sccache server (pid {}) is running inside a sandbox and can write only the checkout \
         it was started in; restarting it unconfined so every checkout's build works",
        pids.join(", ")
    );
    let _ = std::process::Command::new("sccache")
        .arg("--stop-server")
        .output();
    match std::process::Command::new("sccache")
        .arg("--start-server")
        .env("SCCACHE_IDLE_TIMEOUT", "0")
        .output()
    {
        Ok(o) if o.status.success() => {
            tracing::info!("sccache server restarted outside the sandbox")
        }
        Ok(o) => tracing::warn!(
            "sccache --start-server failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => tracing::warn!("could not run sccache: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::host_detect::HostSandbox;
    use super::*;

    #[test]
    fn not_confined_when_write_succeeds() {
        assert_eq!(classify_confinement(false, Some(HostSandbox::Cursor)), None);
        assert_eq!(classify_confinement(false, None), None);
    }

    #[test]
    fn confined_names_host_when_write_blocked() {
        assert_eq!(
            classify_confinement(true, Some(HostSandbox::ClaudeCode)),
            Some(HostSandbox::ClaudeCode)
        );
    }

    #[test]
    fn confined_unidentified_when_blocked_without_named_host() {
        assert_eq!(
            classify_confinement(true, None),
            Some(HostSandbox::Unidentified)
        );
    }

    #[test]
    fn outer_confinement_is_stable_across_calls() {
        // Cached: whatever the first call decided, subsequent calls agree.
        assert_eq!(outer_confinement(), outer_confinement());
    }
}
