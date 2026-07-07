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
