//! Semantic colour palette — wraps ratatui `Style` construction so every panel
//! uses consistent colours without scattering magic constants everywhere.

#[cfg(feature = "tui")]
use ratatui::style::{Color, Modifier, Style};

#[cfg(feature = "tui")]
use crate::state::{ActivityStatus, LogLevel, OpStatus};

/// Semantic style provider.  Construct once and share as `&Theme`.
pub struct Theme {
    pub unicode: bool,
}

#[cfg(feature = "tui")]
impl Theme {
    pub fn new(unicode: bool) -> Self {
        Self { unicode }
    }

    // ── Status colours ────────────────────────────────────────────────────────

    pub fn running(&self) -> Style {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    }
    pub fn success(&self) -> Style {
        Style::default().fg(Color::Green)
    }
    pub fn failed(&self) -> Style {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    }
    pub fn pending(&self) -> Style {
        Style::default().fg(Color::Yellow)
    }
    pub fn waiting(&self) -> Style {
        Style::default().fg(Color::DarkGray)
    }
    pub fn cancelled(&self) -> Style {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM)
    }

    // ── UI chrome ─────────────────────────────────────────────────────────────

    pub fn header_bar(&self) -> Style {
        Style::default().bg(Color::DarkGray).fg(Color::White)
    }
    pub fn input_bg(&self) -> Style {
        Style::default().bg(Color::Rgb(24, 28, 36))
    }
    pub fn input_placeholder(&self) -> Style {
        Style::default().fg(Color::Rgb(100, 110, 120))
    }
    pub fn title(&self) -> Style {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    }
    pub fn selected_item(&self) -> Style {
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    }
    pub fn normal(&self) -> Style {
        Style::default()
    }
    pub fn dim(&self) -> Style {
        Style::default().fg(Color::DarkGray)
    }
    pub fn healthy(&self) -> Style {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    }
    pub fn unhealthy(&self) -> Style {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    }
    pub fn unknown_health(&self) -> Style {
        Style::default().fg(Color::Yellow)
    }
    /// Red accent for the approval banner border and title — a tasteful outline
    /// rather than a full-bleed red fill.
    pub fn approval_border(&self) -> Style {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    }
    /// Highlight for the actionable `[y]` / `[n]` / `[a]` key hints.
    pub fn approval_key(&self) -> Style {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    }
    /// Calm contextual note (e.g. "new workspace …") — informative, not alarming.
    pub fn approval_note(&self) -> Style {
        Style::default().fg(Color::Yellow)
    }
    pub fn footer(&self) -> Style {
        Style::default().bg(Color::DarkGray).fg(Color::DarkGray)
    }
    pub fn footer_key(&self) -> Style {
        Style::default()
            .bg(Color::DarkGray)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    }

    // ── Log colours ───────────────────────────────────────────────────────────

    pub fn log_style(&self, level: &LogLevel) -> Style {
        match level {
            LogLevel::Info => Style::default().fg(Color::White),
            LogLevel::Warn => Style::default().fg(Color::Yellow),
            LogLevel::Error => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            LogLevel::Debug => Style::default().fg(Color::DarkGray),
        }
    }

    // ── Derived: operation / activity status ─────────────────────────────────

    pub fn op_status_style(&self, status: &OpStatus) -> Style {
        match status {
            OpStatus::Running => self.running(),
            OpStatus::Succeeded => self.success(),
            OpStatus::Failed => self.failed(),
            OpStatus::Pending => self.pending(),
            OpStatus::Waiting => self.waiting(),
            OpStatus::Cancelled => self.cancelled(),
        }
    }

    pub fn activity_status_style(&self, status: &ActivityStatus) -> Style {
        match status {
            ActivityStatus::Running => self.running(),
            ActivityStatus::Success => self.success(),
            ActivityStatus::Failed => self.failed(),
            ActivityStatus::Cancelled => self.cancelled(),
        }
    }

    // ── Border styles ─────────────────────────────────────────────────────────

    pub fn border_focused(&self) -> Style {
        Style::default().fg(Color::Cyan)
    }
    pub fn border_unfocused(&self) -> Style {
        Style::default().fg(Color::DarkGray)
    }

    // ── Scrollbar ─────────────────────────────────────────────────────────────

    /// The moving thumb, rendered as a filled cell background rather than the
    /// default `█` glyph. Stacked `█` glyphs leave horizontal gaps between rows
    /// under terminal line-spacing, making the thumb look dashed; painting the
    /// cell background fills it edge-to-edge so the proportional thumb reads as
    /// one continuous bar.
    pub fn scrollbar_thumb(&self) -> Style {
        Style::default().bg(Color::Rgb(110, 120, 132))
    }
    /// The full-height groove behind the thumb. Keeping it visible (a darker
    /// fill) lets the thumb's length be read as a proportion of the whole.
    pub fn scrollbar_track(&self) -> Style {
        Style::default().bg(Color::Rgb(44, 50, 60))
    }
}

/// Stub used when the `tui` feature is disabled so the crate still compiles.
#[cfg(not(feature = "tui"))]
impl Theme {
    pub fn new(unicode: bool) -> Self {
        Self { unicode }
    }
}
