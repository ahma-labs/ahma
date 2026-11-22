//! From the session's state to what the page is sent (SPEC R-GUI.4, R-GUI.5).
//!
//! Every decision about what a human sees is made here, in Rust: which answers a
//! question offers and which of them this session may give, how a reply is
//! parsed, what a path looks like. The page receives data and lays it out.
//!
//! Every string that reaches the page passes through [`clean`], which makes the
//! characters that could spoof one thing as another visible: a right-to-left
//! override in a file name reads as a different path, and a control character
//! can rewrite a line.

use crate::dto::*;
use ahma_tui::state::{AppState, ChatEntry, LogLevel, OpStatus, Operation, TurnPhase};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// How many chat entries, log lines and output lines the page is sent: what it
/// can use, bounded so a long session does not make every update large.
const LOG_LINES: usize = 300;
const OUTPUT_LINES: usize = 60;

/// Make a string safe to put in front of a human: control characters (other
/// than newline and tab) and Unicode's bidirectional and invisible formatting
/// characters are shown as `\u{…}` escapes instead of acting.
pub fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if is_spoofing(c) {
            out.push_str(&format!("\\u{{{:04X}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

fn is_spoofing(c: char) -> bool {
    match c {
        '\n' | '\t' => false,
        // C0 and C1 controls, DEL.
        c if c.is_control() => true,
        // Bidirectional marks, embeddings, overrides and isolates.
        '\u{061C}'
        | '\u{200E}'
        | '\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2066}'..='\u{2069}' => true,
        // Zero-width and word-joining characters, the line and paragraph
        // separators, and the BOM.
        '\u{200B}'..='\u{200D}' | '\u{2060}' | '\u{2028}' | '\u{2029}' | '\u{FEFF}' => true,
        _ => false,
    }
}

// ── Markdown ─────────────────────────────────────────────────────────────────

/// A reply as blocks and spans. Raw HTML is text, an image is its alt text and
/// address, and a link is its text and address, never something to follow.
pub fn markdown(text: &str) -> Vec<Block> {
    let mut parser = Builder::default();
    for event in Parser::new_ext(text, Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES) {
        parser.event(event);
    }
    parser.finish()
}

#[derive(Default)]
struct Builder {
    /// Blocks being built, innermost last: the document, a list item, a quote.
    stack: Vec<Vec<Block>>,
    spans: Vec<Span>,
    em: bool,
    strong: bool,
    link: Option<String>,
    link_text: String,
    code: Option<(Option<String>, String)>,
    heading: Option<u8>,
    lists: Vec<(bool, Option<u64>, Vec<Vec<Block>>)>,
}

impl Builder {
    fn blocks(&mut self) -> &mut Vec<Block> {
        if self.stack.is_empty() {
            self.stack.push(Vec::new());
        }
        self.stack.last_mut().expect("just ensured")
    }

    fn flush_spans(&mut self) -> Vec<Span> {
        std::mem::take(&mut self.spans)
    }

    fn text(&mut self, s: &str) {
        if self.link.is_some() {
            self.link_text.push_str(s);
            return;
        }
        let s = clean(s);
        if s.is_empty() {
            return;
        }
        self.spans.push(Span::Text {
            s,
            em: self.em,
            strong: self.strong,
        });
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => match &mut self.code {
                Some((_, buf)) => buf.push_str(&t),
                None => self.text(&t),
            },
            Event::Code(t) => {
                if self.link.is_some() {
                    self.link_text.push_str(&t);
                } else {
                    self.spans.push(Span::Code { s: clean(&t) });
                }
            }
            // Markup the page must never interpret is text.
            Event::Html(t) | Event::InlineHtml(t) => self.text(&t),
            Event::SoftBreak | Event::HardBreak => self.text("\n"),
            Event::Rule => self.blocks().push(Block::Rule),
            Event::TaskListMarker(done) => self.text(if done { "[x] " } else { "[ ] " }),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Emphasis => self.em = true,
            Tag::Strong => self.strong = true,
            Tag::Heading { level, .. } => {
                self.heading = Some(match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                })
            }
            Tag::CodeBlock(kind) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(l) if !l.is_empty() => Some(clean(&l)),
                    _ => None,
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(start) => self.lists.push((start.is_some(), start, Vec::new())),
            Tag::Item => self.stack.push(Vec::new()),
            Tag::BlockQuote(_) => self.stack.push(Vec::new()),
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.link = Some(clean(&dest_url));
                self.link_text.clear();
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Emphasis => self.em = false,
            TagEnd::Strong => self.strong = false,
            TagEnd::Paragraph | TagEnd::TableRow | TagEnd::TableHead => {
                let spans = self.flush_spans();
                if !spans.is_empty() {
                    self.blocks().push(Block::Para { spans });
                }
            }
            TagEnd::TableCell => self.text("  "),
            TagEnd::Heading(_) => {
                let spans = self.flush_spans();
                let level = self.heading.take().unwrap_or(1);
                self.blocks().push(Block::Heading { level, spans });
            }
            TagEnd::CodeBlock => {
                if let Some((lang, text)) = self.code.take() {
                    self.blocks().push(Block::Code {
                        lang,
                        text: clean(text.trim_end_matches('\n')),
                    });
                }
            }
            TagEnd::Link | TagEnd::Image => {
                let href = self.link.take().unwrap_or_default();
                let s = clean(&std::mem::take(&mut self.link_text));
                self.spans.push(Span::Link { s, href });
            }
            TagEnd::Item => {
                // Text directly in an item (a tight list) has no paragraph.
                let spans = self.flush_spans();
                let mut item = self.stack.pop().unwrap_or_default();
                if !spans.is_empty() {
                    item.insert(0, Block::Para { spans });
                }
                if let Some(list) = self.lists.last_mut() {
                    list.2.push(item);
                }
            }
            TagEnd::List(_) => {
                if let Some((ordered, start, items)) = self.lists.pop() {
                    self.blocks().push(Block::List {
                        ordered,
                        start,
                        items,
                    });
                }
            }
            TagEnd::BlockQuote(_) => {
                let blocks = self.stack.pop().unwrap_or_default();
                self.blocks().push(Block::Quote { blocks });
            }
            _ => {}
        }
    }

    fn finish(mut self) -> Vec<Block> {
        let spans = self.flush_spans();
        if !spans.is_empty() {
            self.blocks().push(Block::Para { spans });
        }
        self.stack.into_iter().next().unwrap_or_default()
    }
}

// ── Regions ──────────────────────────────────────────────────────────────────

fn status_word(status: &OpStatus) -> &'static str {
    match status {
        OpStatus::Running => "running",
        OpStatus::Pending => "queued",
        OpStatus::Waiting => "waiting",
        OpStatus::Succeeded => "succeeded",
        OpStatus::Failed => "failed",
        OpStatus::Cancelled => "cancelled",
        OpStatus::Denied => "denied",
        OpStatus::Interrupted => "interrupted",
    }
}

fn access_word(access: ahma_common::config::ScopeAccess) -> &'static str {
    if access.is_write() {
        "read-write"
    } else {
        "read-only"
    }
}

pub fn header(state: &AppState) -> Header {
    let (running, queued, succeeded, failed) =
        state
            .operations
            .iter()
            .fold((0, 0, 0, 0), |(r, q, s, f), op| match op.status {
                OpStatus::Running => (r + 1, q, s, f),
                OpStatus::Pending | OpStatus::Waiting => (r, q + 1, s, f),
                OpStatus::Succeeded => (r, q, s + 1, f),
                _ => (r, q, s, f + 1),
            });
    let scope = state.sandbox_scope.as_ref();
    Header {
        workspace: clean(&state.workspace),
        hub: state.hub_healthy,
        server: state.server_healthy,
        transport: clean(&state.transport_label),
        sandbox: Sandbox {
            label: clean(&state.sandbox_status.label()),
            enforced: scope.is_some_and(|s| s.enforced),
            sole_authority: state.sandbox_status.is_sole_authority(),
            disclosure: scope.and_then(|s| s.disclosure.as_deref()).map(clean),
            note: scope.and_then(|s| s.platform_note.as_deref()).map(clean),
            write: scope
                .map(|s| s.write.iter().map(|p| clean(p)).collect())
                .unwrap_or_default(),
            read: scope
                .map(|s| s.read.iter().map(|p| clean(p)).collect())
                .unwrap_or_default(),
        },
        model: Some(state.selected_model())
            .filter(|m| !m.is_empty())
            .map(|m| clean(&m)),
        counts: Counts {
            running,
            queued,
            succeeded,
            failed,
        },
    }
}

fn counts(c: &ahma_tui::task_tree::GroupCounts) -> Counts {
    Counts {
        running: c.running,
        queued: c.queued,
        succeeded: c.succeeded,
        failed: c.failed,
    }
}

fn row(op: &Operation, expanded: bool, output: Vec<String>) -> Row {
    Row {
        id: clean(&op.id),
        instance: op.instance_id.as_deref().map(clean),
        title: clean(&op.display_name()),
        status: status_word(&op.status),
        exit_code: op.exit_code,
        duration_ms: op.duration_ms,
        unsandboxed: op.unsandboxed,
        denial: op.denial.as_ref().map(|(path, access)| Denial {
            path: clean(path),
            access: access_word(*access),
        }),
        cancellable: matches!(
            op.status,
            OpStatus::Running | OpStatus::Pending | OpStatus::Waiting
        ),
        expanded,
        output,
    }
}

/// The work view. The controller's rebuild comes first: the sections are a
/// function of the operations and the open section, so what is sent is never
/// older than the state it describes.
pub fn work(state: &AppState) -> Work {
    use ahma_tui::task_tree::RowKind;
    use ahma_tui::work_view::SectionKind;

    state.rebuild_work_view(ahma_tui::ui::wall_ms());
    let sections = state.work_sections.borrow();
    Work {
        all_projects: state.show_all_projects,
        project: state.project_root.as_deref().map(clean),
        sections: sections
            .iter()
            .map(|section| {
                let h = &section.header;
                let mut rows: Vec<Row> = Vec::new();
                for tree_row in &section.rows {
                    match &tree_row.kind {
                        RowKind::Op { op_index, expanded } => {
                            if let Some(op) = state.operations.get(*op_index) {
                                rows.push(row(op, *expanded, Vec::new()));
                            }
                        }
                        RowKind::Output { text, .. } => {
                            if let Some(last) = rows.last_mut()
                                && last.output.len() < OUTPUT_LINES
                            {
                                last.output.push(clean(text));
                            }
                        }
                        RowKind::Instance { .. } | RowKind::Group { .. } => {}
                    }
                }
                Section {
                    key: clean(&section.key),
                    kind: match h.kind {
                        SectionKind::Client => "client",
                        SectionKind::Hooks => "hooks",
                        SectionKind::Ended => "ended",
                        SectionKind::Local => "local",
                    },
                    title: clean(&match h.ordinal {
                        Some(n) => format!("{} #{n}", h.client),
                        None => h.client.clone(),
                    }),
                    scope: clean(&h.scope_short),
                    running: h.running,
                    counts: counts(&h.counts),
                    latest: h.latest_title.as_ref().map(|title| Latest {
                        title: clean(title),
                        status: h
                            .latest_status
                            .as_ref()
                            .map(status_word)
                            .unwrap_or("running"),
                    }),
                    open: section.open,
                    rows,
                }
            })
            .collect(),
    }
}

pub fn chat(state: &AppState) -> Chat {
    let entries = state
        .chat
        .entries()
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let id = state.chat.id_at(i);
            match entry {
                ChatEntry::User { text, .. } => Entry::User {
                    id,
                    text: clean(text),
                },
                ChatEntry::Assistant { content, streaming } => Entry::Assistant {
                    id,
                    blocks: markdown(content),
                    streaming: *streaming,
                },
                ChatEntry::Thinking { content, streaming } => Entry::Thinking {
                    id,
                    text: clean(content),
                    streaming: *streaming,
                },
                ChatEntry::ToolCall {
                    name,
                    args,
                    result,
                    failed,
                    ..
                } => {
                    let parsed = serde_json::from_str::<serde_json::Value>(args).ok();
                    Entry::Tool {
                        id,
                        name: clean(name),
                        title: clean(&ahma_common::op_identity::title_for_value(
                            name,
                            parsed.as_ref(),
                        )),
                        args: clean(args),
                        result: result.as_deref().map(clean),
                        failed: *failed,
                    }
                }
                ChatEntry::Notice { text } => Entry::Notice {
                    id,
                    text: clean(text),
                },
            }
        })
        .collect();
    Chat {
        entries,
        turn: state.turn.as_ref().map(|t| {
            let (phase, tool) = match &t.phase {
                TurnPhase::Loading => ("loading", None),
                TurnPhase::Reading => ("reading", None),
                TurnPhase::Thinking => ("thinking", None),
                TurnPhase::Writing => ("writing", None),
                TurnPhase::Tool { name } => ("tool", Some(clean(name))),
                TurnPhase::AwaitingYou => ("awaiting-you", None),
            };
            Turn { phase, tool }
        }),
        target: state.active_target_instance.as_deref().map(clean),
    }
}

pub fn log(state: &AppState) -> Log {
    let skip = state.log.len().saturating_sub(LOG_LINES);
    Log {
        lines: state
            .log
            .iter()
            .skip(skip)
            .map(|line| LogLine {
                ts: line.timestamp.strftime("%H:%M:%S").to_string(),
                level: match line.level {
                    LogLevel::Error => "error",
                    LogLevel::Warn => "warn",
                    LogLevel::Info | LogLevel::Debug => "info",
                },
                text: clean(&line.message),
            })
            .collect(),
        files: state.log_files.iter().map(|f| clean(&f.name)).collect(),
        active: state.active_log_file.as_deref().map(clean),
    }
}

fn option(
    value: &str,
    label: &str,
    granting: bool,
    allowed: bool,
    hint: Option<&str>,
) -> GateOption {
    GateOption {
        value: value.to_string(),
        label: clean(label),
        granting,
        allowed,
        hint: hint.map(str::to_string),
    }
}

const SAVED_HERE: &str =
    "A saved answer is made in `ahma tui`; this one lasts as long as the session.";

/// The question a human is being asked, with every answer it offers and whether
/// this session may give it. What is not offered here says where it is.
pub fn gates(state: &AppState) -> Gates {
    use ahma_common::scope_grant::GrantDecision;

    let persistent = state.caps.persistent_answers;
    let waiting = state.scope_grant.iter().count()
        + state.scope_grant_queue.len()
        + state.web_approval.iter().count()
        + state.web_approval_queue.len()
        + state.link_ask.iter().count()
        + state.link_ask_queue.len()
        + state.approval.iter().count();

    let top = if let Some(gate) = &state.scope_grant {
        let body = ahma_common::grant_prompt::render(&gate.request);
        Some(Gate::Scope {
            decision_id: clean(&gate.decision_id),
            title: clean(&body.title),
            sections: body
                .sections
                .iter()
                .map(|s| GateSection {
                    heading: clean(&s.heading),
                    body: clean(&s.body),
                })
                .collect(),
            options: body
                .options
                .iter()
                .map(|o| {
                    let granting = o.decision != GrantDecision::Deny;
                    let saving = granting && o.decision.tier().is_persistent();
                    option(
                        o.value,
                        &o.label,
                        granting,
                        !saving || persistent,
                        (saving && !persistent).then_some(SAVED_HERE),
                    )
                })
                .collect(),
            advice: gate.advice.as_ref().map(|a| clean(&a.line())),
        })
    } else if let Some(gate) = &state.web_approval {
        Some(Gate::Web {
            decision_id: clean(&gate.decision_id),
            domain: clean(&gate.domain),
            url: clean(&truncate(&gate.url, 256)),
            tool: gate.tool.as_deref().map(clean),
            options: vec![
                option("deny", "Deny (default)", false, true, None),
                option("once", "Allow this request only", true, true, None),
                option(
                    "session",
                    "Allow this host for the session",
                    true,
                    true,
                    None,
                ),
                option(
                    "always",
                    "Always allow this host",
                    true,
                    persistent,
                    (!persistent).then_some(SAVED_HERE),
                ),
            ],
        })
    } else if let Some(gate) = &state.link_ask {
        Some(link(gate, persistent))
    } else {
        state.approval.as_ref().map(|gate| Gate::Tool {
            op_id: clean(&gate.op_id),
            tool: clean(&gate.tool),
            description: clean(&gate.description),
            note: gate.note.as_deref().map(clean),
            options: vec![
                option("reject", "Reject (default)", false, true, None),
                option("once", "Approve this call", true, true, None),
                option(
                    "always",
                    "Always allow this tool",
                    true,
                    persistent,
                    (!persistent).then_some(SAVED_HERE),
                ),
            ],
        })
    };
    Gates { waiting, top }
}

/// A link a hooked command was refused (SPEC R-LINK.6): the question every
/// surface shows (R-LINK.7) and the answers it offers in the TUI, in that
/// order. A surface that may not save (R-GUI.9) gives only the ones
/// `HarnessLinkAsk::unsaved_choices` names — deny and this site for this
/// session — and each other one says where it is given: the TUI, or the grant
/// line a human runs.
fn link(gate: &ahma_tui::state::LinkAskGate, persistent: bool) -> Gate {
    use ahma_tui::controller::LinkChoice;
    let ask = &gate.ask;
    let question = ask.question();
    let unsaved = ask.unsaved_choices();
    let options = gate
        .choices
        .iter()
        .map(|&choice| {
            let allowed = persistent || unsaved.contains(&choice);
            let hint = match choice {
                LinkChoice::Session => Some(
                    "Commands in this repo may open its links until the agent's harness exits \
                     (at most 12 hours). Not saved."
                        .to_string(),
                ),
                _ if allowed => None,
                _ => ask.grant_command(choice).map(|grant| {
                    clean(&format!(
                        "Saved answers are given in `ahma tui`, or by running `{grant}`."
                    ))
                }),
            };
            let label = match choice {
                LinkChoice::Deny => "Deny (default)".to_string(),
                other => question.label(other),
            };
            GateOption {
                value: choice.value().to_string(),
                label: clean(&label),
                granting: choice != LinkChoice::Deny,
                allowed,
                hint,
            }
        })
        .collect();
    Gate::Link {
        decision_id: clean(&gate.decision_id),
        site: clean(&ask.refusal.host.to_ascii_lowercase()),
        workspace: clean(&ask.workspace.display().to_string()),
        text: clean(&question.text()),
        note: ahma_tui::harness_asks::LINK_NOT_PAUSED.to_string(),
        options,
    }
}

/// At most `max` characters, with an ellipsis when it was cut: a long address is
/// never the whole of what is being approved (SPEC R-WEB.6.2).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

/// Every region.
pub fn view(state: &AppState, rev: u64) -> View {
    View {
        protocol: PROTOCOL,
        rev,
        header: Some(header(state)),
        work: Some(work(state)),
        chat: Some(chat(state)),
        log: Some(log(state)),
        gates: Some(gates(state)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ahma_common::scope_grant::{GrantReason, ScopeGrantRequest};
    use ahma_tui::state::{Capabilities, ScopeGrantGate};

    fn state() -> AppState {
        AppState::new("http://127.0.0.1:9", "HTTP", true)
    }

    // ── clean ────────────────────────────────────────────────────────────────

    /// A right-to-left override makes `/tmp/evil\u{202E}txt.sh` read as another
    /// name. Nothing that reorders, hides or rewrites reaches the page as itself.
    #[test]
    fn characters_that_spoof_are_shown_as_escapes() {
        assert_eq!(
            clean("/tmp/a\u{202E}txt.sh"),
            "/tmp/a\\u{202E}txt.sh",
            "RLO"
        );
        assert_eq!(clean("a\u{200B}b"), "a\\u{200B}b", "zero width");
        assert_eq!(
            clean("x\u{2066}y\u{2069}"),
            "x\\u{2066}y\\u{2069}",
            "isolates"
        );
        assert_eq!(clean("a\u{1b}[2Jb"), "a\\u{001B}[2Jb", "escape");
        assert_eq!(
            clean("a\r\nb\tc"),
            "a\\u{000D}\nb\tc",
            "CR is not a newline"
        );
        assert_eq!(
            clean("plain / path — ünïcode ✓"),
            "plain / path — ünïcode ✓"
        );
    }

    // ── markdown ─────────────────────────────────────────────────────────────

    #[test]
    fn a_reply_becomes_blocks_and_spans() {
        let blocks = markdown(
            "# Title\n\nSome **bold** and `code` and *em*.\n\n- one\n- two\n\n```rust\nlet x = 1;\n```\n",
        );
        assert_eq!(blocks.len(), 4, "{blocks:#?}");
        assert!(matches!(&blocks[0], Block::Heading { level: 1, .. }));
        let Block::Para { spans } = &blocks[1] else {
            panic!("{blocks:#?}")
        };
        assert!(spans.contains(&Span::Text {
            s: "bold".into(),
            em: false,
            strong: true
        }));
        assert!(spans.contains(&Span::Code { s: "code".into() }));
        let Block::List { ordered, items, .. } = &blocks[2] else {
            panic!("{blocks:#?}")
        };
        assert!(!ordered && items.len() == 2);
        assert_eq!(
            blocks[3],
            Block::Code {
                lang: Some("rust".into()),
                text: "let x = 1;".into()
            }
        );
    }

    /// What a model writes is not trusted to be inert: HTML is text, a link is
    /// text with its address beside it, an image is its address and not a fetch.
    #[test]
    fn markup_in_a_reply_is_text_and_links_carry_their_address() {
        let blocks = markdown(
            "see <b onclick=alert(1)>this</b> [click](javascript:alert(1)) ![pic](https://evil/x.png)\n\n<script>alert(2)</script>\n\n<img src=x onerror=alert(3)>",
        );
        let json = serde_json::to_string(&blocks).unwrap();
        // Inline and block HTML alike is a text span: data the page puts in a
        // text node, never something it parses.
        assert!(json.contains("<b onclick=alert(1)>"), "{json}");
        assert!(json.contains("<script>alert(2)</script>"), "{json}");
        assert!(json.contains("<img src=x onerror=alert(3)>"), "{json}");
        assert!(!json.contains(r#""t":"html""#));
        // The link keeps its destination as data, never as markup.
        assert!(
            blocks.iter().any(
                |b| matches!(b, Block::Para { spans } if spans.iter().any(|s| matches!(s,
                Span::Link { s, href } if s == "click" && href == "javascript:alert(1)")))
            ),
            "{json}"
        );
        assert!(
            json.contains(r#""href":"https://evil/x.png""#),
            "an image is a link with its address: {json}"
        );
    }

    #[test]
    fn a_quote_and_an_ordered_list_nest() {
        let blocks = markdown("> quoted\n\n3. three\n4. four\n");
        assert!(matches!(&blocks[0], Block::Quote { blocks } if blocks.len() == 1));
        assert!(matches!(
            &blocks[1],
            Block::List { ordered: true, start: Some(3), items, .. } if items.len() == 2
        ));
    }

    #[test]
    fn an_empty_reply_is_no_blocks() {
        assert!(markdown("").is_empty());
    }

    // ── gates ────────────────────────────────────────────────────────────────

    fn scope_gate(state: &mut AppState) {
        let mut gate = ScopeGrantGate::from_request(ScopeGrantRequest {
            decision_id: "q".into(),
            path: "/opt/cache".into(),
            access: ahma_common::config::ScopeAccess::Rw,
            reason: GrantReason::StderrHeuristic,
            tool: Some("cargo".into()),
            context: Default::default(),
        });
        gate.restart_arming(state.caps.wait_for_presentation);
        state.scope_grant = Some(gate);
    }

    /// The same question the terminal shows, from the same renderer, with the
    /// answers this session may not give marked — and saying where they are.
    #[test]
    fn a_question_offers_the_terminals_answers_and_marks_the_ones_this_session_may_not_give() {
        let mut terminal = state();
        scope_gate(&mut terminal);
        let g = gates(&terminal);
        let Some(Gate::Scope {
            decision_id,
            title,
            sections,
            options,
            ..
        }) = g.top
        else {
            panic!("a scope question")
        };
        assert_eq!(decision_id, "q");
        let body =
            ahma_common::grant_prompt::render(&terminal.scope_grant.as_ref().unwrap().request);
        assert_eq!(title, body.title, "the one prompt body, R-PERM.3.4");
        assert_eq!(sections.len(), body.sections.len());
        assert_eq!(
            options.iter().map(|o| o.value.as_str()).collect::<Vec<_>>(),
            body.options.iter().map(|o| o.value).collect::<Vec<_>>(),
            "the same choices in the same order"
        );
        assert_eq!(options[0].value, "deny");
        assert!(options.iter().all(|o| o.allowed), "the terminal may save");

        let mut gui = state();
        gui.caps = Capabilities {
            host_commands: false,
            persistent_answers: false,
            wait_for_presentation: true,
            commands: ahma_tui::state::CommandPolicy::Only(&[]),
        };
        scope_gate(&mut gui);
        let Some(Gate::Scope { options, .. }) = gates(&gui).top else {
            unreachable!()
        };
        let denied: Vec<_> = options.iter().filter(|o| !o.allowed).collect();
        assert!(!denied.is_empty(), "a saving answer exists and is marked");
        assert!(denied.iter().all(|o| o.granting && o.hint.is_some()));
        assert!(
            options[0].allowed && !options[0].granting,
            "deny is always there"
        );
    }

    #[test]
    fn a_web_question_offers_deny_first_and_always_only_where_it_may() {
        let mut gui = state();
        gui.caps.persistent_answers = false;
        gui.web_approval = Some(ahma_tui::state::WebApprovalGate::from_request(
            ahma_common::web_approval::WebApprovalRequest {
                decision_id: "w".into(),
                domain: "example.com".into(),
                url: format!("https://example.com/{}", "a".repeat(400)),
                tool: Some("fetch_webpage".into()),
            },
        ));
        let Some(Gate::Web { url, options, .. }) = gates(&gui).top else {
            panic!()
        };
        assert_eq!(
            options.iter().map(|o| o.value.as_str()).collect::<Vec<_>>(),
            ["deny", "once", "session", "always"]
        );
        assert!(options.last().is_some_and(|o| !o.allowed));
        assert!(
            url.chars().count() <= 257,
            "a long address is cut: {}",
            url.len()
        );
        assert!(url.ends_with('…'));
    }

    fn link_gate(host: &str, local: bool) -> ahma_tui::state::LinkAskGate {
        let mut gate =
            ahma_tui::state::LinkAskGate::from_ask(ahma_tui::harness_asks::HarnessLinkAsk {
                workspace: std::env::temp_dir().join("ws"),
                refusal: ahma_common::harness_asks::LinkRefusal {
                    host: host.into(),
                    shown: format!("https://{host}/o/r/pull/12"),
                    local,
                    outside_egress: false,
                    at: 1,
                    harness_pid: Some(std::process::id()),
                    command_digest: None,
                },
                harness_pid: std::process::id(),
            });
        gate.restart_arming(true);
        gate
    }

    fn gui_state() -> AppState {
        let mut gui = state();
        gui.caps = Capabilities {
            host_commands: false,
            persistent_answers: false,
            wait_for_presentation: true,
            commands: ahma_tui::state::CommandPolicy::Only(&[]),
        };
        gui
    }

    /// SPEC R-LINK.6, R-LINK.7, R-GUI.9: a link a hooked command was refused
    /// is the question every surface shows, ending with the guidance, and
    /// offers the TUI's answers in its order, deny first — with only deny and
    /// this site for this session given here, and each saving answer naming
    /// the grant line that gives it. It waits behind a scope or web question,
    /// as in the terminal, and counts as waiting.
    #[test]
    fn a_link_question_gives_deny_and_this_session_and_says_where_the_rest_are() {
        use link_values::*;
        let mut gui = gui_state();
        gui.link_ask = Some(link_gate("github.com", false));
        let g = gates(&gui);
        assert_eq!(g.waiting, 1);
        let Some(Gate::Link {
            decision_id,
            site,
            workspace,
            text,
            note,
            options,
        }) = g.top
        else {
            panic!("a link question: {:?}", g.top)
        };
        let ask = &gui.link_ask.as_ref().unwrap().ask;
        assert_eq!(decision_id, ask.decision_id());
        assert_eq!(site, "github.com");
        assert_eq!(workspace, ask.workspace.display().to_string());
        assert_eq!(text, ask.question().text(), "the one question body");
        assert!(text.ends_with(ahma_common::open_url::GUIDANCE), "{text}");
        assert_eq!(note, ahma_tui::harness_asks::LINK_NOT_PAUSED);
        assert_eq!(
            options.iter().map(|o| o.value.as_str()).collect::<Vec<_>>(),
            [
                DENY,
                SESSION,
                ALWAYS_SITE_THIS_REPO,
                ALWAYS_SITE_ALL_REPOS,
                ALWAYS_ANY_THIS_REPO
            ],
            "the TUI's answers, in its order"
        );
        let given: Vec<_> = options
            .iter()
            .filter(|o| o.allowed)
            .map(|o| o.value.as_str())
            .collect();
        assert_eq!(given, [DENY, SESSION], "no saving answer, no \"this time\"");
        assert!(!options[0].granting && options[0].label.contains("default"));
        assert!(options[1..].iter().all(|o| o.granting));
        for saving in options.iter().filter(|o| !o.allowed) {
            let hint = saving.hint.as_deref().unwrap_or_default();
            assert!(
                hint.contains("`ahma tui`") && hint.contains("ahma permissions grant open-url"),
                "{hint}"
            );
        }
        assert!(
            options[2]
                .hint
                .as_deref()
                .is_some_and(|h| h.contains(&format!(
                    "`ahma permissions grant open-url github.com --workspace '{}'`",
                    ask.workspace.display()
                ))),
            "{:?}",
            options[2].hint
        );

        let mut local = gui_state();
        local.link_ask = Some(link_gate("localhost", true));
        local.link_ask_queue.push_back(link_gate("docs.rs", false));
        let g = gates(&local);
        assert_eq!(g.waiting, 2, "the queue counts");
        let Some(Gate::Link { options, .. }) = g.top else {
            panic!()
        };
        assert!(
            options.iter().all(|o| o.value != ALWAYS_ANY_THIS_REPO),
            "no any-site answer for a local site"
        );

        scope_gate(&mut local);
        let g = gates(&local);
        assert!(
            matches!(g.top, Some(Gate::Scope { .. })),
            "a scope question first"
        );
        assert_eq!(g.waiting, 3);
    }

    /// SPEC R-LINK.6, R-GUI.9: the three example links in `ahma web` and
    /// `ahma desktop`: the TUI's rows in its order, each lasting one naming
    /// the site it would save, and only deny and this session given here.
    #[test]
    fn the_three_examples_show_this_repo_first_and_give_deny_and_session() {
        use link_values::*;
        for host in ["github.com", "api.github.com", "someone.github.io"] {
            let mut gui = gui_state();
            gui.link_ask = Some(link_gate(host, false));
            let Some(Gate::Link { options, .. }) = gates(&gui).top else {
                panic!("a link question")
            };
            let rows: Vec<(String, String, bool)> = options
                .iter()
                .map(|o| (o.value.clone(), o.label.clone(), o.allowed))
                .collect();
            let want = [
                (DENY, "Deny (default)".to_string(), true),
                (SESSION, "This site, this session".into(), true),
                (
                    ALWAYS_SITE_THIS_REPO,
                    format!("Always for {host} — this repo only"),
                    false,
                ),
                (
                    ALWAYS_SITE_ALL_REPOS,
                    format!("Always for {host} — all repos"),
                    false,
                ),
                (
                    ALWAYS_ANY_THIS_REPO,
                    "Always for any site — this repo only".into(),
                    false,
                ),
            ]
            .map(|(v, l, a)| (v.to_string(), l, a));
            assert_eq!(rows, want, "{host}");
        }
    }

    /// The values the page sends back, as `links::policy::LinkChoice::value`
    /// spells them (pinned here so a rename is seen).
    mod link_values {
        pub const DENY: &str = "deny";
        pub const SESSION: &str = "session";
        pub const ALWAYS_SITE_THIS_REPO: &str = "always-site-this-repo";
        pub const ALWAYS_SITE_ALL_REPOS: &str = "always-site-all-repos";
        pub const ALWAYS_ANY_THIS_REPO: &str = "always-any-site-this-repo";
    }

    #[test]
    fn with_nothing_asked_there_is_no_gate() {
        let g = gates(&state());
        assert!(g.top.is_none() && g.waiting == 0);
    }

    // ── a whole view ─────────────────────────────────────────────────────────

    #[test]
    fn a_whole_view_carries_every_region_and_serializes() {
        let mut s = state();
        s.chat.push(ChatEntry::User {
            text: "hello".into(),
            payload: None,
            started_at: None,
            duration_ms: None,
        });
        s.chat.push(ChatEntry::Assistant {
            content: "hi **there**".into(),
            streaming: false,
        });
        let v = view(&s, 3);
        assert_eq!((v.protocol, v.rev), (PROTOCOL, 3));
        assert!(v.header.is_some() && v.work.is_some() && v.log.is_some() && v.gates.is_some());
        let chat = v.chat.as_ref().unwrap();
        assert_eq!(chat.entries.len(), 2);
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["chat"]["entries"][0]["kind"], "user");
        assert_eq!(json["chat"]["entries"][1]["kind"], "assistant");
    }

    /// Whatever a path or a title says, it reaches the page cleaned.
    #[test]
    fn what_a_row_names_is_cleaned_before_it_is_sent() {
        let mut s = state();
        let mut op = Operation::new("op1\u{202E}", "run_terminal_command", OpStatus::Running);
        op.title = Some("rm \u{202E}txt.sh".into());
        s.operations.push(op);
        let r = row(&s.operations[0], false, vec![]);
        assert!(
            !r.id.contains('\u{202E}') && !r.title.contains('\u{202E}'),
            "{r:?}"
        );
    }
}
