//! Semantic colour palette — wraps ratatui `Style` construction so every panel
//! uses consistent colours without scattering magic constants everywhere.

use ratatui::style::{Color, Modifier, Style};

use crate::state::{ActivityStatus, LogLevel, OpStatus};

/// Semantic style provider.  Construct once and share as `&Theme`.
pub struct Theme {
    pub unicode: bool,
    /// Whether colour may be emitted at all. `false` under `NO_COLOR`, where
    /// every style keeps its modifiers (bold, dim) and loses its hues — the
    /// convention is about colour, not about flattening emphasis.
    pub color: bool,
}

impl Theme {
    pub fn new(unicode: bool) -> Self {
        Self {
            unicode,
            color: true,
        }
    }

    /// Construct with an explicit colour decision — used at startup so
    /// `NO_COLOR` is honoured (<https://no-color.org>).
    pub fn with_color(unicode: bool, color: bool) -> Self {
        Self { unicode, color }
    }

    /// Gate for every style this type hands out: under `NO_COLOR` the
    /// foreground and background are dropped and the modifiers kept, so the UI
    /// still distinguishes emphasis without emitting a single colour escape.
    /// One funnel means a new style method cannot forget the rule.
    fn c(&self, style: Style) -> Style {
        if self.color {
            style
        } else {
            Style::default().add_modifier(style.add_modifier)
        }
    }

    // ── Status colours ────────────────────────────────────────────────────────

    pub fn running(&self) -> Style {
        self.c({
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        })
    }
    pub fn success(&self) -> Style {
        self.c(Style::default().fg(Color::Green))
    }
    pub fn failed(&self) -> Style {
        self.c(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
    }
    pub fn pending(&self) -> Style {
        self.c(Style::default().fg(Color::Yellow))
    }
    pub fn waiting(&self) -> Style {
        self.c(Style::default().fg(Color::DarkGray))
    }
    /// "Protection depends on someone else": the sandbox chip's NESTED/DEFERRED
    /// states, where a host sandbox (Cursor, Docker, …) is the authority rather
    /// than ahma. Deliberately distinct from `pending()` — "ahma is starting up"
    /// (yellow) and "ahma is not enforcing at all" must not share a colour
    /// (SPEC R7.5: disclosure states what protection actually depends on).
    pub fn host_authority(&self) -> Style {
        self.c({
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD)
        })
    }
    pub fn cancelled(&self) -> Style {
        self.c({
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM)
        })
    }

    // ── UI chrome ─────────────────────────────────────────────────────────────

    pub fn header_bar(&self) -> Style {
        self.c(Style::default().bg(Color::DarkGray).fg(Color::White))
    }
    pub fn input_bg(&self) -> Style {
        self.c(Style::default().bg(Color::Rgb(24, 28, 36)))
    }
    pub fn input_placeholder(&self) -> Style {
        self.c(Style::default().fg(Color::Rgb(100, 110, 120)))
    }
    pub fn title(&self) -> Style {
        self.c({
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        })
    }
    pub fn selected_item(&self) -> Style {
        self.c({
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        })
    }
    pub fn normal(&self) -> Style {
        self.c(Style::default())
    }

    // ── Markdown (assistant replies) ─────────────────────────────────────────

    pub fn md_heading(&self) -> Style {
        self.c(Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD))
    }
    /// Inline `code` and code blocks: a tinted background so code reads as
    /// code even under `NO_COLOR` loses the tint (it keeps nothing else).
    pub fn md_code(&self) -> Style {
        self.c(Style::default()
            .fg(Color::Rgb(230, 219, 116))
            .bg(Color::Rgb(40, 44, 52)))
    }
    pub fn md_quote(&self) -> Style {
        self.c(Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::ITALIC))
    }
    pub fn md_link(&self) -> Style {
        self.c(Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::UNDERLINED))
    }
    pub fn dim(&self) -> Style {
        self.c(Style::default().fg(Color::DarkGray))
    }
    pub fn healthy(&self) -> Style {
        self.c({
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        })
    }
    pub fn unhealthy(&self) -> Style {
        self.c(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
    }
    pub fn unknown_health(&self) -> Style {
        self.c(Style::default().fg(Color::Yellow))
    }
    /// Accent for an approval banner's border and title.
    ///
    /// Deliberately *not* red. Being asked to approve something is a normal,
    /// expected part of using ahma — red is reserved for things that went
    /// wrong (a crashed operation, a dead server, a denial), and spending it on
    /// a routine question teaches the user to discount the colour. Cyan matches
    /// the focused-border accent, which is what an approval prompt is: the
    /// thing currently wanting attention.
    pub fn approval_border(&self) -> Style {
        self.c(Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD))
    }
    /// Highlight for the actionable `[y]` / `[n]` / `[a]` key hints.
    pub fn approval_key(&self) -> Style {
        self.c(Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD))
    }
    /// Calm contextual note (e.g. "new workspace …") — informative, not alarming.
    pub fn approval_note(&self) -> Style {
        self.c(Style::default().fg(Color::Yellow))
    }
    /// Footer key *descriptions*. Must contrast with the DarkGray footer bar:
    /// this was DarkGray-on-DarkGray for a while, which rendered every hint
    /// description invisible — the primary discoverability surface showed only
    /// the key chips with blank meanings next to them.
    pub fn footer(&self) -> Style {
        self.c(Style::default().bg(Color::DarkGray).fg(Color::Gray))
    }
    pub fn footer_key(&self) -> Style {
        self.c({
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        })
    }

    // ── Log colours ───────────────────────────────────────────────────────────

    pub fn log_style(&self, level: &LogLevel) -> Style {
        self.c({
            match level {
                LogLevel::Info => Style::default().fg(Color::White),
                LogLevel::Warn => Style::default().fg(Color::Yellow),
                LogLevel::Error => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                LogLevel::Debug => Style::default().fg(Color::DarkGray),
            }
        })
    }

    // ── Derived: operation / activity status ─────────────────────────────────

    pub fn op_status_style(&self, status: &OpStatus) -> Style {
        self.c({
            match status {
                OpStatus::Running => self.running(),
                OpStatus::Succeeded => self.success(),
                OpStatus::Failed | OpStatus::Denied => self.failed(),
                OpStatus::Pending => self.pending(),
                OpStatus::Waiting => self.waiting(),
                OpStatus::Cancelled => self.cancelled(),
                // Dimmed, not red: an interruption is an absence of knowledge,
                // not an observed failure.
                OpStatus::Interrupted => self.waiting(),
            }
        })
    }

    pub fn activity_status_style(&self, status: &ActivityStatus) -> Style {
        self.c({
            match status {
                ActivityStatus::Running => self.running(),
                ActivityStatus::Success => self.success(),
                ActivityStatus::Failed => self.failed(),
                ActivityStatus::Cancelled => self.cancelled(),
            }
        })
    }

    // ── Border styles ─────────────────────────────────────────────────────────

    // ── The work view's chrome (SPEC R24.9) ─────────────────────────────────
    //
    // A rule, not a box: the section headers *are* the structure, so a border
    // around them would be a second frame drawn around a frame.

    /// The `───` fill of a section rule.
    pub fn section_rule(&self) -> Style {
        self.c(Style::default().fg(Color::DarkGray))
    }
    /// The part of a rule beside a section that is running something.
    pub fn section_rule_live(&self) -> Style {
        self.c(Style::default().fg(Color::Cyan))
    }
    /// A section's name.
    pub fn section_title(&self) -> Style {
        self.c(Style::default().add_modifier(Modifier::BOLD))
    }
    /// The selected section's name.
    pub fn section_title_selected(&self) -> Style {
        self.c({
            Style::default()
                .bg(Color::Rgb(38, 42, 52))
                .add_modifier(Modifier::BOLD)
        })
    }
    /// The selected row inside a section. Subtler than `selected_item`, which
    /// is a bordered pane's highlight; here the whole screen is the view, so a
    /// heavy bar on every frame is noise.
    pub fn work_row_selected(&self) -> Style {
        self.c(Style::default().bg(Color::Rgb(38, 42, 52)))
    }

    pub fn border_focused(&self) -> Style {
        self.c(Style::default().fg(Color::Cyan))
    }
    pub fn border_unfocused(&self) -> Style {
        self.c(Style::default().fg(Color::DarkGray))
    }

    // ── Scrollbar ─────────────────────────────────────────────────────────────

    /// The moving thumb, rendered as a filled cell background rather than the
    /// default `█` glyph. Stacked `█` glyphs leave horizontal gaps between rows
    /// under terminal line-spacing, making the thumb look dashed; painting the
    /// cell background fills it edge-to-edge so the proportional thumb reads as
    /// one continuous bar.
    pub fn scrollbar_thumb(&self) -> Style {
        self.c(Style::default().bg(Color::Rgb(110, 120, 132)))
    }
    /// The full-height groove behind the thumb. Keeping it visible (a darker
    /// fill) lets the thumb's length be read as a proportion of the whole.
    pub fn scrollbar_track(&self) -> Style {
        self.c(Style::default().bg(Color::Rgb(44, 50, 60)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ActivityStatus, LogLevel, OpStatus};
    use ratatui::style::{Color, Modifier, Style};

    #[test]
    fn new_sets_unicode_flag() {
        assert!(Theme::new(true).unicode);
        assert!(!Theme::new(false).unicode);
        assert!(Theme::new(true).color, "colour is on unless refused");
    }

    #[test]
    fn work_row_selected_style() {
        let theme = Theme::new(true);
        assert_eq!(
            theme.work_row_selected(),
            theme.c(Style::default().bg(Color::Rgb(38, 42, 52)))
        );
    }

    /// Under `NO_COLOR` every style keeps its modifiers and loses its hues.
    /// The convention is about colour; dropping bold/dim too would flatten the
    /// emphasis a monochrome terminal relies on to show structure.
    #[test]
    fn no_color_strips_hues_but_keeps_emphasis() {
        let plain = Theme::with_color(true, false);

        assert_eq!(plain.success(), Style::default());
        assert_eq!(
            plain.failed(),
            Style::default().add_modifier(Modifier::BOLD),
            "bold survives; red does not"
        );
        assert_eq!(plain.header_bar(), Style::default(), "backgrounds go too");
        assert_eq!(
            plain.log_style(&LogLevel::Error),
            Style::default().add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            plain.op_status_style(&OpStatus::Running),
            Style::default().add_modifier(Modifier::BOLD)
        );

        // And the coloured theme is unaffected.
        assert_eq!(
            Theme::new(true).success(),
            Style::default().fg(Color::Green)
        );
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
        // An approval prompt is a routine question, not a failure: red stays
        // reserved for things that actually went wrong, so the colour keeps
        // meaning something when it does appear.
        assert_eq!(
            t.approval_border(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        );
        assert_ne!(
            t.approval_border(),
            t.failed(),
            "approval must not look like failure"
        );
        assert_eq!(
            t.approval_key(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        );
        assert_eq!(t.approval_note(), Style::default().fg(Color::Yellow));
        // The description text must not match the bar background — fg == bg
        // made every footer hint invisible (only the key chips rendered).
        assert_eq!(
            t.footer(),
            Style::default().bg(Color::DarkGray).fg(Color::Gray)
        );
        assert_ne!(
            t.footer().fg,
            t.footer().bg,
            "footer text must be readable on the footer bar"
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
