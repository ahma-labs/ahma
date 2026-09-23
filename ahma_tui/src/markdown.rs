//! Render an assistant reply's Markdown as styled terminal lines.
//!
//! Models answer in Markdown. Shown raw, a reply is a thicket of `**`, `#` and
//! backtick fences; rendered, it has headings, emphasis, lists and code blocks
//! that read as code. The output is logical lines — the chat pane prefixes and
//! wraps them as it does any other entry.
//!
//! A reply that is still streaming is rendered the same way: an unclosed fence
//! simply renders the rest as code until the closing fence arrives.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme::Theme;

/// Render `text` into logical lines (not wrapped).
pub fn render(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut r = Renderer::new(theme);
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    for event in Parser::new_ext(text, options) {
        r.event(event);
    }
    r.finish()
}

struct Renderer<'t> {
    theme: &'t Theme,
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    /// Inline style modifiers in effect (emphasis, strong, heading…).
    styles: Vec<Style>,
    /// Open lists: `None` for bullets, `Some(n)` for the next ordinal.
    lists: Vec<Option<u64>>,
    in_code_block: bool,
    quote_depth: usize,
    /// A link's target, shown after its text.
    link: Option<String>,
}

impl<'t> Renderer<'t> {
    fn new(theme: &'t Theme) -> Self {
        Self {
            theme,
            lines: Vec::new(),
            current: Vec::new(),
            styles: Vec::new(),
            lists: Vec::new(),
            in_code_block: false,
            quote_depth: 0,
            link: None,
        }
    }

    fn style(&self) -> Style {
        self.styles
            .iter()
            .fold(self.theme.normal(), |acc, s| acc.patch(*s))
    }

    fn push_text(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        if self.current.is_empty() && self.quote_depth > 0 {
            let bar = if self.theme.unicode { "│ " } else { "| " };
            self.current
                .push(Span::styled(bar.repeat(self.quote_depth), self.theme.dim()));
        }
        self.current.push(Span::styled(text.to_string(), style));
    }

    fn end_line(&mut self) {
        let spans = std::mem::take(&mut self.current);
        self.lines.push(Line::from(spans));
    }

    /// A blank line between blocks, never two in a row and never leading.
    fn block_gap(&mut self) {
        if !self.current.is_empty() {
            self.end_line();
        }
        if self.lines.last().is_some_and(|l| !l.spans.is_empty()) {
            self.lines.push(Line::default());
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) if self.in_code_block => self.code_text(&t),
            Event::Text(t) => {
                let style = self.style();
                self.push_text(&t, style);
            }
            Event::Code(t) => {
                let style = self.theme.md_code();
                self.push_text(&t, style);
            }
            Event::SoftBreak => {
                let style = self.style();
                self.push_text(" ", style);
            }
            Event::HardBreak => self.end_line(),
            Event::Rule => {
                self.block_gap();
                let rule = if self.theme.unicode { "─" } else { "-" };
                self.current
                    .push(Span::styled(rule.repeat(24), self.theme.dim()));
                self.end_line();
            }
            Event::TaskListMarker(done) => {
                let mark = if done { "[x] " } else { "[ ] " };
                let style = self.style();
                self.push_text(mark, style);
            }
            // Raw HTML and footnotes are shown as their text.
            Event::Html(t) | Event::InlineHtml(t) => {
                let style = self.style();
                self.push_text(t.trim_end(), style);
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            // A paragraph inside a list item continues the item's line.
            Tag::Paragraph if self.lists.is_empty() => self.block_gap(),
            Tag::Heading { level, .. } => {
                self.block_gap();
                let mut style = self.theme.md_heading();
                if level == HeadingLevel::H1 {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                self.styles.push(style);
            }
            Tag::BlockQuote(_) => {
                self.block_gap();
                self.quote_depth += 1;
                self.styles.push(self.theme.md_quote());
            }
            Tag::CodeBlock(kind) => {
                self.block_gap();
                self.in_code_block = true;
                if let CodeBlockKind::Fenced(lang) = kind
                    && !lang.is_empty()
                {
                    self.current
                        .push(Span::styled(format!("  {lang}"), self.theme.dim()));
                    self.end_line();
                }
            }
            Tag::List(start) => {
                if self.lists.is_empty() {
                    self.block_gap();
                } else if !self.current.is_empty() {
                    self.end_line();
                }
                self.lists.push(start);
            }
            Tag::Item => {
                if !self.current.is_empty() {
                    self.end_line();
                }
                let depth = self.lists.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let bullet = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let s = format!("{indent}{n}. ");
                        *n += 1;
                        s
                    }
                    _ => {
                        let b = if self.theme.unicode { "•" } else { "-" };
                        format!("{indent}{b} ")
                    }
                };
                self.current.push(Span::styled(bullet, self.theme.dim()));
            }
            Tag::Emphasis => self
                .styles
                .push(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self
                .styles
                .push(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .styles
                .push(Style::default().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.link = Some(dest_url.to_string());
                self.styles.push(self.theme.md_link());
            }
            Tag::Table(_) => self.block_gap(),
            Tag::TableCell if !self.current.is_empty() => {
                let sep = if self.theme.unicode { " │ " } else { " | " };
                self.push_text(sep, self.theme.dim());
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph if self.lists.is_empty() => self.end_line(),
            TagEnd::Heading(_) => {
                self.styles.pop();
                self.end_line();
            }
            TagEnd::BlockQuote(_) => {
                self.styles.pop();
                if !self.current.is_empty() {
                    self.end_line();
                }
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                self.in_code_block = false;
                if !self.current.is_empty() {
                    self.end_line();
                }
            }
            TagEnd::List(_) => {
                self.lists.pop();
                if !self.current.is_empty() {
                    self.end_line();
                }
            }
            TagEnd::Item if !self.current.is_empty() => self.end_line(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some(url) = self.link.take() {
                    self.push_text(&format!(" ({url})"), self.theme.dim());
                }
            }
            TagEnd::TableRow | TagEnd::TableHead => self.end_line(),
            _ => {}
        }
    }

    /// Code block text: one tinted, indented line per source line, verbatim.
    fn code_text(&mut self, text: &str) {
        let style = self.theme.md_code();
        let mut pieces = text.split('\n').peekable();
        while let Some(piece) = pieces.next() {
            if pieces.peek().is_none() && piece.is_empty() {
                break;
            }
            self.current.push(Span::styled("  ", self.theme.normal()));
            self.current.push(Span::styled(piece.to_string(), style));
            if pieces.peek().is_some() {
                self.end_line();
            }
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        if !self.current.is_empty() {
            self.end_line();
        }
        while self.lines.last().is_some_and(|l| l.spans.is_empty()) {
            self.lines.pop();
        }
        self.lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| &*s.content).collect())
            .collect()
    }

    #[test]
    fn markdown_syntax_is_rendered_not_shown() {
        let theme = Theme::new(true);
        let out = render(
            "# Plan\n\nRun **cargo** with `--release`.\n\n- first\n- second\n",
            &theme,
        );
        assert_eq!(
            text(&out),
            vec![
                "Plan",
                "",
                "Run cargo with --release.",
                "",
                "• first",
                "• second"
            ]
        );
        let bold = out[2]
            .spans
            .iter()
            .find(|s| s.content == "cargo")
            .expect("bold run");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let code = out[2]
            .spans
            .iter()
            .find(|s| s.content == "--release")
            .expect("inline code");
        assert_eq!(code.style, theme.md_code());
    }

    #[test]
    fn a_code_block_keeps_its_lines_verbatim() {
        let theme = Theme::new(true);
        let out = render("```rust\nfn main() {\n    run();\n}\n```\n", &theme);
        assert_eq!(
            text(&out),
            vec!["  rust", "  fn main() {", "      run();", "  }"]
        );
    }

    /// Mid-stream the closing fence has not arrived yet; the rest renders as
    /// code rather than as mangled prose.
    #[test]
    fn an_unclosed_fence_renders_as_code_while_streaming() {
        let theme = Theme::new(true);
        let out = render("Here:\n\n```\nlet x = 1;\n", &theme);
        assert_eq!(text(&out).last().map(String::as_str), Some("  let x = 1;"));
    }

    #[test]
    fn ordered_and_nested_lists_are_numbered_and_indented() {
        let theme = Theme::new(false);
        let out = render("1. one\n2. two\n   - inner\n", &theme);
        assert_eq!(text(&out), vec!["1. one", "2. two", "  - inner"]);
    }
}
