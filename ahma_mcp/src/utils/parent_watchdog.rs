//! # Parent-death watchdog
//!
//! A dead-man's switch that terminates this process when the parent that
//! spawned it disappears.
//!
//! IDEs (Cursor, Antigravity, Claude Code, …) spawn `ahma serve stdio` as an
//! MCP server connected over a stdin/stdout pipe. The intended exit path is
//! **stdin EOF**: when the IDE closes the pipe, the proxy loop's
//! `stdio.receive()` returns `None` and the process exits. That path is
//! fragile in practice:
//!
//! * an uncaught `kill -9` of the IDE can leave the pipe write-end held by a
//!   sibling/inherited fd, so EOF is never delivered;
//! * the process can be killed/reparented while still inside startup
//!   (`spawn_background_bridge` health-check loop) before it ever begins
//!   reading stdin;
//! * a hung downstream connection can park the proxy loop.
//!
//! When any of these happen the server is reparented to init/launchd (pid 1)
//! and runs forever. Thousands of such orphans have been observed
//! accumulating across IDE sessions.
//!
//! This watchdog is the OS-level backstop. On Unix it records the spawning
//! parent's pid and polls `getppid()`; the kernel reparents an orphan to pid 1
//! (or another subreaper) the instant its parent dies, so a changed ppid is an
//! unambiguous "my parent is gone" signal. When detected, the process exits.
//!
//! IMPORTANT: install this **only** on the IDE-facing frontend process. The
//! intentionally-detached background bridge and hub daemon are reparented by
//! design (they outlive their spawner and self-terminate via idle-timeout);
//! installing the watchdog there would kill them immediately.

use std::time::Duration;

/// How often the watchdog re-checks its parent. A few seconds of latency is
/// irrelevant for orphan reaping and keeps the syscall cost negligible.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Pure decision predicate, extracted for unit testing.
///
/// Returns `true` when `current_ppid` indicates the original parent is gone:
/// either the process has been reparented (ppid changed) or it has been
/// reparented all the way to the init process (ppid <= 1).
#[cfg(unix)]
pub(crate) fn parent_is_gone(original_ppid: i32, current_ppid: i32) -> bool {
    current_ppid != original_ppid || current_ppid <= 1
}

/// Spawn the parent-death watchdog as a background tokio task.
///
/// Call this once, early, on the IDE-facing frontend process only. No-op when
/// the process is already orphaned at install time (nothing meaningful to
/// watch) or on non-Unix platforms.
#[cfg(unix)]
pub fn spawn_parent_death_watchdog() {
    // SAFETY: `getppid` is always safe to call and never fails.
    let original_ppid = unsafe { libc::getppid() };

    if original_ppid <= 1 {
        // Already orphaned, or launched directly by init — there is no
        // meaningful parent to watch.
        tracing::debug!(
            original_ppid,
            "parent_watchdog: no watchable parent at install; not arming"
        );
        return;
    }

    tracing::debug!(original_ppid, "parent_watchdog: armed");
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(POLL_INTERVAL);
        // The first tick completes immediately; skip it so we don't race the
        // very spawn that installed us.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            // SAFETY: see above.
            let current_ppid = unsafe { libc::getppid() };
            if parent_is_gone(original_ppid, current_ppid) {
                tracing::info!(
                    original_ppid,
                    current_ppid,
                    "parent_watchdog: parent process exited; terminating orphaned ahma server"
                );
                // The frontend proxy holds no in-flight operations of its own,
                // so an immediate exit is safe and avoids any risk of a hung
                // graceful-shutdown path keeping the orphan alive.
                std::process::exit(0);
            }
        }
    });
}

/// Non-Unix stub. On Windows, orphan reaping is handled by Job Object
/// association (the IDE's job, plus our own startup self-enforcement); a
/// dedicated parent-handle wait can be added here if orphaning is observed.
#[cfg(not(unix))]
pub fn spawn_parent_death_watchdog() {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn parent_alive_does_not_trigger() {
        // Same ppid as recorded, and it's a real (non-init) pid → keep running.
        assert!(!parent_is_gone(4321, 4321));
    }

    #[test]
    fn reparented_to_init_triggers() {
        // Unix reparents orphans to pid 1 (or a subreaper); ppid 1 means gone.
        assert!(parent_is_gone(4321, 1));
        assert!(parent_is_gone(4321, 0));
    }

    #[test]
    fn reparented_to_different_parent_triggers() {
        // ppid changed to some other live process (e.g. a subreaper) → original
        // parent is still gone.
        assert!(parent_is_gone(4321, 9999));
    }
}
