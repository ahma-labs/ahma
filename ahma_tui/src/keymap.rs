//! Maps raw crossterm `KeyEvent`s to semantic `Action`s for the TUI event loop.

#[cfg(feature = "tui")]
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::state::{Focus, Mode, PaletteState};

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
    AwaitOp,
    PinOp,
    // Toggles
    ToggleHelp,
    ToggleDetail,
    // Old palette (`:`)
    OpenPalette,
    PaletteChar(char),
    PaletteBackspace,
    PaletteComplete,
    PaletteEsc,
    PaletteSubmit,
    PaletteUp,
    PaletteDown,
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

/// Convert a raw crossterm `KeyEvent` into an `Action`.
///
/// Checked in priority order:
/// 1. Command navigator (if visible)
/// 2. Log switcher modal (if open)
/// 3. Log filter (if active)
/// 4. Chat input (if in Chat mode and input is focused)
/// 5. Global / Monitor shortcuts
#[cfg(feature = "tui")]
pub fn map_key(
    key: KeyEvent,
    _mode: Mode,
    focus: Focus,
    palette: &PaletteState,
    nav_visible: bool,
    log_filter_active: bool,
    log_files_modal_open: bool,
) -> Action {
    // Navigator has highest priority when open.
    if nav_visible {
        return map_navigator_key(key);
    }

    if log_files_modal_open {
        return map_log_modal_key(key);
    }

    if palette.visible {
        return map_palette_key(key);
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

#[cfg(feature = "tui")]
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

#[cfg(feature = "tui")]
fn map_navigator_key(key: KeyEvent) -> Action {
    use KeyCode::*;
    use KeyModifiers as KM;

    match (key.code, key.modifiers) {
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

#[cfg(feature = "tui")]
fn map_log_modal_key(key: KeyEvent) -> Action {
    use KeyCode::*;
    use KeyModifiers as KM;

    match (key.code, key.modifiers) {
        (Esc, _) => Action::CloseLogSwitcher,
        (Enter, _) => Action::SubmitLogSwitcher,
        (Up, _) | (Char('k'), KM::NONE) => Action::Up,
        (Down, _) | (Char('j'), KM::NONE) => Action::Down,
        _ => Action::Unknown,
    }
}

// ─── Global (monitor) keys ───────────────────────────────────────────────────

#[cfg(feature = "tui")]
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
        (Char('c'), KM::NONE) if focus == Focus::OpsDag => Action::CancelOp,
        (Char('a'), KM::NONE) if focus == Focus::OpsDag => Action::AwaitOp,
        (Char('p'), KM::NONE) if focus == Focus::OpsDag => Action::PinOp,

        // Toggles
        (Char('?'), _) => Action::ToggleHelp,
        (Char('d'), KM::NONE) => Action::ToggleDetail,

        // Old `:` palette (kept for backward compat in Monitor mode)
        (Char(':'), _) => Action::OpenPalette,

        // Navigator via `/` in non-filter context
        (Char('/'), KM::NONE) if focus != Focus::Log => Action::OpenNavigator,

        // Log filter (`/` when Log pane is focused)
        (Char('/'), KM::NONE) if focus == Focus::Log => Action::StartFilter,

        (Esc, _) => Action::FocusChat,

        _ => Action::Unknown,
    }
}

#[cfg(feature = "tui")]
fn map_palette_key(key: KeyEvent) -> Action {
    use KeyCode::*;
    use KeyModifiers as KM;

    match (key.code, key.modifiers) {
        (Esc, _) => Action::PaletteEsc,
        (Enter, _) => Action::PaletteSubmit,
        (Tab, _) => Action::PaletteComplete,
        (BackTab, _) => Action::PaletteDown,
        (Up, _) => Action::PaletteUp,
        (Down, _) => Action::PaletteDown,
        (Backspace, _) => Action::PaletteBackspace,
        (Char(c), KM::NONE) | (Char(c), KM::SHIFT) => Action::PaletteChar(c),
        _ => Action::Unknown,
    }
}

#[cfg(feature = "tui")]
fn map_filter_key(key: KeyEvent) -> Action {
    use KeyCode::*;
    use KeyModifiers as KM;

    match (key.code, key.modifiers) {
        (Esc, _) => Action::FilterEsc,
        (Enter, _) => Action::FilterEsc, // commit filter
        (Backspace, _) => Action::FilterBackspace,
        (Char(c), KM::NONE) | (Char(c), KM::SHIFT) => Action::FilterChar(c),
        _ => Action::Unknown,
    }
}

/// Stub when the `tui` feature is disabled.
#[cfg(not(feature = "tui"))]
pub fn map_key(
    _key: (),
    _mode: Mode,
    _focus: Focus,
    _palette: &PaletteState,
    _nav_visible: bool,
    _log_filter_active: bool,
    _log_files_modal_open: bool,
) -> Action {
    Action::Unknown
}
