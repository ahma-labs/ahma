//! Whether a sandboxed command may signal processes outside its own sandbox
//! (SPEC R6.2.6).
//!
//! Off by default: on macOS the Seatbelt profile then grants
//! `(allow signal (target same-sandbox))`, so a command may `kill` only the
//! process tree it started under the same profile. One agent's `kill -TERM` of
//! another session's cargo build — which is how a shared build lock turned into
//! "chaos" across three checkouts — is refused by the kernel with `Operation not
//! permitted`. The opt-out (`[sandbox] signal_other_processes = true`) restores
//! the blanket `(allow signal)` for a workflow that genuinely has to stop a
//! pre-existing server.
//!
//! Process-global like the keychain toggle: set once at startup from the
//! resolved settings, read by every profile generation.

use std::sync::atomic::{AtomicBool, Ordering};

static SIGNAL_OTHER_PROCESSES: AtomicBool = AtomicBool::new(false);

/// Install the resolved `[sandbox] signal_other_processes` value.
pub fn set_signal_other_processes(allowed: bool) {
    SIGNAL_OTHER_PROCESSES.store(allowed, Ordering::Relaxed);
}

/// Whether sandboxed commands may signal processes outside their sandbox.
pub fn signal_other_processes_allowed() -> bool {
    SIGNAL_OTHER_PROCESSES.load(Ordering::Relaxed)
}

/// The Seatbelt rule for signals under the current setting.
pub fn seatbelt_signal_rule() -> &'static str {
    if signal_other_processes_allowed() {
        "(allow signal)\n"
    } else {
        "(allow signal (target same-sandbox))\n"
    }
}

/// Explain a `kill` the sandbox refused, if `stderr`/`stdout` show one, so
/// the agent learns it hit a boundary rather than a dead pid.
pub fn signal_denial_note(stderr: &str, stdout: &str) -> Option<String> {
    let pid = super::denial_scan::scan_signal_denial(stderr)
        .or_else(|| super::denial_scan::scan_signal_denial(stdout))?;
    Some(format!(
        "The sandbox refused to signal pid {pid}: it is not part of this command's own process \
         tree (another session's build, a server started earlier, or an unrelated process). \
         ahma confines signals to the command's own sandbox so one agent cannot stop another's \
         work (SPEC R6.2.6). If that process must be stopped, ask the human; a workflow that \
         genuinely needs this can set `[sandbox] signal_other_processes = true`."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_opt_out_restores_allow_signal() {
        set_signal_other_processes(false);
        assert_eq!(
            seatbelt_signal_rule(),
            "(allow signal (target same-sandbox))\n"
        );
        set_signal_other_processes(true);
        assert_eq!(seatbelt_signal_rule(), "(allow signal)\n");
        set_signal_other_processes(false);
    }

    #[test]
    fn a_refused_kill_is_explained() {
        let note = signal_denial_note("bash: line 0: kill: (78690) - Operation not permitted", "")
            .expect("bash kill denial");
        assert!(note.contains("pid 78690"), "{note}");
        assert!(note.contains("ask the human"), "{note}");
        assert!(
            signal_denial_note("", "kill: kill 5804 failed: operation not permitted").is_some()
        );
        assert!(signal_denial_note("No such process", "").is_none());
    }
}
