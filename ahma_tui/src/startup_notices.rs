//! Startup events the user must actually see.
//!
//! Everything interesting that happens before the first frame — a version
//! mismatch that restarted someone else's bridge, a self re-exec, a refused
//! bridge reuse, a `--profile` that did not load — used to be reported with
//! `tracing::warn!`. The default log target is a **file** the user has never
//! been told about, so in practice those events were silent: the TUI simply
//! opened, having (for instance) just restarted a bridge that was serving a
//! live IDE session.
//!
//! This is the channel for those events. Producers call [`push`] during
//! startup; the app drains them into the log pane on the first frame, and
//! anything at [`Level::Warn`] or above is *also* written into the chat
//! transcript, which is the pane that is actually open by default.
//!
//! A process-global buffer rather than a threaded-through parameter because
//! producers sit on several unrelated call paths that all run before the app
//! state exists, and because a `std::process::exec` self-restart discards
//! anything held in a local (the restarted process re-reports for itself).

use std::sync::{Mutex, OnceLock};

/// How loudly a startup notice should be shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Worth recording, not worth interrupting for (log pane only).
    Info,
    /// The user would be surprised to learn this happened silently — shown in
    /// the chat transcript as well as the log pane.
    Warn,
}

/// One thing that happened before the UI came up.
#[derive(Debug, Clone)]
pub struct Notice {
    pub level: Level,
    pub message: String,
}

fn buffer() -> &'static Mutex<Vec<Notice>> {
    static BUFFER: OnceLock<Mutex<Vec<Notice>>> = OnceLock::new();
    BUFFER.get_or_init(|| Mutex::new(Vec::new()))
}

/// Record a startup event for display once the UI is up. Also mirrored to
/// `tracing` so the log file keeps its record for post-hoc debugging.
pub fn push(level: Level, message: impl Into<String>) {
    let message = message.into();
    match level {
        Level::Info => tracing::info!("{}", message),
        Level::Warn => tracing::warn!("{}", message),
    }
    // A poisoned lock must not take the TUI down over a status message.
    if let Ok(mut buf) = buffer().lock() {
        buf.push(Notice { level, message });
    }
}

/// Take everything recorded so far, leaving the buffer empty.
pub fn drain() -> Vec<Notice> {
    buffer()
        .lock()
        .map(|mut buf| std::mem::take(&mut *buf))
        .unwrap_or_default()
}

/// Serializes tests that exercise the process-global buffer. Without it, two
/// such tests running on different threads drain each other's notices and fail
/// intermittently — the kind of flake that only shows up under CI load.
#[cfg(test)]
pub static TEST_GUARD: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    /// Notices survive until drained, in order, and a drain empties the buffer
    /// so a second reader does not replay them.
    #[test]
    fn push_then_drain_returns_in_order_and_empties() {
        let _guard = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let _ = drain();
        push(Level::Info, "first");
        push(Level::Warn, "second");

        let drained = drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].level, Level::Info);
        assert_eq!(drained[0].message, "first");
        assert_eq!(drained[1].level, Level::Warn);
        assert_eq!(drained[1].message, "second");

        assert!(drain().is_empty(), "a drained buffer must not replay");
    }
}
