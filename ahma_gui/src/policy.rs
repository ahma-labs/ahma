//! What a browser session may do (SPEC R-GUI.9).
//!
//! The terminal runs as the human, outside every sandbox, so it may do anything.
//! A browser tab is reached by a socket on this machine, and what it is trusted
//! with is less: it watches, chats, and answers questions for the length of the
//! session. Anything that writes a setting, stores a command that ahma will run
//! later, or runs one outside the sandbox stays in the terminal.
//!
//! This is enforced where text is parsed (`ahma_tui::controller`), not by what
//! the page shows: a `!` or a `/setup` typed into chat reaches the same parser.
//! And it is an allow-list, so a command the TUI gains later is closed to the
//! browser until someone decides it is not dangerous here.

use ahma_tui::state::{Capabilities, CommandPolicy, CommandRule};

const fn rule(head: &'static str) -> CommandRule {
    CommandRule {
        head,
        refuse_first_arg: &[],
    }
}

/// The slash commands a browser session may run.
pub const COMMANDS: &[CommandRule] = &[
    rule("/help"),
    rule("/?"),
    rule("/intro"),
    rule("/getting-started"),
    rule("/tasks"),
    rule("/chat"),
    // `/log file <path>` starts a log-monitoring tool call on a path.
    CommandRule {
        head: "/log",
        refuse_first_arg: &["file"],
    },
    rule("/scope"),
    rule("/clear"),
    rule("/compact"),
    rule("/resume"),
    rule("/export"),
    rule("/model"),
    // `/provider add` and `/provider numctx` write the user's configuration.
    CommandRule {
        head: "/provider",
        refuse_first_arg: &["add", "numctx"],
    },
    rule("/tools"),
    rule("/skills"),
    rule("/skill"),
    rule("/analyze"),
    // `/doctor fix` applies a change to the user's files.
    CommandRule {
        head: "/doctor",
        refuse_first_arg: &["fix"],
    },
    // A tool call in the session's own sandboxed MCP session, like chat.
    rule("/run"),
];

/// What a browser session may do.
pub const fn capabilities() -> Capabilities {
    Capabilities {
        host_commands: false,
        persistent_answers: false,
        wait_for_presentation: true,
        commands: CommandPolicy::Only(COMMANDS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every command the TUI advertises is decided: either the browser may run
    /// it, or it is refused for a reason. A command added to the TUI fails this
    /// until someone puts it in one list or the other, which is the point.
    #[test]
    fn every_advertised_command_is_decided_for_the_browser() {
        const REFUSED: &[(&str, &str)] = &[
            ("/setup", "writes settings and applies proposals"),
            ("/connect", "stores a provider and its key"),
            ("/sync", "persists the execution mode"),
            ("/async", "persists the execution mode"),
            ("/minimize", "persists a token setting"),
            ("/mcp", "stores servers, including commands ahma will spawn"),
            ("/agent", "loads and saves profiles"),
            ("/approve", "answered by intent, so there is one path"),
            ("/reject", "answered by intent, so there is one path"),
            ("/quit", "ends the session; a tab closing does that"),
            ("/exit", "ends the session"),
            ("/settings", "an editor the page does not have"),
        ];
        let caps = capabilities();
        let mut heads: Vec<&str> = ahma_tui::state::SLASH_COMMANDS
            .iter()
            .filter_map(|(cmd, _)| cmd.split_whitespace().next())
            .collect();
        heads.sort_unstable();
        heads.dedup();
        for head in heads {
            let allowed = COMMANDS.iter().any(|r| r.head == head);
            let refused = REFUSED.iter().any(|(h, _)| *h == head);
            assert!(
                allowed ^ refused,
                "{head} is advertised by the TUI but not decided for the browser (or decided \
                 twice): add it to COMMANDS or to REFUSED here, with why"
            );
            assert_eq!(caps.allows_command(head), allowed, "{head}");
        }
    }

    /// The commands that take a dangerous first argument are refused in that
    /// form and allowed in the others.
    #[test]
    fn a_safe_command_is_not_a_way_to_a_dangerous_one() {
        let caps = capabilities();
        for (cmd, ok) in [
            ("/log", true),
            ("/log file /var/log/system.log", false),
            ("/doctor", true),
            ("/doctor why is the sandbox off", true),
            ("/doctor fix 1", false),
            ("/provider", true),
            ("/provider add x http://y", false),
            ("/provider numctx 4096", false),
            ("/export markdown", true),
            // Things that are not commands at all.
            ("/", false),
            ("", false),
            ("/unknown-skill do something", false),
        ] {
            assert_eq!(caps.allows_command(cmd), ok, "{cmd:?}");
        }
        assert!(!caps.host_commands && !caps.persistent_answers);
        assert!(caps.wait_for_presentation);
    }
}
