//! `/intro` — ahma in two levels (SPEC R24.12.6).
//!
//! Level one is a screen of one-line answers someone in a hurry can read in
//! half a minute; Enter on a line opens its second level, for the people who
//! want the detail. Shown once by itself on the first run, and on request
//! after that (`/intro`, `/getting-started`).
//!
//! Keep it short and true. Every command and key named here is checked by
//! `intro_names_only_real_commands` against the command list, so the tour
//! cannot drift into describing things that do not exist.

/// One line of the tour and what opens under it.
pub struct Topic {
    pub title: &'static str,
    pub summary: &'static str,
    pub detail: &'static [&'static str],
}

pub const TOPICS: &[Topic] = &[
    Topic {
        title: "What this is",
        summary: "Chat with a model you choose; its tools run in a sandbox locked to this folder.",
        detail: &[
            "The sandbox is enforced by the operating system (Seatbelt on macOS, Landlock on",
            "Linux), not by the model's good behaviour: a command simply cannot write outside",
            "the folder. Editors connected to ahma (Claude Code, Cursor, …) show their work",
            "here too, so this window is also where you watch and stop what they are doing.",
        ],
    },
    Topic {
        title: "Trust & safety",
        summary: "Trust a folder once. Anything outside it, network, or settings always asks.",
        detail: &[
            "Trusting a folder lets tools read, edit, build and test inside it without asking.",
            "Still asked every time: paths outside it, web access, tools on other MCP servers,",
            "and any change to ahma's own settings. `!cmd` runs unsandboxed — only when you",
            "type it. Every grant is logged to ~/.ahma/permissions-audit.jsonl; review or undo",
            "them in /settings trust.",
        ],
    },
    Topic {
        title: "Chat",
        summary: "Press i or /chat and type. The line under the chat says what the model is doing.",
        detail: &[
            "Reading N tokens, thinking, writing, running a tool, or waiting for you — always",
            "from real events. Esc stops a turn. A dropped connection is retried once, and",
            "says so. /compact shrinks a long conversation; /resume brings back the last one.",
        ],
    },
    Topic {
        title: "Models",
        summary: "/setup connects one, /model switches. Local models are private but slower.",
        detail: &[
            "Ollama, LM Studio and llama-server on this machine are found by themselves;",
            "cloud keys come from environment variables and are never stored. An editor's own",
            "model is offered only while it is connected — if it goes, chat moves to your last",
            "local model and tells you. A local model starts with the core tools and asks for",
            "more when it needs them. Slow? The status line shows why: loading, reading N",
            "tokens — try a smaller model, /compact, or a larger context in /provider numctx.",
        ],
    },
    Topic {
        title: "The work view",
        summary: "Everything ahma runs — for you or your editors — one section per client.",
        detail: &[
            "↑/↓ move, Enter opens a section or an operation, Space shows its live output,",
            "c cancels it, f switches between this project and all projects.",
        ],
    },
    Topic {
        title: "Commands & keys",
        summary: "/ opens every command, ? shows every key.",
        detail: &[
            "Type to filter the / menu. `!cmd` runs a shell command in this window; `#goal`",
            "breaks a goal into steps. Ctrl-C always cancels, a second one quits.",
        ],
    },
    Topic {
        title: "Settings",
        summary: "/settings shows them all; /settings <word> jumps straight to one.",
        detail: &[
            "Your settings live in ~/.ahma/settings.toml, a project's in its .ahma/ folder.",
            "Security switches are set with command-line flags only, so they are visible where",
            "ahma is started; trust and grants change only after a confirming second key.",
        ],
    },
    Topic {
        title: "When something is off",
        summary: "/doctor checks ahma and explains; it fixes things only with your OK.",
        detail: &[
            "It checks settings, granted folders, the daemon's build, trust and the logs. Ask",
            "it anything about ahma with /doctor <question>. /log shows ahma's own log live;",
            "/scope shows what the sandbox allows.",
        ],
    },
];

/// The command words the tour mentions, for the drift test.
#[cfg(test)]
fn named_commands() -> Vec<&'static str> {
    TOPICS
        .iter()
        .flat_map(|t| std::iter::once(t.summary).chain(t.detail.iter().copied()))
        .flat_map(|line| line.split(|c: char| c.is_whitespace() || c == ','))
        .filter(|w| w.starts_with('/') && w.len() > 1 && !w.contains('.'))
        .map(|w| w.trim_end_matches(['.', ';', ':', ')']))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tour may only name commands the TUI actually has.
    #[test]
    fn intro_names_only_real_commands() {
        let known: Vec<String> = crate::state::builtin_commands()
            .iter()
            .map(|c| {
                c.command
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        for cmd in named_commands() {
            assert!(
                known.iter().any(|k| k == cmd),
                "/intro mentions {cmd}, which is not a command"
            );
        }
    }

    /// Level one is meant to be read in one glance: one short line per topic.
    #[test]
    fn level_one_fits_a_glance() {
        assert!(TOPICS.len() <= 8);
        for t in TOPICS {
            assert!(t.summary.chars().count() <= 90, "{}: too long", t.title);
        }
    }
}
