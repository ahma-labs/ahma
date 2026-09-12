//! One operation identity, computed at the source (SPEC R24.7).
//!
//! ## The bug this module exists to make impossible
//!
//! The TUI used to name operations by *reverse-engineering* them: it tried to
//! parse a command out of the operation's JSON args, then out of a free-text
//! "Execute …" sentence, then — failing both — out of the operation **id**, so a
//! row could end up reading `op_41_echo_hello` or, worse, just `run_terminal_command`
//! for every row in the list. A user opening `ahma tui` after their IDE had been
//! working for ten minutes saw a screen of near-identical, meaningless labels.
//!
//! That is not a formatting bug and no formatter can fix it. The information was
//! never on the wire. The server knows exactly what command it ran; the observer
//! is left guessing. So the fix is to **send what we know**: the server computes a
//! human title once, and every surface renders the same one.
//!
//! ## The identity line
//!
//! One line, rendered identically in the chat history, the monitor rows, the grant
//! prompts, and the per-operation log names:
//!
//! ```text
//! ⚙ cargo nextest run -p ahma_core · ahma · running 12s
//! ✓ cargo nextest run -p ahma_core · ahma · exit 0 · 41s
//! ✗ touch /etc/foo · ahma · denied: outside sandbox scope
//! ```
//!
//! Status glyph, what ran, where, and how it ended. The `origin` badge (`[cursor]`,
//! `[tui]`) is added only when more than one origin is in view — which is what makes
//! an interleaved timeline of IDE work and TUI work legible instead of confusing.

use serde_json::{Map, Value};

/// Longest title we will render before eliding. Long enough for a real command
/// (`cargo nextest run -p ahma_core --no-fail-fast`), short enough to leave room
/// for the directory and status on one terminal line.
const MAX_TITLE: usize = 80;

/// Compute the human title for an operation — the **one** place this is decided.
///
/// Called server-side, where the command is actually known. `args` is the tool's
/// argument object as sent by the MCP client.
///
/// For `run_terminal_command` (and anything else carrying a `command` argument)
/// the title *is* the command, because that is what the user thinks they ran. For
/// other tools it is the tool name plus its most salient argument, which is still
/// far more than the bare tool name the TUI used to fall back to.
pub fn title_for(tool_name: &str, args: Option<&Map<String, Value>>) -> String {
    if let Some(cmd) = args.and_then(command_arg) {
        return clip(&first_line(&cmd));
    }
    match args.and_then(salient_arg) {
        Some(arg) => clip(&format!("{tool_name} {}", first_line(&arg))),
        None => tool_name.to_string(),
    }
}

/// [`title_for`], for callers holding a whole JSON value rather than an arg map.
pub fn title_for_value(tool_name: &str, args: Option<&Value>) -> String {
    title_for(tool_name, args.and_then(Value::as_object))
}

/// The first non-empty (trimmed) string value among `keys`, in order.
/// Shared by [`command_arg`] and [`salient_arg`], which differ only in which
/// keys they look for.
fn first_nonempty_str(args: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(s) = args.get(*key).and_then(Value::as_str)
            && !s.trim().is_empty()
        {
            return Some(s.trim().to_string());
        }
    }
    None
}

/// The literal command a tool was asked to run, if it has one.
fn command_arg(args: &Map<String, Value>) -> Option<String> {
    first_nonempty_str(args, &["command", "cmd", "command_line"])
}

/// The argument most worth showing beside a tool's name — a path, a pattern, a
/// query. Deliberately a small, ordered list rather than "the first string we
/// find": a stable choice is what makes rows comparable at a glance.
fn salient_arg(args: &Map<String, Value>) -> Option<String> {
    first_nonempty_str(
        args,
        &[
            "path",
            "file",
            "file_path",
            "query",
            "pattern",
            "url",
            "target",
            "name",
        ],
    )
}

/// A command may be a whole shell script. The first non-empty line is what the
/// user recognizes; the rest belongs in the detail pane, not the row.
fn first_line(s: &str) -> String {
    let mut lines = s.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next().unwrap_or("").to_string();
    if lines.next().is_some() {
        format!("{first} …")
    } else {
        first
    }
}

/// Clip to [`MAX_TITLE`], on a character boundary, with an ellipsis.
fn clip(s: &str) -> String {
    if s.chars().count() <= MAX_TITLE {
        return s.to_string();
    }
    let cut: String = s.chars().take(MAX_TITLE - 1).collect();
    format!("{}…", cut.trim_end())
}

/// How an operation ended (or hasn't yet) — the tail of the identity line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpOutcome {
    /// Still running, for this many seconds.
    Running {
        /// Seconds elapsed so far.
        elapsed_secs: u64,
    },
    /// Finished with an exit code (when the runner reported one) and a duration.
    Finished {
        /// Process exit code, when known. `None` for operations that are not
        /// processes (or a runner that reported none) — and shown as the status
        /// word rather than a fabricated `exit 0`.
        exit_code: Option<i64>,
        /// Status word from the wire: `Completed`, `Failed`, `Cancelled`, `TimedOut`.
        status: String,
        /// Wall-clock duration in milliseconds.
        duration_ms: u64,
    },
    /// The sandbox blocked it. Carries the reason so the row *is* the explanation.
    Denied {
        /// Short reason, e.g. "outside sandbox scope".
        reason: String,
    },
}

/// Everything a surface needs to render one operation, identically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpIdentity<'a> {
    /// The human title (from the wire; see [`title_for`]).
    pub title: &'a str,
    /// Working directory, if known. Rendered as its final component — the full
    /// path belongs in the detail pane, not in every row.
    pub cwd: Option<&'a str>,
    /// Which session initiated it (`cursor`, `claude-code`, `tui`, `cli`, `hook`).
    pub origin: Option<&'a str>,
    /// How it ended, or that it hasn't.
    pub outcome: OpOutcome,
}

impl OpOutcome {
    /// The outcome as a sentence, for surfaces that narrate rather than tabulate
    /// (an operation card's end line, a detail header) — the prose counterpart
    /// of [`OpIdentity::glyph`].
    ///
    /// It lives here, beside `glyph` and `render`, because every surface that
    /// describes an outcome must agree about `Denied`. Spelled locally, each one
    /// wrote a three-arm match over the *display* status with a `_` fallback,
    /// and a denial — which is exactly the outcome the user most needs named —
    /// fell into it and was reported as an ordinary finish (SPEC R24.7,
    /// R-PERM.7).
    pub fn friendly_phrase(&self) -> String {
        match self {
            // Not terminal: a caller asking for an end phrase while the
            // operation still runs gets the neutral word, not a verdict.
            OpOutcome::Running { .. } => "Finished".to_string(),
            OpOutcome::Denied { reason } => format!("Denied: {reason}"),
            OpOutcome::Finished {
                status, exit_code, ..
            } => match status.as_str() {
                "Completed" => "Finished successfully".to_string(),
                "Cancelled" => "Cancelled".to_string(),
                "TimedOut" => "Timed out".to_string(),
                // `Failed` and anything a future producer sends. An exit code is
                // the most precise thing available, so prefer it.
                _ => match exit_code {
                    Some(code) => format!("Failed (exit {code})"),
                    None => "Failed".to_string(),
                },
            },
        }
    }
}

impl OpIdentity<'_> {
    /// The status glyph. Deliberately the same three characters everywhere, so a
    /// user learns them once.
    pub fn glyph(&self) -> &'static str {
        match &self.outcome {
            OpOutcome::Running { .. } => "⚙",
            OpOutcome::Denied { .. } => "✗",
            OpOutcome::Finished {
                exit_code, status, ..
            } => {
                let ok = exit_code.map(|c| c == 0).unwrap_or(status == "Completed");
                if ok { "✓" } else { "✗" }
            }
        }
    }

    /// Render the identity line.
    ///
    /// `show_origin` is set by the caller when more than one origin is present in
    /// the visible set — a badge on every row when everything came from the same
    /// place is noise, and noise is what made the old labels useless.
    pub fn render(&self, show_origin: bool) -> String {
        let mut out = format!("{} {}", self.glyph(), self.title);

        if let Some(cwd) = self.cwd
            && let Some(name) = dir_label(cwd)
        {
            out.push_str(" · ");
            out.push_str(&name);
        }

        if show_origin && let Some(origin) = self.origin {
            out.push_str(" [");
            out.push_str(origin);
            out.push(']');
        }

        out.push_str(" · ");
        match &self.outcome {
            OpOutcome::Running { elapsed_secs } => {
                out.push_str(&format!("running {}", human_duration_secs(*elapsed_secs)));
            }
            OpOutcome::Denied { reason } => {
                out.push_str("denied: ");
                out.push_str(reason);
            }
            OpOutcome::Finished {
                exit_code,
                status,
                duration_ms,
            } => {
                match exit_code {
                    // An exit code is the most precise thing we can say, so say it.
                    Some(code) => out.push_str(&format!("exit {code}")),
                    // No exit code (cancelled, timed out, not a process): say the
                    // status word rather than inventing an `exit 0` that never was.
                    None => out.push_str(&status.to_lowercase()),
                }
                out.push_str(" · ");
                out.push_str(&human_duration_ms(*duration_ms));
            }
        }
        out
    }
}

/// The final component of a working directory — `ahma`, not
/// `/Users/me/github/ahma`. The full path is one keystroke away in the detail
/// pane; in a row it would crowd out the thing the user actually came to read.
fn dir_label(cwd: &str) -> Option<String> {
    std::path::Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
}

/// `41s`, `2m 03s`, `1h 12m`.
fn human_duration_secs(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {:02}s", s / 60, s % 60),
        s => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// Sub-second durations keep one decimal (`0.4s`); above that, [`human_duration_secs`].
fn human_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        return format!("{:.1}s", ms as f64 / 1000.0);
    }
    human_duration_secs(ms / 1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_terminal_command_is_titled_by_its_command() {
        // The whole point: a user who ran `cargo nextest run` should see
        // `cargo nextest run`, not `run_terminal_command` and not `op_41_cargo`.
        let args = json!({"command": "cargo nextest run -p ahma_core", "working_directory": "/x"});
        assert_eq!(
            title_for_value("run_terminal_command", Some(&args)),
            "cargo nextest run -p ahma_core"
        );
    }

    #[test]
    fn a_multiline_script_is_titled_by_its_first_line() {
        let args = json!({"command": "cargo build\ncargo test\n"});
        assert_eq!(
            title_for_value("run_terminal_command", Some(&args)),
            "cargo build …"
        );
    }

    #[test]
    fn a_very_long_command_is_clipped_on_a_char_boundary() {
        let long = "cargo nextest run ".repeat(20);
        let args = json!({ "command": long });
        let title = title_for_value("run_terminal_command", Some(&args));
        assert!(title.chars().count() <= MAX_TITLE, "clipped to fit a row");
        assert!(title.ends_with('…'), "and says so");
    }

    #[test]
    fn other_tools_get_their_most_salient_argument() {
        // Still far more useful than a bare tool name repeated down the column.
        let args = json!({"path": "/src/main.rs"});
        assert_eq!(
            title_for_value("read_file", Some(&args)),
            "read_file /src/main.rs"
        );

        let args = json!({"query": "fn main"});
        assert_eq!(
            title_for_value("grep_search", Some(&args)),
            "grep_search fn main"
        );
    }

    #[test]
    fn a_tool_with_no_useful_args_falls_back_to_its_name() {
        assert_eq!(title_for("status", None), "status");
        assert_eq!(title_for_value("status", Some(&json!({}))), "status");
        // An empty command string is not a title.
        assert_eq!(
            title_for_value("run_terminal_command", Some(&json!({"command": "  "}))),
            "run_terminal_command"
        );
    }

    #[test]
    fn a_running_operation_reads_as_running() {
        let id = OpIdentity {
            title: "cargo nextest run",
            cwd: Some("/Users/me/github/ahma"),
            origin: Some("cursor"),
            outcome: OpOutcome::Running { elapsed_secs: 12 },
        };
        assert_eq!(id.render(false), "⚙ cargo nextest run · ahma · running 12s");
    }

    #[test]
    fn a_finished_operation_shows_its_exit_code_and_duration() {
        let id = OpIdentity {
            title: "cargo nextest run",
            cwd: Some("/Users/me/github/ahma"),
            origin: None,
            outcome: OpOutcome::Finished {
                exit_code: Some(0),
                status: "Completed".into(),
                duration_ms: 41_000,
            },
        };
        assert_eq!(
            id.render(false),
            "✓ cargo nextest run · ahma · exit 0 · 41s"
        );

        let failed = OpIdentity {
            outcome: OpOutcome::Finished {
                exit_code: Some(101),
                status: "Failed".into(),
                duration_ms: 3_400,
            },
            ..id
        };
        assert_eq!(
            failed.render(false),
            "✗ cargo nextest run · ahma · exit 101 · 3s"
        );
    }

    #[test]
    fn without_an_exit_code_we_say_the_status_rather_than_invent_one() {
        // A cancelled operation has no exit code. Rendering `exit 0` would be a
        // lie, and rendering nothing would leave the row unexplained.
        let id = OpIdentity {
            title: "cargo build",
            cwd: None,
            origin: None,
            outcome: OpOutcome::Finished {
                exit_code: None,
                status: "Cancelled".into(),
                duration_ms: 2_000,
            },
        };
        assert_eq!(id.render(false), "✗ cargo build · cancelled · 2s");
    }

    #[test]
    fn a_denied_operation_explains_itself_in_the_row() {
        // The row *is* the explanation — this is what makes a denial findable and
        // therefore answerable (R-PERM.7).
        let id = OpIdentity {
            title: "touch /etc/foo",
            cwd: Some("/Users/me/github/ahma"),
            origin: None,
            outcome: OpOutcome::Denied {
                reason: "outside sandbox scope".into(),
            },
        };
        assert_eq!(
            id.render(false),
            "✗ touch /etc/foo · ahma · denied: outside sandbox scope"
        );
    }

    #[test]
    fn the_origin_badge_appears_only_when_origins_are_mixed() {
        let id = OpIdentity {
            title: "cargo build",
            cwd: None,
            origin: Some("tui"),
            outcome: OpOutcome::Running { elapsed_secs: 3 },
        };
        // Everything came from one place: a badge on every row is pure noise.
        assert_eq!(id.render(false), "⚙ cargo build · running 3s");
        // Mixed: the badge is the whole reason the timeline is readable.
        assert_eq!(id.render(true), "⚙ cargo build [tui] · running 3s");
    }

    #[test]
    fn durations_stay_readable_at_every_scale() {
        assert_eq!(human_duration_ms(400), "0.4s");
        assert_eq!(human_duration_ms(41_000), "41s");
        assert_eq!(human_duration_ms(125_000), "2m 05s");
        assert_eq!(human_duration_ms(4_500_000), "1h 15m");
    }
}
