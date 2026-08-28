//! Undoing what the TUI does to the user's terminal — on every exit path,
//! including a panic.
//!
//! `app::run_ratatui` puts the terminal into raw mode, switches it to the
//! alternate screen, turns on mouse capture and bracketed paste, and (where the
//! terminal supports it) pushes a Kitty keyboard-protocol flag. All five have to
//! be undone or the user is left with a shell that echoes nothing, shows the
//! wrong screen, and emits escape noise on every mouse move.
//!
//! The restore used to sit as a straight-line block after the event loop's future
//! resolved. That covers a normal exit and an `Err` return, and nothing else: a
//! panic inside the loop unwinds straight past it. The result was the worst
//! version of a crash — the terminal stays raw and on the alternate screen, so
//! the panic message itself is written somewhere the user cannot see, and they
//! are left with a shell that appears dead and no explanation of why.
//! `ahma_tui/SPEC.md` §3 requires the restore to hold "even upon panic"; this
//! module is what makes that true.
//!
//! Two mechanisms, because neither alone is enough:
//!
//! * [`TerminalGuard`]'s `Drop` covers unwinding, `?` returns, and normal exit —
//!   every path that runs destructors.
//! * A chained panic hook covers the ordering `Drop` cannot: it restores
//!   **before** the default hook prints, so the panic message lands on the
//!   normal screen instead of an alternate screen that is about to be discarded.
//!   Chaining (rather than replacing) follows
//!   `ahma_mcp::utils::logging::install_panic_flush_hook`, so the two hooks
//!   compose and the log flush is not lost.
//!
//! Restoring twice is harmless in principle but is suppressed anyway by a shared
//! `done` flag, so the keyboard-flag pop cannot unbalance its stack.

#![cfg(feature = "tui")]

use std::io::{self, Write};
use std::panic::PanicHookInfo;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{DisableBracketedPaste, DisableMouseCapture, PopKeyboardEnhancementFlags};
use crossterm::execute;
use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};

/// The type `std::panic::set_hook` takes and `take_hook` returns.
type PanicHook = Box<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static>;

/// Write the escape sequence that undoes the TUI's terminal setup.
///
/// Split out from [`restore_stdout`] so the sequence can be asserted against a
/// buffer in a test rather than against the developer's actual terminal.
///
/// `keyboard_enhanced` mirrors whether the Kitty flag push actually succeeded:
/// popping a flag that was never pushed would unbalance crossterm's stack.
fn write_restore_sequence<W: Write>(out: &mut W, keyboard_enhanced: bool) -> io::Result<()> {
    if keyboard_enhanced {
        execute!(out, PopKeyboardEnhancementFlags)?;
    }
    execute!(
        out,
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste,
        crossterm::cursor::Show,
    )
}

/// Best-effort restore of the real terminal. Never panics and never returns an
/// error: it runs on the way out (including from inside a panic hook), where
/// there is nobody left to report to and a second panic would abort the process.
fn restore_stdout(keyboard_enhanced: bool) {
    let _ = disable_raw_mode();
    let mut out = io::stdout();
    let _ = write_restore_sequence(&mut out, keyboard_enhanced);
    let _ = out.flush();
}

/// Compose a panic hook that restores the terminal and then defers to `previous`.
///
/// The order is the whole point: the default hook writes the panic message to
/// stderr, and on an alternate screen in raw mode that message is invisible and
/// unscrollable. Restoring first means the user sees it.
///
/// `restore` is a parameter rather than a direct call to [`restore_stdout`] so
/// the composition — that it restores, that it restores *first*, and that it
/// still calls the previous hook — is testable without installing a
/// process-global hook or writing to the developer's terminal.
fn compose_panic_hook(
    previous: PanicHook,
    done: Arc<AtomicBool>,
    keyboard_enhanced: Arc<AtomicBool>,
    restore: impl Fn(bool) + Sync + Send + 'static,
) -> PanicHook {
    Box::new(move |info| {
        if !done.swap(true, Ordering::SeqCst) {
            restore(keyboard_enhanced.load(Ordering::SeqCst));
        }
        previous(info);
    })
}

/// Owns the terminal's "we changed it" state and guarantees the undo.
///
/// Arm this immediately after `enable_raw_mode()` — before the rest of the
/// setup — so an error partway through the setup is covered too. Tell it about
/// the keyboard flag with [`TerminalGuard::set_keyboard_enhanced`] once that
/// push is known to have succeeded.
pub(crate) struct TerminalGuard {
    done: Arc<AtomicBool>,
    keyboard_enhanced: Arc<AtomicBool>,
    restore: Restore,
}

/// How a guard puts the terminal back. Injected rather than hard-wired so
/// [`TerminalGuard::drop`] itself can be tested: a test that let `Drop` call
/// [`restore_stdout`] would take the test runner's own terminal out of raw mode
/// and spray escape codes through the captured output. A test that instead
/// re-implements the drop logic proves nothing about the drop.
type Restore = Arc<dyn Fn(bool) + Sync + Send>;

impl TerminalGuard {
    /// Take ownership of the terminal's restored-ness and install the panic hook.
    pub(crate) fn arm() -> Self {
        let guard = Self::with_restore(Arc::new(restore_stdout));
        let previous = std::panic::take_hook();
        std::panic::set_hook(compose_panic_hook(
            previous,
            Arc::clone(&guard.done),
            Arc::clone(&guard.keyboard_enhanced),
            restore_stdout,
        ));
        guard
    }

    /// A guard with no panic hook and a caller-supplied restore. Used by
    /// [`Self::arm`] and by the tests; installing a process-global panic hook is
    /// [`Self::arm`]'s job alone, so a test can construct a guard without
    /// disturbing the rest of the test binary.
    fn with_restore(restore: Restore) -> Self {
        Self {
            done: Arc::new(AtomicBool::new(false)),
            keyboard_enhanced: Arc::new(AtomicBool::new(false)),
            restore,
        }
    }

    /// Record that the Kitty keyboard-protocol flag was pushed, so the restore
    /// pops it. Call only when the push actually succeeded.
    pub(crate) fn set_keyboard_enhanced(&self, enhanced: bool) {
        self.keyboard_enhanced.store(enhanced, Ordering::SeqCst);
    }

    /// Restore now. Idempotent, and shares its flag with the panic hook, so
    /// whichever of the two runs first is the only one that writes.
    pub(crate) fn restore(&self) {
        if self.done.swap(true, Ordering::SeqCst) {
            return;
        }
        (self.restore)(self.keyboard_enhanced.load(Ordering::SeqCst));
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    /// The bytes crossterm emits are terminal-control sequences, so the readable
    /// assertion is "did the sequence for X get written", not an exact string.
    fn rendered(keyboard_enhanced: bool) -> String {
        let mut buf: Vec<u8> = Vec::new();
        write_restore_sequence(&mut buf, keyboard_enhanced).expect("writing to a Vec cannot fail");
        String::from_utf8(buf).expect("crossterm emits ASCII escape sequences")
    }

    #[test]
    fn the_restore_sequence_undoes_every_part_of_the_setup() {
        let seq = rendered(false);
        // `?1049l` leaves the alternate screen, `?1000l`/`?1006l` disable mouse
        // reporting, `?2004l` disables bracketed paste, `?25h` shows the cursor.
        for (code, what) in [
            ("?1049l", "leave alternate screen"),
            ("?1006l", "disable SGR mouse reporting"),
            ("?1000l", "disable mouse capture"),
            ("?2004l", "disable bracketed paste"),
            ("?25h", "show the cursor"),
        ] {
            assert!(
                seq.contains(code),
                "restore sequence is missing {what} ({code}); the user would be left with it \
                 still switched on. Got: {seq:?}"
            );
        }
    }

    #[test]
    fn the_keyboard_flag_is_popped_only_when_it_was_pushed() {
        // Crossterm's pop is `CSI <`. Popping a flag that was never pushed
        // unbalances its stack, which is why the guard tracks the push result
        // instead of always popping.
        assert!(
            rendered(true).contains("\x1b[<"),
            "an enhanced-keyboard session must pop the flag it pushed"
        );
        assert!(
            !rendered(false).contains("\x1b[<"),
            "a session that never pushed the keyboard flag must not pop one"
        );
    }

    /// A guard whose restore records the `keyboard_enhanced` value it was called
    /// with, so tests can assert on both the count and the argument.
    fn observing_guard() -> (TerminalGuard, Arc<Mutex<Vec<bool>>>) {
        let calls: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&calls);
        let guard =
            TerminalGuard::with_restore(Arc::new(move |enhanced| sink.lock().push(enhanced)));
        (guard, calls)
    }

    #[test]
    fn dropping_the_guard_restores_the_terminal() {
        let (guard, calls) = observing_guard();
        assert!(
            calls.lock().is_empty(),
            "arming must not restore anything yet"
        );
        drop(guard);
        assert_eq!(
            calls.lock().len(),
            1,
            "Drop is what covers an unwind: without it a panic in the event loop \
             leaves the terminal raw and on the alternate screen (ahma_tui/SPEC.md §3)"
        );
    }

    #[test]
    fn restoring_twice_writes_once() {
        let (guard, calls) = observing_guard();
        guard.restore();
        guard.restore();
        drop(guard);
        assert_eq!(
            calls.lock().len(),
            1,
            "restore must be idempotent: the explicit call at the end of the event \
             loop, the panic hook, and Drop can all run for one session, and popping \
             the keyboard flag more than once unbalances crossterm's stack"
        );
    }

    #[test]
    fn the_guard_restores_with_the_keyboard_flag_it_was_told_about() {
        let (guard, calls) = observing_guard();
        guard.set_keyboard_enhanced(true);
        drop(guard);
        assert_eq!(
            *calls.lock(),
            vec![true],
            "the push result must reach the restore, or an enhanced session leaves \
             its keyboard flag on the stack"
        );
    }

    #[test]
    fn the_panic_hook_restores_before_deferring_to_the_previous_hook() {
        let order: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

        let previous: PanicHook = {
            let order = Arc::clone(&order);
            Box::new(move |_| order.lock().push("previous"))
        };
        let restore = {
            let order = Arc::clone(&order);
            move |_enhanced: bool| order.lock().push("restore")
        };

        let hook = compose_panic_hook(
            previous,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(true)),
            restore,
        );

        // Build a real `PanicHookInfo` by catching a panic with the composed hook
        // installed, then put the previous hook back so no other test sees it.
        let saved = std::panic::take_hook();
        std::panic::set_hook(hook);
        let _ = std::panic::catch_unwind(|| panic!("deliberate panic under test"));
        std::panic::set_hook(saved);

        assert_eq!(
            *order.lock(),
            vec!["restore", "previous"],
            "the terminal must be restored BEFORE the default hook prints, or the \
             panic message is written to an alternate screen the user never sees — \
             and the previous hook must still run, or chaining would drop \
             ahma_mcp's panic log flush (SPEC R-SIGN.5)"
        );
    }

    #[test]
    fn the_panic_hook_restores_at_most_once() {
        let restores = Arc::new(AtomicBool::new(false));
        let count = Arc::new(Mutex::new(0usize));
        let previous: PanicHook = Box::new(|_| {});
        let restore = {
            let count = Arc::clone(&count);
            move |_enhanced: bool| *count.lock() += 1
        };
        let hook = compose_panic_hook(
            previous,
            Arc::clone(&restores),
            Arc::new(AtomicBool::new(false)),
            restore,
        );

        let saved = std::panic::take_hook();
        std::panic::set_hook(hook);
        let _ = std::panic::catch_unwind(|| panic!("first"));
        let _ = std::panic::catch_unwind(|| panic!("second"));
        std::panic::set_hook(saved);

        assert_eq!(
            *count.lock(),
            1,
            "a double panic must not emit the restore sequence twice"
        );
    }
}
