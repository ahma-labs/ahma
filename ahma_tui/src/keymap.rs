//! Maps raw crossterm `KeyEvent`s to semantic `Action`s for the TUI event loop.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::state::{Focus, ModalState};

/// High-level action emitted from a key press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Quit,
    Up,
    Down,
    Top,
    Bottom,
    Enter,
    Tab,
    BackTab,
    // Approval
    Approve,
    ApproveAlways,
    Reject,
    // Op actions
    CancelOp,
    PinOp,
    /// Ask for access to the path a selected *denied* operation was refused —
    /// re-raises the grant question through the same broker (SPEC R-PERM.7.1).
    ReRaiseGrant,
    /// Fold/unfold the selected task-tree node inline (Space) — Enter drills
    /// into the full-screen detail view instead.
    ToggleNode,
    /// Close the full-screen operation detail overlay.
    DetailClose,
    /// Toggle the task tree between this project's instances and all projects.
    ToggleProjectFilter,
    /// Open (or close) the chat pane. Chat is a thing you choose to do, so it
    /// has a key rather than being the screen (SPEC R24.9).
    ToggleChat,
    // Toggles
    ToggleHelp,
    // Log filter
    StartFilter,
    FilterChar(char),
    FilterBackspace,
    FilterEsc,
    // Chat mode input
    FocusChat,
    /// A printable character typed into the chat input box.
    InputChar(char),
    /// Backspace in the chat input box.
    InputBackspace,
    /// Submit the chat input (Enter without Shift).
    InputSubmit,
    /// Insert a real newline (Shift+Enter) in the chat input.
    InputNewline,
    /// Clear the chat input (Esc when non-empty, or double-Esc).
    InputClear,
    // `/` command navigator
    OpenNavigator,
    NavChar(char),
    NavBackspace,
    NavComplete,
    NavEsc,
    NavSubmit,
    NavUp,
    NavDown,
    // Log monitor actions
    ToggleWrap,
    ToggleZoom,
    OpenLogSwitcher,
    CloseLogSwitcher,
    SubmitLogSwitcher,
    ApproveSymlink,
    Unknown,
}

// ─── Key reference (help overlay and footer) ──────────────────────────────────

/// Where a documented key applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyScope {
    /// Any panel except while typing in the chat input.
    Global,
    /// The work view (sections and tasks).
    Work,
    /// The log pane.
    Log,
    /// The chat input.
    Chat,
    /// A pending approval / scope-grant / web-access prompt.
    Gate,
}

/// One documented key binding.
pub struct KeyDoc {
    pub scope: KeyScope,
    /// The keys, as `map_key` receives them; checked by the tests below, so a
    /// documented key that does nothing fails the build's tests.
    pub keys: &'static [(KeyCode, KeyModifiers)],
    /// How the keys are written in help and the footer.
    pub label: &'static str,
    pub action: &'static str,
    /// Shown in the footer for its scope (the rest are in `?` help).
    pub footer: bool,
}

const NONE: KeyModifiers = KeyModifiers::NONE;
const CTRL: KeyModifiers = KeyModifiers::CONTROL;
const SHIFT: KeyModifiers = KeyModifiers::SHIFT;

/// The single source for every key the help overlay and footer mention. The
/// help used to be hand-written next to the bindings and drifted: it said Enter
/// opened a section after Enter started opening chat, and omitted half the
/// work-view keys. `every_bound_key_is_documented` and
/// `every_documented_key_does_something` keep this table and `map_key` in step.
pub const KEY_REFERENCE: &[KeyDoc] = &[
    KeyDoc {
        scope: KeyScope::Global,
        keys: &[(KeyCode::Char('c'), CTRL)],
        label: "Ctrl-C",
        action: "cancel the running turn; again to quit",
        footer: false,
    },
    KeyDoc {
        scope: KeyScope::Global,
        keys: &[(KeyCode::Char('q'), NONE)],
        label: "q",
        action: "quit (asks again while work runs)",
        footer: true,
    },
    KeyDoc {
        scope: KeyScope::Global,
        keys: &[(KeyCode::Tab, NONE), (KeyCode::BackTab, SHIFT)],
        label: "Tab",
        action: "next panel (Shift-Tab: previous)",
        footer: true,
    },
    KeyDoc {
        scope: KeyScope::Global,
        keys: &[(KeyCode::Char('?'), NONE)],
        label: "?",
        action: "help",
        footer: true,
    },
    KeyDoc {
        scope: KeyScope::Global,
        keys: &[(KeyCode::Char('i'), NONE)],
        label: "i",
        action: "open or close chat",
        footer: true,
    },
    KeyDoc {
        scope: KeyScope::Global,
        keys: &[(KeyCode::Char('/'), NONE)],
        label: "/",
        action: "commands (in the log pane: filter)",
        footer: false,
    },
    KeyDoc {
        scope: KeyScope::Global,
        keys: &[(KeyCode::Esc, NONE)],
        label: "Esc",
        action: "back to the chat input",
        footer: false,
    },
    KeyDoc {
        scope: KeyScope::Global,
        keys: &[
            (KeyCode::Up, NONE),
            (KeyCode::Down, NONE),
            (KeyCode::Char('k'), NONE),
            (KeyCode::Char('j'), NONE),
        ],
        label: "↑↓ j k",
        action: "move / scroll",
        footer: true,
    },
    KeyDoc {
        scope: KeyScope::Global,
        keys: &[(KeyCode::Char('g'), NONE), (KeyCode::Char('G'), SHIFT)],
        label: "g G",
        action: "top / bottom",
        footer: false,
    },
    KeyDoc {
        scope: KeyScope::Global,
        keys: &[],
        label: "PgUp PgDn",
        action: "scroll the panel under focus or the mouse",
        footer: false,
    },
    KeyDoc {
        scope: KeyScope::Work,
        keys: &[(KeyCode::Enter, NONE)],
        label: "Enter",
        action: "on a header: chat with that window · on a task: full detail",
        footer: true,
    },
    KeyDoc {
        scope: KeyScope::Work,
        keys: &[(KeyCode::Char(' '), NONE)],
        label: "Space",
        action: "fold/unfold; expand a task's output",
        footer: true,
    },
    KeyDoc {
        scope: KeyScope::Work,
        keys: &[(KeyCode::Char('f'), NONE)],
        label: "f",
        action: "this project / all projects",
        footer: true,
    },
    KeyDoc {
        scope: KeyScope::Work,
        keys: &[(KeyCode::Char('c'), NONE)],
        label: "c",
        action: "cancel the selected operation",
        footer: true,
    },
    KeyDoc {
        scope: KeyScope::Work,
        keys: &[(KeyCode::Char('p'), NONE)],
        label: "p",
        action: "pin the selected operation",
        footer: false,
    },
    KeyDoc {
        scope: KeyScope::Work,
        keys: &[(KeyCode::Char('a'), NONE)],
        label: "a",
        action: "ask for access again (on a denied operation)",
        footer: false,
    },
    KeyDoc {
        scope: KeyScope::Log,
        keys: &[(KeyCode::Enter, NONE), (KeyCode::Char('z'), NONE)],
        label: "Enter z",
        action: "zoom / restore the pane",
        footer: true,
    },
    KeyDoc {
        scope: KeyScope::Log,
        keys: &[(KeyCode::Char('w'), NONE)],
        label: "w",
        action: "toggle line wrap",
        footer: true,
    },
    KeyDoc {
        scope: KeyScope::Log,
        keys: &[(KeyCode::Char('l'), NONE)],
        label: "l",
        action: "switch log file",
        footer: true,
    },
    KeyDoc {
        scope: KeyScope::Log,
        keys: &[(KeyCode::Char('a'), NONE)],
        label: "a",
        action: "approve a pending symlink",
        footer: false,
    },
    KeyDoc {
        scope: KeyScope::Chat,
        keys: &[(KeyCode::Enter, NONE)],
        label: "Enter",
        action: "send",
        footer: true,
    },
    KeyDoc {
        scope: KeyScope::Chat,
        keys: &[(KeyCode::Enter, SHIFT)],
        label: "Shift-Enter",
        action: "newline",
        footer: true,
    },
    KeyDoc {
        scope: KeyScope::Chat,
        keys: &[(KeyCode::Esc, NONE)],
        label: "Esc",
        action: "clear input; if empty, cancel the turn or leave chat",
        footer: true,
    },
    KeyDoc {
        scope: KeyScope::Chat,
        keys: &[(KeyCode::Up, NONE), (KeyCode::Down, NONE)],
        label: "↑↓",
        action: "earlier / later message (on the first / last line)",
        footer: false,
    },
    KeyDoc {
        scope: KeyScope::Chat,
        keys: &[(KeyCode::Char('t'), CTRL)],
        label: "Ctrl-T",
        action: "start `/run <tool>`",
        footer: false,
    },
    KeyDoc {
        scope: KeyScope::Chat,
        keys: &[(KeyCode::Char('/'), NONE)],
        label: "/",
        action: "commands (from an empty input)",
        footer: true,
    },
    KeyDoc {
        scope: KeyScope::Chat,
        keys: &[(KeyCode::Char('?'), NONE)],
        label: "?",
        action: "help (from an empty input)",
        footer: true,
    },
    KeyDoc {
        scope: KeyScope::Gate,
        keys: &[(KeyCode::Char('y'), NONE)],
        label: "y",
        action: "approve (a scope grant: read+write)",
        footer: false,
    },
    KeyDoc {
        scope: KeyScope::Gate,
        keys: &[(KeyCode::Char('a'), NONE)],
        label: "a",
        action: "always allow this tool here (web: always this domain)",
        footer: false,
    },
    KeyDoc {
        scope: KeyScope::Gate,
        keys: &[(KeyCode::Char('n'), NONE)],
        label: "n",
        action: "reject — Enter and Esc also deny a grant",
        footer: false,
    },
];

/// The documented keys for a scope, in table order.
pub fn key_docs(scope: KeyScope) -> impl Iterator<Item = &'static KeyDoc> {
    KEY_REFERENCE.iter().filter(move |d| d.scope == scope)
}

/// Convert a raw crossterm `KeyEvent` into an `Action`.
///
/// Checked in priority order:
/// 1. Command navigator (if visible)
/// 2. Log switcher modal (if open)
/// 3. Log filter (if active)
/// 4. Chat input (if in Chat mode and input is focused)
/// 5. Global / Monitor shortcuts
pub fn map_key(key: KeyEvent, focus: Focus, modal: &ModalState, log_filter_active: bool) -> Action {
    // Open overlays take key priority, in this order: navigator > log-file
    // switcher. (Help and the inline pickers are dispatched before map_key is
    // reached.)
    match modal {
        ModalState::Navigator(_) => return map_navigator_key(key),
        ModalState::LogFiles { .. } => return map_log_modal_key(key),
        ModalState::OperationDetail(_) => return map_op_detail_key(key),
        ModalState::LogLineDetail(_) => return map_log_line_detail_key(key),
        _ => {}
    }

    if log_filter_active {
        return map_filter_key(key);
    }

    // If the input box is focused, let it handle key inputs.
    if focus == Focus::Chat {
        return map_chat_input_key(key);
    }

    map_global_key(key, focus)
}

// ─── Chat input ───────────────────────────────────────────────────────────────

fn map_chat_input_key(key: KeyEvent) -> Action {
    use KeyCode::*;
    use KeyModifiers as KM;

    match (key.code, key.modifiers) {
        // Submit with Enter (no modifier).
        (Enter, KM::NONE) => Action::InputSubmit,
        // Real newline with Shift+Enter.
        (Enter, KM::SHIFT) => Action::InputNewline,
        // Ctrl-C always quits.
        (Char('c'), KM::CONTROL) => Action::Quit,
        // Esc clears input.
        (Esc, _) => Action::InputClear,
        // `/` at start of line opens the navigator.
        // Detected at Action dispatch time (app.rs) because we can't easily
        // check cursor position here; InputChar('/') is emitted and app.rs
        // intercepts it when the input is empty.
        (Backspace, _) => Action::InputBackspace,
        // Tab cycles focus to monitor panels while keeping input context.
        (Tab, KM::NONE) => Action::Tab,
        (BackTab, _) => Action::BackTab,
        (Char(c), KM::NONE) | (Char(c), KM::SHIFT) => Action::InputChar(c),
        _ => Action::Unknown,
    }
}

// ─── Navigator ────────────────────────────────────────────────────────────────

fn map_navigator_key(key: KeyEvent) -> Action {
    use KeyCode::*;
    use KeyModifiers as KM;

    match (key.code, key.modifiers) {
        (Char('c'), KM::CONTROL) => Action::Quit,
        (Esc, _) => Action::NavEsc,
        (Enter, _) => Action::NavSubmit,
        (Tab, _) => Action::NavComplete,
        (Up, _) | (Char('k'), KM::NONE) => Action::NavUp,
        (Down, _) | (Char('j'), KM::NONE) => Action::NavDown,
        (Backspace, _) => Action::NavBackspace,
        (Char(c), KM::NONE) | (Char(c), KM::SHIFT) => Action::NavChar(c),
        _ => Action::Unknown,
    }
}

// ─── Log Switcher Modal ───────────────────────────────────────────────────────

fn map_log_modal_key(key: KeyEvent) -> Action {
    use KeyCode::*;
    use KeyModifiers as KM;

    match (key.code, key.modifiers) {
        (Char('c'), KM::CONTROL) => Action::Quit,
        (Esc, _) => Action::CloseLogSwitcher,
        (Enter, _) => Action::SubmitLogSwitcher,
        (Up, _) | (Char('k'), KM::NONE) => Action::Up,
        (Down, _) | (Char('j'), KM::NONE) => Action::Down,
        _ => Action::Unknown,
    }
}

// ─── Operation detail overlay ─────────────────────────────────────────────────

/// Keys inside the full-screen operation detail view: close (Esc/q/Enter),
/// scroll (j/k/arrows, g/G), and cancel the viewed operation (c).
fn map_op_detail_key(key: KeyEvent) -> Action {
    use KeyCode::*;
    use KeyModifiers as KM;

    match (key.code, key.modifiers) {
        (Esc, _) | (Char('q'), KM::NONE) | (Enter, _) => Action::DetailClose,
        (Char('c'), KM::CONTROL) => Action::Quit,
        (Up, _) | (Char('k'), KM::NONE) => Action::Up,
        (Down, _) | (Char('j'), KM::NONE) => Action::Down,
        (Char('g'), KM::NONE) => Action::Top,
        (Char('G'), KM::SHIFT) | (Char('G'), KM::NONE) => Action::Bottom,
        (Char('c'), KM::NONE) => Action::CancelOp,
        _ => Action::Unknown,
    }
}

/// Keys for the log-line overlay: the same close/scroll vocabulary as the
/// operation overlay, minus `c` — there is no operation behind a log line to
/// cancel, and silently accepting the key would suggest otherwise.
fn map_log_line_detail_key(key: KeyEvent) -> Action {
    use KeyCode::*;
    use KeyModifiers as KM;

    match (key.code, key.modifiers) {
        (Char('c'), KM::NONE) => Action::Unknown,
        _ => map_op_detail_key(key),
    }
}

// ─── Global (monitor) keys ───────────────────────────────────────────────────

fn map_global_key(key: KeyEvent, focus: Focus) -> Action {
    use KeyCode::*;
    use KeyModifiers as KM;

    match (key.code, key.modifiers) {
        // Universal quit
        (Char('q'), KM::NONE) => Action::Quit,
        (Char('c'), KM::CONTROL) => Action::Quit,

        // Navigation
        (Up, _) | (Char('k'), KM::NONE) => Action::Up,
        (Down, _) | (Char('j'), KM::NONE) => Action::Down,
        (Char('g'), KM::NONE) => Action::Top,
        (Char('G'), KM::SHIFT) | (Char('G'), KM::NONE) => Action::Bottom,
        (Tab, KM::NONE) => Action::Tab,
        (BackTab, _) => Action::BackTab,

        // Log focus Enter triggers ToggleZoom; other panels trigger Enter
        (Enter, _) if focus == Focus::Log => Action::ToggleZoom,
        (Enter, _) => Action::Enter,

        // Log monitor specific hotkeys
        (Char('w'), KM::NONE) if focus == Focus::Log => Action::ToggleWrap,
        (Char('l'), KM::NONE) if focus == Focus::Log => Action::OpenLogSwitcher,
        (Char('a'), KM::NONE) if focus == Focus::Log => Action::ApproveSymlink,

        // Approval
        (Char('y'), KM::NONE) => Action::Approve,
        (Char('n'), KM::NONE) => Action::Reject,

        // Op actions (only meaningful when OpsDag is focused)
        (Char('c'), KM::NONE) if focus == Focus::Work => Action::CancelOp,
        (Char('p'), KM::NONE) if focus == Focus::Work => Action::PinOp,
        (Char('a'), KM::NONE) if focus == Focus::Work => Action::ReRaiseGrant,
        (Char('f'), KM::NONE) if focus == Focus::Work => Action::ToggleProjectFilter,
        // `i` is the documented global chat toggle ("Open or close the chat
        // pane") — it must not fire while `Focus::Chat` is reading literal
        // characters, but should work from any other pane, not just Work.
        (Char('i'), KM::NONE) if focus != Focus::Chat => Action::ToggleChat,
        (Char(' '), KM::NONE) if focus == Focus::Work => Action::ToggleNode,

        // Zoom the focused pane to full screen and back.
        (Char('z'), KM::NONE) if focus.is_zoomable() => Action::ToggleZoom,

        // Toggles
        (Char('?'), _) => Action::ToggleHelp,

        // Navigator via `/` in non-filter context
        (Char('/'), KM::NONE) if focus != Focus::Log => Action::OpenNavigator,

        // Log filter (`/` when Log pane is focused)
        (Char('/'), KM::NONE) if focus == Focus::Log => Action::StartFilter,

        (Esc, _) => Action::FocusChat,

        _ => Action::Unknown,
    }
}

fn map_filter_key(key: KeyEvent) -> Action {
    use KeyCode::*;
    use KeyModifiers as KM;

    match (key.code, key.modifiers) {
        (Char('c'), KM::CONTROL) => Action::Quit,
        (Esc, _) => Action::FilterEsc,
        (Enter, _) => Action::FilterEsc, // commit filter
        (Backspace, _) => Action::FilterBackspace,
        (Char(c), KM::NONE) | (Char(c), KM::SHIFT) => Action::FilterChar(c),
        _ => Action::Unknown,
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{CommandNavigator, Focus, ModalState};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    /// Build a KeyEvent with the given code and modifiers.
    fn k(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn documented(scopes: &[KeyScope], code: KeyCode) -> bool {
        KEY_REFERENCE
            .iter()
            .filter(|d| scopes.contains(&d.scope))
            .any(|d| d.keys.iter().any(|(c, _)| *c == code))
    }

    /// Every key the work view and log pane act on is in the help. The help
    /// was hand-written and drifted from the bindings; this is what stops it.
    #[test]
    fn every_bound_key_is_documented() {
        let mut universe: Vec<KeyEvent> = Vec::new();
        for c in ('a'..='z').chain('0'..='9').chain([' ', '/', '?']) {
            universe.push(k(KeyCode::Char(c), KeyModifiers::NONE));
        }
        for c in 'A'..='Z' {
            universe.push(k(KeyCode::Char(c), KeyModifiers::SHIFT));
        }
        for code in [
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Tab,
            KeyCode::BackTab,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Backspace,
        ] {
            universe.push(kn(code));
        }
        universe.push(k(KeyCode::Char('c'), KeyModifiers::CONTROL));

        for (focus, scope) in [(Focus::Work, KeyScope::Work), (Focus::Log, KeyScope::Log)] {
            for key in &universe {
                let action = map_key(*key, focus, &ModalState::None, false);
                if action == Action::Unknown {
                    continue;
                }
                assert!(
                    documented(&[KeyScope::Global, scope, KeyScope::Gate], key.code),
                    "{:?} does {action:?} in {focus:?} but is not in KEY_REFERENCE",
                    key.code
                );
            }
        }
    }

    /// Every documented work-view, log and global key does something there.
    #[test]
    fn every_documented_key_does_something() {
        for doc in KEY_REFERENCE {
            let focuses: &[Focus] = match doc.scope {
                KeyScope::Global => &[Focus::Work, Focus::Log],
                KeyScope::Work => &[Focus::Work],
                KeyScope::Log => &[Focus::Log],
                // Handled before map_key; checked in app.rs.
                KeyScope::Chat | KeyScope::Gate => continue,
            };
            for &(code, mods) in doc.keys {
                for &focus in focuses {
                    assert_ne!(
                        map_key(k(code, mods), focus, &ModalState::None, false),
                        Action::Unknown,
                        "`{}` is documented as '{}' but does nothing in {focus:?}",
                        doc.label,
                        doc.action
                    );
                }
            }
        }
    }

    /// Build a KeyEvent with no modifiers.
    fn kn(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn navigator_modal() -> ModalState {
        ModalState::Navigator(CommandNavigator::default())
    }

    fn logfiles_modal() -> ModalState {
        ModalState::LogFiles { selected: 0 }
    }

    fn none_modal() -> ModalState {
        ModalState::None
    }

    fn op_detail_modal() -> ModalState {
        ModalState::OperationDetail(crate::state::OperationDetailState {
            op_id: "op_1".into(),
            instance_id: None,
            scroll: 0,
        })
    }

    fn log_line_modal() -> ModalState {
        ModalState::LogLineDetail(crate::state::LogLineDetailState {
            text: "pid=1 role=bridge INFO something long".into(),
            scroll: 0,
        })
    }

    /// The log-line overlay shares the close/scroll vocabulary of the operation
    /// overlay, so muscle memory carries over between the two.
    #[test]
    fn log_line_detail_close_and_scroll_keys() {
        for (key, want) in [
            (kn(KeyCode::Esc), Action::DetailClose),
            (kn(KeyCode::Char('q')), Action::DetailClose),
            (kn(KeyCode::Enter), Action::DetailClose),
            (kn(KeyCode::Char('j')), Action::Down),
            (kn(KeyCode::Char('k')), Action::Up),
            (kn(KeyCode::Char('g')), Action::Top),
            (k(KeyCode::Char('G'), KeyModifiers::SHIFT), Action::Bottom),
        ] {
            assert_eq!(
                map_key(key, Focus::Log, &log_line_modal(), false),
                want,
                "key {key:?}"
            );
        }
    }

    /// `c` cancels the operation behind the *operation* overlay. There is no
    /// operation behind a log line, so the key must do nothing rather than
    /// silently imply one was cancelled.
    #[test]
    fn log_line_detail_does_not_borrow_the_cancel_key() {
        assert_eq!(
            map_key(kn(KeyCode::Char('c')), Focus::Log, &log_line_modal(), false),
            Action::Unknown
        );
        assert_eq!(
            map_key(
                kn(KeyCode::Char('c')),
                Focus::Log,
                &op_detail_modal(),
                false
            ),
            Action::CancelOp,
            "the operation overlay keeps its cancel key"
        );
    }

    // ─── Operation detail overlay dispatch (map_op_detail_key) ──────────────────

    #[test]
    fn op_detail_close_and_scroll_keys() {
        for (key, want) in [
            (kn(KeyCode::Esc), Action::DetailClose),
            (kn(KeyCode::Char('q')), Action::DetailClose),
            (kn(KeyCode::Enter), Action::DetailClose),
            (kn(KeyCode::Char('j')), Action::Down),
            (kn(KeyCode::Char('k')), Action::Up),
            (kn(KeyCode::Char('g')), Action::Top),
            (k(KeyCode::Char('G'), KeyModifiers::SHIFT), Action::Bottom),
            (kn(KeyCode::Char('c')), Action::CancelOp),
        ] {
            assert_eq!(
                map_key(key, Focus::Chat, &op_detail_modal(), false),
                want,
                "key {key:?}"
            );
        }
    }

    /// The overlay outranks chat-input mapping even while chat is focused —
    /// typing must not leak into the input box behind the overlay.
    #[test]
    fn op_detail_swallows_plain_chars() {
        assert_eq!(
            map_key(
                kn(KeyCode::Char('x')),
                Focus::Chat,
                &op_detail_modal(),
                false
            ),
            Action::Unknown
        );
    }

    /// Only the log pane zooms: the work view already fills the screen
    /// (SPEC R24.9).
    #[test]
    fn z_zooms_the_log_pane_only() {
        assert_eq!(
            map_key(kn(KeyCode::Char('z')), Focus::Log, &none_modal(), false),
            Action::ToggleZoom
        );
        assert_ne!(
            map_key(kn(KeyCode::Char('z')), Focus::Work, &none_modal(), false),
            Action::ToggleZoom,
            "there is nothing to zoom the work view out of"
        );
        // In chat focus 'z' is just a typed character.
        assert_eq!(
            map_key(kn(KeyCode::Char('z')), Focus::Chat, &none_modal(), false),
            Action::InputChar('z')
        );
    }

    #[test]
    fn space_folds_tree_node_in_ops_focus() {
        assert_eq!(
            map_key(kn(KeyCode::Char(' ')), Focus::Work, &none_modal(), false),
            Action::ToggleNode
        );
    }

    // ─── Navigator modal dispatch (map_navigator_key) ───────────────────────────

    #[test]
    fn navigator_esc() {
        assert_eq!(
            map_key(kn(KeyCode::Esc), Focus::Chat, &navigator_modal(), false),
            Action::NavEsc
        );
    }

    #[test]
    fn navigator_enter() {
        assert_eq!(
            map_key(kn(KeyCode::Enter), Focus::Chat, &navigator_modal(), false),
            Action::NavSubmit
        );
    }

    #[test]
    fn navigator_tab() {
        assert_eq!(
            map_key(kn(KeyCode::Tab), Focus::Chat, &navigator_modal(), false),
            Action::NavComplete
        );
    }

    #[test]
    fn navigator_up_and_k() {
        assert_eq!(
            map_key(kn(KeyCode::Up), Focus::Chat, &navigator_modal(), false),
            Action::NavUp
        );
        assert_eq!(
            map_key(
                kn(KeyCode::Char('k')),
                Focus::Chat,
                &navigator_modal(),
                false
            ),
            Action::NavUp
        );
    }

    #[test]
    fn navigator_down_and_j() {
        assert_eq!(
            map_key(kn(KeyCode::Down), Focus::Chat, &navigator_modal(), false),
            Action::NavDown
        );
        assert_eq!(
            map_key(
                kn(KeyCode::Char('j')),
                Focus::Chat,
                &navigator_modal(),
                false
            ),
            Action::NavDown
        );
    }

    #[test]
    fn navigator_backspace() {
        assert_eq!(
            map_key(
                kn(KeyCode::Backspace),
                Focus::Chat,
                &navigator_modal(),
                false
            ),
            Action::NavBackspace
        );
    }

    #[test]
    fn navigator_char_none_and_shift() {
        assert_eq!(
            map_key(
                kn(KeyCode::Char('x')),
                Focus::Chat,
                &navigator_modal(),
                false
            ),
            Action::NavChar('x')
        );
        assert_eq!(
            map_key(
                k(KeyCode::Char('X'), KeyModifiers::SHIFT),
                Focus::Chat,
                &navigator_modal(),
                false
            ),
            Action::NavChar('X')
        );
    }

    #[test]
    fn navigator_unknown_fallthrough() {
        // A control char that matches no arm.
        assert_eq!(
            map_key(
                k(KeyCode::Char('a'), KeyModifiers::CONTROL),
                Focus::Chat,
                &navigator_modal(),
                false
            ),
            Action::Unknown
        );
    }

    #[test]
    fn navigator_ctrl_c_quits() {
        // Ctrl-C is documented as a universal quit; the navigator modal must
        // not swallow it (matches the fix already applied to chat input,
        // global keys, and the op-detail overlay).
        assert_eq!(
            map_key(
                k(KeyCode::Char('c'), KeyModifiers::CONTROL),
                Focus::Chat,
                &navigator_modal(),
                false
            ),
            Action::Quit
        );
    }

    // ─── Log switcher modal dispatch (map_log_modal_key) ────────────────────────

    #[test]
    fn log_modal_esc() {
        assert_eq!(
            map_key(kn(KeyCode::Esc), Focus::Chat, &logfiles_modal(), false),
            Action::CloseLogSwitcher
        );
    }

    #[test]
    fn log_modal_enter() {
        assert_eq!(
            map_key(kn(KeyCode::Enter), Focus::Chat, &logfiles_modal(), false),
            Action::SubmitLogSwitcher
        );
    }

    #[test]
    fn log_modal_up_and_k() {
        assert_eq!(
            map_key(kn(KeyCode::Up), Focus::Chat, &logfiles_modal(), false),
            Action::Up
        );
        assert_eq!(
            map_key(
                kn(KeyCode::Char('k')),
                Focus::Chat,
                &logfiles_modal(),
                false
            ),
            Action::Up
        );
    }

    #[test]
    fn log_modal_down_and_j() {
        assert_eq!(
            map_key(kn(KeyCode::Down), Focus::Chat, &logfiles_modal(), false),
            Action::Down
        );
        assert_eq!(
            map_key(
                kn(KeyCode::Char('j')),
                Focus::Chat,
                &logfiles_modal(),
                false
            ),
            Action::Down
        );
    }

    #[test]
    fn log_modal_unknown_fallthrough() {
        assert_eq!(
            map_key(
                kn(KeyCode::Char('z')),
                Focus::Chat,
                &logfiles_modal(),
                false
            ),
            Action::Unknown
        );
    }

    #[test]
    fn log_modal_ctrl_c_quits() {
        assert_eq!(
            map_key(
                k(KeyCode::Char('c'), KeyModifiers::CONTROL),
                Focus::Chat,
                &logfiles_modal(),
                false
            ),
            Action::Quit
        );
    }

    // ─── Log filter dispatch (map_filter_key) ───────────────────────────────────

    #[test]
    fn filter_esc() {
        assert_eq!(
            map_key(kn(KeyCode::Esc), Focus::Log, &none_modal(), true),
            Action::FilterEsc
        );
    }

    #[test]
    fn filter_enter_commits() {
        assert_eq!(
            map_key(kn(KeyCode::Enter), Focus::Log, &none_modal(), true),
            Action::FilterEsc
        );
    }

    #[test]
    fn filter_backspace() {
        assert_eq!(
            map_key(kn(KeyCode::Backspace), Focus::Log, &none_modal(), true),
            Action::FilterBackspace
        );
    }

    #[test]
    fn filter_char_none_and_shift() {
        assert_eq!(
            map_key(kn(KeyCode::Char('f')), Focus::Log, &none_modal(), true),
            Action::FilterChar('f')
        );
        assert_eq!(
            map_key(
                k(KeyCode::Char('F'), KeyModifiers::SHIFT),
                Focus::Log,
                &none_modal(),
                true
            ),
            Action::FilterChar('F')
        );
    }

    #[test]
    fn filter_unknown_fallthrough() {
        assert_eq!(
            map_key(kn(KeyCode::Tab), Focus::Log, &none_modal(), true),
            Action::Unknown
        );
    }

    #[test]
    fn filter_ctrl_c_quits() {
        assert_eq!(
            map_key(
                k(KeyCode::Char('c'), KeyModifiers::CONTROL),
                Focus::Log,
                &none_modal(),
                true
            ),
            Action::Quit
        );
    }

    // ─── Chat input dispatch (map_chat_input_key) ───────────────────────────────

    #[test]
    fn chat_input_submit() {
        assert_eq!(
            map_key(kn(KeyCode::Enter), Focus::Chat, &none_modal(), false),
            Action::InputSubmit
        );
    }

    #[test]
    fn chat_input_newline_shift_enter() {
        assert_eq!(
            map_key(
                k(KeyCode::Enter, KeyModifiers::SHIFT),
                Focus::Chat,
                &none_modal(),
                false
            ),
            Action::InputNewline
        );
    }

    #[test]
    fn chat_input_ctrl_c_quits() {
        assert_eq!(
            map_key(
                k(KeyCode::Char('c'), KeyModifiers::CONTROL),
                Focus::Chat,
                &none_modal(),
                false
            ),
            Action::Quit
        );
    }

    #[test]
    fn chat_input_esc_clears() {
        assert_eq!(
            map_key(kn(KeyCode::Esc), Focus::Chat, &none_modal(), false),
            Action::InputClear
        );
    }

    #[test]
    fn chat_input_backspace() {
        assert_eq!(
            map_key(kn(KeyCode::Backspace), Focus::Chat, &none_modal(), false),
            Action::InputBackspace
        );
    }

    #[test]
    fn chat_input_tab() {
        assert_eq!(
            map_key(kn(KeyCode::Tab), Focus::Chat, &none_modal(), false),
            Action::Tab
        );
    }

    #[test]
    fn chat_input_backtab() {
        assert_eq!(
            map_key(kn(KeyCode::BackTab), Focus::Chat, &none_modal(), false),
            Action::BackTab
        );
    }

    #[test]
    fn chat_input_char_none_and_shift() {
        assert_eq!(
            map_key(kn(KeyCode::Char('h')), Focus::Chat, &none_modal(), false),
            Action::InputChar('h')
        );
        assert_eq!(
            map_key(
                k(KeyCode::Char('H'), KeyModifiers::SHIFT),
                Focus::Chat,
                &none_modal(),
                false
            ),
            Action::InputChar('H')
        );
    }

    #[test]
    fn chat_input_slash_emits_inputchar() {
        // `/` in the chat box is emitted as InputChar; app.rs intercepts it.
        assert_eq!(
            map_key(kn(KeyCode::Char('/')), Focus::Chat, &none_modal(), false),
            Action::InputChar('/')
        );
    }

    #[test]
    fn chat_input_unknown_fallthrough() {
        // Ctrl + non-'c' char hits no arm in the chat input map.
        assert_eq!(
            map_key(
                k(KeyCode::Char('x'), KeyModifiers::CONTROL),
                Focus::Chat,
                &none_modal(),
                false
            ),
            Action::Unknown
        );
    }

    // ─── Global / monitor dispatch (map_global_key) ─────────────────────────────

    #[test]
    fn global_quit_q() {
        assert_eq!(
            map_key(kn(KeyCode::Char('q')), Focus::Work, &none_modal(), false),
            Action::Quit
        );
    }

    #[test]
    fn global_quit_ctrl_c() {
        assert_eq!(
            map_key(
                k(KeyCode::Char('c'), KeyModifiers::CONTROL),
                Focus::Work,
                &none_modal(),
                false
            ),
            Action::Quit
        );
    }

    #[test]
    fn global_up_arrow_and_k() {
        assert_eq!(
            map_key(kn(KeyCode::Up), Focus::Work, &none_modal(), false),
            Action::Up
        );
        assert_eq!(
            map_key(kn(KeyCode::Char('k')), Focus::Work, &none_modal(), false),
            Action::Up
        );
    }

    #[test]
    fn global_down_arrow_and_j() {
        assert_eq!(
            map_key(kn(KeyCode::Down), Focus::Work, &none_modal(), false),
            Action::Down
        );
        assert_eq!(
            map_key(kn(KeyCode::Char('j')), Focus::Work, &none_modal(), false),
            Action::Down
        );
    }

    #[test]
    fn global_top_g() {
        assert_eq!(
            map_key(kn(KeyCode::Char('g')), Focus::Work, &none_modal(), false),
            Action::Top
        );
    }

    #[test]
    fn global_bottom_g_shift_and_none() {
        assert_eq!(
            map_key(
                k(KeyCode::Char('G'), KeyModifiers::SHIFT),
                Focus::Work,
                &none_modal(),
                false
            ),
            Action::Bottom
        );
        assert_eq!(
            map_key(kn(KeyCode::Char('G')), Focus::Work, &none_modal(), false),
            Action::Bottom
        );
    }

    #[test]
    fn global_tab_and_backtab() {
        assert_eq!(
            map_key(kn(KeyCode::Tab), Focus::Work, &none_modal(), false),
            Action::Tab
        );
        assert_eq!(
            map_key(kn(KeyCode::BackTab), Focus::Work, &none_modal(), false),
            Action::BackTab
        );
    }

    #[test]
    fn global_enter_on_log_is_zoom() {
        assert_eq!(
            map_key(kn(KeyCode::Enter), Focus::Log, &none_modal(), false),
            Action::ToggleZoom
        );
    }

    #[test]
    fn global_enter_elsewhere_is_enter() {
        assert_eq!(
            map_key(kn(KeyCode::Enter), Focus::Work, &none_modal(), false),
            Action::Enter
        );
    }

    #[test]
    fn global_log_toggle_wrap_w() {
        assert_eq!(
            map_key(kn(KeyCode::Char('w')), Focus::Log, &none_modal(), false),
            Action::ToggleWrap
        );
    }

    #[test]
    fn global_log_open_switcher_l() {
        assert_eq!(
            map_key(kn(KeyCode::Char('l')), Focus::Log, &none_modal(), false),
            Action::OpenLogSwitcher
        );
    }

    #[test]
    fn global_log_approve_symlink_a() {
        assert_eq!(
            map_key(kn(KeyCode::Char('a')), Focus::Log, &none_modal(), false),
            Action::ApproveSymlink
        );
    }

    #[test]
    fn global_approve_y() {
        assert_eq!(
            map_key(kn(KeyCode::Char('y')), Focus::Work, &none_modal(), false),
            Action::Approve
        );
    }

    #[test]
    fn global_reject_n() {
        assert_eq!(
            map_key(kn(KeyCode::Char('n')), Focus::Work, &none_modal(), false),
            Action::Reject
        );
    }

    #[test]
    fn global_opsdag_cancel_c() {
        assert_eq!(
            map_key(kn(KeyCode::Char('c')), Focus::Work, &none_modal(), false),
            Action::CancelOp
        );
    }

    /// `d` was bound to an explicit no-op (ToggleDetail); the binding is gone
    /// and the key falls through as Unknown. (`a`, formerly the no-op AwaitOp,
    /// now carries the denied-row grant re-raise — see below.)
    #[test]
    fn removed_noop_binding_falls_through() {
        assert_eq!(
            map_key(kn(KeyCode::Char('d')), Focus::Work, &none_modal(), false),
            Action::Unknown
        );
    }

    /// `a` on the tasks pane asks for access to a denied operation's path.
    #[test]
    fn opsdag_a_reraises_the_grant_question() {
        assert_eq!(
            map_key(kn(KeyCode::Char('a')), Focus::Work, &none_modal(), false),
            Action::ReRaiseGrant
        );
    }

    #[test]
    fn global_opsdag_pin_p() {
        assert_eq!(
            map_key(kn(KeyCode::Char('p')), Focus::Work, &none_modal(), false),
            Action::PinOp
        );
    }

    #[test]
    fn global_toggle_help() {
        assert_eq!(
            map_key(kn(KeyCode::Char('?')), Focus::Work, &none_modal(), false),
            Action::ToggleHelp
        );
    }

    #[test]
    /// The `:` command palette was removed: it was a reachable but invisible
    /// mode that swallowed keys and only ever logged "Command: <text>".
    fn global_colon_no_longer_opens_palette() {
        assert_eq!(
            map_key(kn(KeyCode::Char(':')), Focus::Work, &none_modal(), false),
            Action::Unknown
        );
    }

    #[test]
    fn global_slash_opens_navigator_when_not_log() {
        assert_eq!(
            map_key(kn(KeyCode::Char('/')), Focus::Work, &none_modal(), false),
            Action::OpenNavigator
        );
    }

    #[test]
    fn global_slash_starts_filter_when_log() {
        assert_eq!(
            map_key(kn(KeyCode::Char('/')), Focus::Log, &none_modal(), false),
            Action::StartFilter
        );
    }

    #[test]
    fn global_esc_focuses_chat() {
        assert_eq!(
            map_key(kn(KeyCode::Esc), Focus::Work, &none_modal(), false),
            Action::FocusChat
        );
    }

    #[test]
    fn global_i_toggles_chat_from_work_and_log_but_not_from_chat() {
        // `i` is the documented global chat toggle — it must work from Work
        // and Log focus. From Chat focus it never reaches `map_global_key`
        // at all (map_key routes Focus::Chat to map_chat_input_key first),
        // where a literal 'i' must be typed instead.
        assert_eq!(
            map_key(kn(KeyCode::Char('i')), Focus::Work, &none_modal(), false),
            Action::ToggleChat
        );
        assert_eq!(
            map_key(kn(KeyCode::Char('i')), Focus::Log, &none_modal(), false),
            Action::ToggleChat
        );
        assert_eq!(
            map_key(kn(KeyCode::Char('i')), Focus::Chat, &none_modal(), false),
            Action::InputChar('i')
        );
    }

    #[test]
    fn global_unknown_fallthrough() {
        // 'x' with no modifier matches no global arm ('z' is now zoom).
        assert_eq!(
            map_key(kn(KeyCode::Char('x')), Focus::Work, &none_modal(), false),
            Action::Unknown
        );
    }

    #[test]
    fn global_w_off_log_is_unknown() {
        // 'w' only special on Log focus; elsewhere falls through.
        assert_eq!(
            map_key(kn(KeyCode::Char('w')), Focus::Work, &none_modal(), false),
            Action::Unknown
        );
    }

    #[test]
    fn modal_priority_navigator_beats_filter_and_chat() {
        // Even with log_filter_active and Chat focus, an open Navigator modal wins.
        assert_eq!(
            map_key(kn(KeyCode::Esc), Focus::Chat, &navigator_modal(), true),
            Action::NavEsc
        );
    }

    #[test]
    fn filter_beats_chat_focus() {
        // log_filter_active takes priority over Chat focus when no modal is open.
        assert_eq!(
            map_key(kn(KeyCode::Char('a')), Focus::Chat, &none_modal(), true),
            Action::FilterChar('a')
        );
    }
}
