//! Maps raw crossterm `KeyEvent`s to semantic `Action`s for the TUI event loop.

#[cfg(feature = "tui")]
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::state::{Focus, PaletteState};

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
    Reject,
    // Op actions
    CancelOp,
    AwaitOp,
    PinOp,
    // Toggles
    ToggleHelp,
    ToggleDetail,
    // Palette
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
    Unknown,
}

/// Convert a raw crossterm `KeyEvent` into an `Action`.
///
/// Palette mode and log-filter mode are checked first so those inputs do not
/// accidentally trigger global shortcuts.
#[cfg(feature = "tui")]
pub fn map_key(
    key: KeyEvent,
    focus: Focus,
    palette: &PaletteState,
    log_filter_active: bool,
) -> Action {
    if palette.visible {
        return map_palette_key(key);
    }

    if log_filter_active {
        return map_filter_key(key);
    }

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
        (Enter, _) => Action::Enter,

        // Approval (only meaningful when banner is visible; app handles guard)
        (Char('y'), KM::NONE) => Action::Approve,
        (Char('n'), KM::NONE) => Action::Reject,

        // Op actions (only meaningful when OpsDag is focused)
        (Char('c'), KM::NONE) if focus == Focus::OpsDag => Action::CancelOp,
        (Char('a'), KM::NONE) if focus == Focus::OpsDag => Action::AwaitOp,
        (Char('p'), KM::NONE) if focus == Focus::OpsDag => Action::PinOp,

        // Toggles
        (Char('?'), _) => Action::ToggleHelp,
        (Char('d'), KM::NONE) => Action::ToggleDetail,

        // Palette
        (Char(':'), _) => Action::OpenPalette,

        // Log filter
        (Char('/'), KM::NONE) if focus == Focus::Log => Action::StartFilter,

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
    _focus: Focus,
    _palette: &PaletteState,
    _log_filter_active: bool,
) -> Action {
    Action::Unknown
}
