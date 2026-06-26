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

#[cfg(all(test, feature = "tui"))]
mod tests {
    use super::*;
    use crate::state::{ActivityStatus, LogLevel, OpStatus};
    use ratatui::style::{Color, Modifier, Style};

    #[test]
    fn new_sets_unicode_flag() {
        assert!(Theme::new(true).unicode);
        assert!(!Theme::new(false).unicode);
    }

    #[test]
    fn status_colour_methods() {
        let t = Theme::new(true);
        assert_eq!(
            t.running(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        );
        assert_eq!(t.success(), Style::default().fg(Color::Green));
        assert_eq!(
            t.failed(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        );
        assert_eq!(t.pending(), Style::default().fg(Color::Yellow));
        assert_eq!(t.waiting(), Style::default().fg(Color::DarkGray));
        assert_eq!(
            t.cancelled(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM)
        );
    }

    #[test]
    fn ui_chrome_methods() {
        let t = Theme::new(false);
        assert_eq!(
            t.header_bar(),
            Style::default().bg(Color::DarkGray).fg(Color::White)
        );
        assert_eq!(t.input_bg(), Style::default().bg(Color::Rgb(24, 28, 36)));
        assert_eq!(
            t.input_placeholder(),
            Style::default().fg(Color::Rgb(100, 110, 120))
        );
        assert_eq!(
            t.title(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            t.selected_item(),
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        );
        assert_eq!(t.normal(), Style::default());
        assert_eq!(t.dim(), Style::default().fg(Color::DarkGray));
        assert_eq!(
            t.healthy(),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            t.unhealthy(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        );
        assert_eq!(t.unknown_health(), Style::default().fg(Color::Yellow));
        assert_eq!(
            t.approval_border(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            t.approval_key(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        );
        assert_eq!(t.approval_note(), Style::default().fg(Color::Yellow));
        assert_eq!(
            t.footer(),
            Style::default().bg(Color::DarkGray).fg(Color::DarkGray)
        );
        assert_eq!(
            t.footer_key(),
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        );
    }

    #[test]
    fn border_and_scrollbar_methods() {
        let t = Theme::new(true);
        assert_eq!(t.border_focused(), Style::default().fg(Color::Cyan));
        assert_eq!(t.border_unfocused(), Style::default().fg(Color::DarkGray));
        assert_eq!(
            t.scrollbar_thumb(),
            Style::default().bg(Color::Rgb(110, 120, 132))
        );
        assert_eq!(
            t.scrollbar_track(),
            Style::default().bg(Color::Rgb(44, 50, 60))
        );
    }

    #[test]
    fn log_style_covers_every_level() {
        let t = Theme::new(true);
        assert_eq!(
            t.log_style(&LogLevel::Info),
            Style::default().fg(Color::White)
        );
        assert_eq!(
            t.log_style(&LogLevel::Warn),
            Style::default().fg(Color::Yellow)
        );
        assert_eq!(
            t.log_style(&LogLevel::Error),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            t.log_style(&LogLevel::Debug),
            Style::default().fg(Color::DarkGray)
        );
    }

    #[test]
    fn op_status_style_covers_every_variant() {
        let t = Theme::new(true);
        assert_eq!(t.op_status_style(&OpStatus::Running), t.running());
        assert_eq!(t.op_status_style(&OpStatus::Succeeded), t.success());
        assert_eq!(t.op_status_style(&OpStatus::Failed), t.failed());
        assert_eq!(t.op_status_style(&OpStatus::Pending), t.pending());
        assert_eq!(t.op_status_style(&OpStatus::Waiting), t.waiting());
        assert_eq!(t.op_status_style(&OpStatus::Cancelled), t.cancelled());
    }

    #[test]
    fn activity_status_style_covers_every_variant() {
        let t = Theme::new(false);
        assert_eq!(
            t.activity_status_style(&ActivityStatus::Running),
            t.running()
        );
        assert_eq!(
            t.activity_status_style(&ActivityStatus::Success),
            t.success()
        );
        assert_eq!(t.activity_status_style(&ActivityStatus::Failed), t.failed());
        assert_eq!(
            t.activity_status_style(&ActivityStatus::Cancelled),
            t.cancelled()
        );
    }
}
