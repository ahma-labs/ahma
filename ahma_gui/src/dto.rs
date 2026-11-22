//! What crosses between the server and the page (SPEC R-GUI.4, R-GUI.5).
//!
//! The server decides everything a human is shown and every choice they are
//! offered; the page lays it out. So the view is data — labels, ordering, what is
//! enabled, which answers a question offers — and the page sends back only
//! [`WireIntent`]s and [`WireReport`]s, an allow-list that is a strict subset of
//! what the session can do and rejects what it does not know.
//!
//! The bytes are pinned by the tests in this module and by `tests/golden`: the
//! page's hand-written types are checked against the same fixtures, so the two
//! cannot drift apart silently.

use serde::{Deserialize, Serialize};

/// Bumped when the shape changes incompatibly. The page refuses a server of a
/// protocol it does not speak, and says so.
pub const PROTOCOL: u32 = 1;

// ── Server → page ────────────────────────────────────────────────────────────

/// One update of the view: only the regions that changed since the last one the
/// page was sent, or all of them on connect.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct View {
    pub protocol: u32,
    pub rev: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<Header>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work: Option<Work>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat: Option<Chat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log: Option<Log>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gates: Option<Gates>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Header {
    pub workspace: String,
    /// The per-user hub answers.
    pub hub: bool,
    /// The MCP server answers.
    pub server: bool,
    pub transport: String,
    pub sandbox: Sandbox,
    pub model: Option<String>,
    pub counts: Counts,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Sandbox {
    /// What the sandbox is, in the words every surface uses.
    pub label: String,
    pub enforced: bool,
    /// ahma is the only thing confining commands.
    pub sole_authority: bool,
    /// The loud disclosure when it is not (SPEC R7).
    pub disclosure: Option<String>,
    pub note: Option<String>,
    pub write: Vec<String>,
    pub read: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Counts {
    pub running: usize,
    pub queued: usize,
    pub succeeded: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Work {
    /// Every project, not just the one this session was opened in.
    pub all_projects: bool,
    pub project: Option<String>,
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Section {
    /// Stable across re-registration: what an intent names.
    pub key: String,
    /// `client`, `hooks`, `ended` or `local`.
    pub kind: &'static str,
    pub title: String,
    pub scope: String,
    pub running: bool,
    pub counts: Counts,
    pub latest: Option<Latest>,
    pub open: bool,
    /// Only for the open section.
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Latest {
    pub title: String,
    pub status: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Row {
    /// The operation's id, and the instance that ran it: together what an
    /// intent about it names (SPEC R24.10.6).
    pub id: String,
    pub instance: Option<String>,
    pub title: String,
    pub status: &'static str,
    pub exit_code: Option<i64>,
    pub duration_ms: Option<u64>,
    /// Ran outside the sandbox (SPEC R-HUB.9); shown wherever the row is.
    pub unsandboxed: bool,
    pub denial: Option<Denial>,
    pub cancellable: bool,
    pub expanded: bool,
    /// The tail of its output, for the expanded row.
    pub output: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Denial {
    pub path: String,
    /// `read-only` or `read-write`.
    pub access: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Chat {
    pub entries: Vec<Entry>,
    pub turn: Option<Turn>,
    /// Which window the next turn goes to.
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Turn {
    /// `loading`, `reading`, `thinking`, `writing`, `tool` or `awaiting-you`.
    pub phase: &'static str,
    pub tool: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    User {
        id: u64,
        text: String,
    },
    Assistant {
        id: u64,
        blocks: Vec<Block>,
        streaming: bool,
    },
    Thinking {
        id: u64,
        text: String,
        streaming: bool,
    },
    Tool {
        id: u64,
        name: String,
        title: String,
        args: String,
        result: Option<String>,
        failed: bool,
    },
    Notice {
        id: u64,
        text: String,
    },
}

/// A reply, parsed in Rust into blocks and spans, so the page never sees markup
/// it could be made to interpret (SPEC R-GUI.5).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Block {
    Para {
        spans: Vec<Span>,
    },
    Heading {
        level: u8,
        spans: Vec<Span>,
    },
    Code {
        lang: Option<String>,
        text: String,
    },
    List {
        ordered: bool,
        start: Option<u64>,
        items: Vec<Vec<Block>>,
    },
    Quote {
        blocks: Vec<Block>,
    },
    Rule,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Span {
    Text {
        s: String,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        em: bool,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        strong: bool,
    },
    Code {
        s: String,
    },
    /// Shown with its destination; never followed by the page.
    Link {
        s: String,
        href: String,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Log {
    pub lines: Vec<LogLine>,
    pub files: Vec<String>,
    pub active: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LogLine {
    pub ts: String,
    /// `info`, `warn` or `error`.
    pub level: &'static str,
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Gates {
    /// Questions waiting, the one shown included.
    pub waiting: usize,
    pub top: Option<Gate>,
}

/// The question a human is being asked, with every answer it offers.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Gate {
    /// A sandbox-scope question (SPEC R-PERM.3.4): the body is the one every
    /// surface shows.
    Scope {
        decision_id: String,
        title: String,
        sections: Vec<GateSection>,
        options: Vec<GateOption>,
        advice: Option<String>,
    },
    /// A web-egress question.
    Web {
        decision_id: String,
        domain: String,
        url: String,
        tool: Option<String>,
        options: Vec<GateOption>,
    },
    /// A link a hooked command was refused (SPEC R-LINK.6), asked here before
    /// the harness dialog would: the question every surface shows.
    Link {
        decision_id: String,
        /// The site, on a line of its own (ASCII; R-LINK.7).
        site: String,
        /// The repo the refused command ran in: what a session answer is bound to.
        workspace: String,
        /// `links::policy::LinkQuestion::text`: the site, the address, its notes
        /// and the guidance (R-LINK.7).
        text: String,
        /// That nothing waits on the command, so an answer applies when it runs
        /// again.
        note: String,
        options: Vec<GateOption>,
    },
    /// The chat's tool-approval gate.
    Tool {
        op_id: String,
        tool: String,
        description: String,
        note: Option<String>,
        options: Vec<GateOption>,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GateSection {
    pub heading: String,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GateOption {
    /// What an intent names (`read-write-session`, `once`, `reject`).
    pub value: String,
    pub label: String,
    /// Takes access rather than refusing it.
    pub granting: bool,
    /// This session may give it. A saved answer is not offered here, and says
    /// where it is (SPEC R-GUI.9).
    pub allowed: bool,
    pub hint: Option<String>,
}

// ── Page → server ────────────────────────────────────────────────────────────

/// What the page may ask. A strict subset of [`ahma_tui::controller::Intent`]:
/// no persistent answer, no host command and no setting is expressible, and a
/// field the server does not know is refused rather than ignored.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WireIntent {
    Submit {
        text: String,
    },
    // Struct variants, even with no fields: serde does not deny unknown fields on
    // a unit variant of an internally tagged enum, and an intent that carries
    // something this server does not know is refused, not ignored.
    CancelTurn {},
    AnswerScopeGrant {
        decision_id: String,
        option: String,
    },
    ToggleScopeGrantDetail {
        decision_id: String,
    },
    AnswerWebApproval {
        decision_id: String,
        answer: WireWebAnswer,
    },
    AnswerLinkAsk {
        decision_id: String,
        answer: WireLinkAnswer,
    },
    AnswerToolApproval {
        op_id: String,
        answer: WireToolAnswer,
    },
    CancelOp {
        id: String,
        instance: Option<String>,
    },
    ReRaiseGrant {
        id: String,
        instance: Option<String>,
    },
    OpenSection {
        key: Option<String>,
    },
    ChatWith {
        key: String,
    },
    ToggleOpOutput {
        id: String,
        instance: Option<String>,
    },
    ToggleProjectFilter {},
}

/// `always` is not here: a saved answer is not this surface's to give.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WireWebAnswer {
    Deny,
    Once,
    Session,
}

/// The answers a hooked command's refused link takes here (SPEC R-LINK.6,
/// R-GUI.9): no saving answer, and no "this time", since nothing waits on the
/// command.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WireLinkAnswer {
    Deny,
    Session,
}

/// `always` is not here either, as for [`WireWebAnswer`].
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WireToolAnswer {
    Reject,
    Once,
}

/// What a tab tells the server about what it is showing.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WireReport {
    /// The question is on screen, in a visible, focused document.
    GateVisible { decision_id: String },
    /// The tab can no longer be read: hidden, blurred or the question went.
    GateHidden { decision_id: String },
    /// Whether the whole "if you choose always" line is on screen.
    GateSaveLine { decision_id: String, visible: bool },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The bytes of what the page sends are fixed, and anything else is
    /// refused: an unknown intent, an extra field, and above all a persistent
    /// answer, which the page has no way to say.
    #[test]
    fn the_page_can_say_only_what_the_allow_list_names() {
        let ok = |v: serde_json::Value| serde_json::from_value::<WireIntent>(v);
        assert_eq!(
            ok(json!({"type": "submit", "text": "hello"})).unwrap(),
            WireIntent::Submit {
                text: "hello".into()
            }
        );
        assert_eq!(
            ok(json!({"type": "answer_scope_grant", "decision_id": "d", "option": "read-write-session"}))
                .unwrap(),
            WireIntent::AnswerScopeGrant {
                decision_id: "d".into(),
                option: "read-write-session".into()
            }
        );
        assert_eq!(
            ok(json!({"type": "answer_web_approval", "decision_id": "d", "answer": "session"}))
                .unwrap(),
            WireIntent::AnswerWebApproval {
                decision_id: "d".into(),
                answer: WireWebAnswer::Session
            }
        );
        for refused in [
            json!({"type": "answer_web_approval", "decision_id": "d", "answer": "always"}),
            json!({"type": "answer_tool_approval", "op_id": "o", "answer": "always"}),
            json!({"type": "run_host_command", "command": "id"}),
            json!({"type": "submit", "text": "x", "persist": true}),
            json!({"type": "cancel_turn", "extra": 1}),
            json!({"text": "no type"}),
            json!("a string"),
        ] {
            assert!(ok(refused.clone()).is_err(), "{refused} must be refused");
        }
    }

    /// SPEC R-LINK.6, R-GUI.9: the answer to a hooked command's refused link
    /// is deny or this site for this session. "This time" is not one — nothing
    /// waits on the command — and no saving answer can be said at all.
    #[test]
    fn a_link_answer_is_deny_or_this_site_for_this_session() {
        let ok = |v: serde_json::Value| serde_json::from_value::<WireIntent>(v);
        for (raw, answer) in [
            ("deny", WireLinkAnswer::Deny),
            ("session", WireLinkAnswer::Session),
        ] {
            assert_eq!(
                ok(json!({"type": "answer_link_ask", "decision_id": "l", "answer": raw})).unwrap(),
                WireIntent::AnswerLinkAsk {
                    decision_id: "l".into(),
                    answer
                }
            );
        }
        for refused in [
            "once",
            "always-site",
            "always-site-this-repo",
            "always-site-all-repos",
            "always-any-site",
            "always-domain-this-repo",
            "always-any-site-this-repo",
        ] {
            assert!(
                ok(json!({"type": "answer_link_ask", "decision_id": "l", "answer": refused}))
                    .is_err(),
                "{refused} must be refused"
            );
        }
        assert!(
            ok(json!({"type": "answer_link_ask", "decision_id": "l", "answer": "deny", "x": 1}))
                .is_err()
        );
    }

    #[test]
    fn a_report_names_what_a_tab_shows_and_nothing_else() {
        let ok = |v: serde_json::Value| serde_json::from_value::<WireReport>(v);
        assert_eq!(
            ok(json!({"type": "gate_visible", "decision_id": "d"})).unwrap(),
            WireReport::GateVisible {
                decision_id: "d".into()
            }
        );
        assert!(ok(json!({"type": "gate_visible"})).is_err());
        assert!(
            ok(json!({"type": "gate_save_line", "decision_id": "d", "visible": true, "x": 1}))
                .is_err()
        );
    }

    /// Only the regions that changed are sent, and the protocol is always there.
    #[test]
    fn a_view_carries_only_the_regions_it_updates() {
        let view = View {
            protocol: PROTOCOL,
            rev: 7,
            gates: Some(Gates {
                waiting: 0,
                top: None,
            }),
            ..View::default()
        };
        assert_eq!(
            serde_json::to_value(&view).unwrap(),
            json!({"protocol": 1, "rev": 7, "gates": {"waiting": 0, "top": null}})
        );
    }

    /// The text of a reply is data in a tree, tagged so the page can build
    /// nodes from it and never has to read markup.
    #[test]
    fn a_reply_is_a_tree_of_blocks_and_spans() {
        let block = Block::List {
            ordered: false,
            start: None,
            items: vec![vec![Block::Para {
                spans: vec![
                    Span::Text {
                        s: "a ".into(),
                        em: false,
                        strong: true,
                    },
                    Span::Code { s: "b".into() },
                    Span::Link {
                        s: "c".into(),
                        href: "https://example.com".into(),
                    },
                ],
            }]],
        };
        assert_eq!(
            serde_json::to_value(&block).unwrap(),
            json!({"t": "list", "ordered": false, "start": null, "items": [[
                {"t": "para", "spans": [
                    {"t": "text", "s": "a ", "strong": true},
                    {"t": "code", "s": "b"},
                    {"t": "link", "s": "c", "href": "https://example.com"}
                ]}
            ]]})
        );
    }
}
