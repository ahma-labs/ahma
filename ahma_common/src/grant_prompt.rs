//! The one prompt body every grant surface renders (SPEC R-PERM.3.4).
//!
//! A permission question a hurried human can actually judge carries, in this
//! order: who is asking, what was blocked, what the agent claims it needs,
//! the minimum that would work, what a grant lets every later command do, the
//! risk, and the exact settings line an `always` answer would write — then the
//! choices, deny first. The elicitation form, the TUI modal, the hook's
//! terminal text and the `sandbox_grant` tool result all render *this*
//! structure, so no surface can quietly be shorter than another, and the
//! literature's failure mode — a bare "grant X?" reaching someone minutes later
//! with no context — cannot come back one surface at a time.

use std::path::Path;

use crate::config::ScopeAccess;
use crate::scope_grant::{GrantDecision, GrantReason, ScopeGrantRequest};

/// One titled block of the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSection {
    pub heading: String,
    pub body: String,
}

/// One answer the human may give, with the key the TUI binds and the value the
/// elicitation form sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptOption {
    pub decision: GrantDecision,
    /// TUI key (`n`, `r`, `y`, `R`, `Y`, `o`, `w`).
    pub key: char,
    /// The elicitation form value (`deny`, `read-only-session`, …).
    pub value: &'static str,
    /// Human label, shown as the form's option title and in the modal.
    pub label: String,
}

/// The rendered question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptBody {
    /// One line: what is being asked.
    pub title: String,
    /// The seven sections, in the order SPEC R-PERM.3.4 fixes.
    pub sections: Vec<PromptSection>,
    /// The choices, deny first.
    pub options: Vec<PromptOption>,
}

impl PromptBody {
    /// The question without its choices: the title and every section.
    ///
    /// What an elicitation message, a terminal hook and a relayed tool result
    /// show. Each of those carries the choices its own way — the form as a
    /// titled select, the hook and the tool result as the exact commands — so
    /// listing them again here, with TUI key letters nobody can press there,
    /// only adds noise to the one screen a hurried human reads.
    pub fn to_message(&self) -> String {
        let mut out = format!("{}\n", self.title);
        for s in &self.sections {
            out.push_str(&format!(
                "\n{}\n  {}\n",
                s.heading,
                s.body.replace('\n', "\n  ")
            ));
        }
        out
    }

    /// [`Self::to_message`] followed by the choices, by name, deny first.
    pub fn to_text(&self) -> String {
        let mut out = self.to_message();
        out.push_str("\nChoices (deny is the default):\n");
        for o in &self.options {
            out.push_str(&format!("  {:<20} {}\n", o.value, o.label));
        }
        out
    }
}

fn unknown() -> String {
    "unknown".to_string()
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Render `req` into the body every surface shows.
pub fn render(req: &ScopeGrantRequest) -> PromptBody {
    let ctx = &req.context;
    let path = req.path.display().to_string();
    let workspace = ctx
        .requester
        .as_ref()
        .and_then(|r| r.workspace.as_deref())
        .map(|w| w.display().to_string());
    let want_write = ctx.write_denied || req.access.is_write();
    // `--tmp` asks for exactly the access the flag means; nothing was refused,
    // so "minimum that would work" is the request itself.
    let startup = req.reason == GrantReason::StartupFlag;
    // `logs_approve`: nothing was refused either, and the answer is a
    // read-only `log-target` grant, never an `fs-scope` one (SPEC R9.2).
    let log_target = req.reason == GrantReason::LogTarget;
    // A terminal-hook refusal answered ahead of the harness dialog
    // (R-PERM.10(e)): the session is the harness's, and there is no once.
    let harness = req.reason == GrantReason::HarnessRefusal;
    let saved_tiers = req.reason.offers_saved_tiers();
    let minimum_access = if ctx.write_denied || (startup && req.access.is_write()) {
        ScopeAccess::Rw
    } else {
        ScopeAccess::Ro
    };

    // 1. Who is asking
    let who = match &ctx.requester {
        Some(r) => format!(
            "{} · workspace {} · session {}{}",
            r.client.clone().unwrap_or_else(unknown),
            workspace.clone().unwrap_or_else(unknown),
            r.session_id
                .as_deref()
                .map(short_id)
                .unwrap_or_else(unknown),
            if r.pid != 0 {
                format!(" · pid {}", r.pid)
            } else {
                String::new()
            }
        ),
        None => "unknown session (a terminal hook, or an older ahma)".to_string(),
    };

    // 2. What was blocked
    let mut blocked = if log_target {
        format!(
            "nothing was blocked: a log file in .ahma/logs links to this file outside the \
             workspace; approving lets ahma's log tools read it: {path}"
        )
    } else if startup {
        format!(
            "nothing was blocked: this server was started with --tmp (or [sandbox] tmp_access \
             = true), which asks for {} access to the system temp directory {}",
            minimum_access.label(),
            path
        )
    } else {
        format!(
            "{} {} {}",
            req.tool.as_deref().unwrap_or("a sandboxed command"),
            if want_write {
                "tried to write"
            } else {
                "tried to read"
            },
            ctx.evidence
                .as_ref()
                .and_then(|e| e.raw_path.as_deref())
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| path.clone())
        )
    };
    if let Some(cmd) = &ctx.command {
        blocked.push_str(&format!("\ncommand: {cmd}"));
    }
    match req.reason {
        GrantReason::PreExecViolation => {
            blocked.push_str("\nblocked before it ran: the path is exact")
        }
        GrantReason::StderrHeuristic => {
            blocked.push_str("\nread from the command's error output: double-check the path")
        }
        GrantReason::KernelRecord => blocked.push_str(
            "\nthe path and the access are exact: the kernel's own record of the refusal",
        ),
        GrantReason::StartupFlag => blocked.push_str(
            "\nthe path is exact: this machine's temp directory, shared by every program you \
             run, and part of no project",
        ),
        GrantReason::LogTarget => blocked.push_str(
            "\nthe path is exact: where the link resolves now. The agent can create links in \
             .ahma/logs itself, so check this is a log you expect it to read",
        ),
        GrantReason::HarnessRefusal => blocked.push_str(
            "\nthe path is exact: the kernel refused it and ahma's terminal hook recorded it. \
             Unless you answer here, the agent's harness asks you in its own dialog before its \
             next command",
        ),
        GrantReason::Unknown => blocked.push_str(
            "\nsent by a newer ahma for a reason this version does not recognise: double-check \
             the path",
        ),
    }
    if let Some(e) = &ctx.evidence
        && let Some(line) = &e.line
    {
        blocked.push_str(&format!("\nevidence: {}", line.trim()));
    }
    if ctx.times_asked > 1 {
        blocked.push_str(&format!(
            "\nasked {} times this session (first at {})",
            ctx.times_asked,
            ctx.first_asked_at
                .map(crate::permissions::fmt_unix_secs)
                .unwrap_or_else(unknown)
        ));
    }

    // 3. What the agent says it needs
    let claim = match &ctx.agent_claim {
        Some(c) => format!(
            "\"{}\"\n(the agent's claim, in the agent's own words — it is the party asking, not a \
             witness)",
            c.trim()
        ),
        None => "nothing — the agent gave no reason".to_string(),
    };

    // 4. Minimum that would work
    let minimum = format!(
        "{} on {}{}",
        minimum_access.label(),
        path,
        if startup {
            " (what --tmp asks for; deny and it stays out of this session's scope)"
        } else if log_target {
            " (a log target is only ever read, never written)"
        } else if ctx.write_denied {
            " (a write was refused, so read-only would not fix it)"
        } else {
            " (no write was refused; read-only is enough until one is)"
        }
    );

    // 5. What a grant allows
    let workspace_label = workspace
        .clone()
        .unwrap_or_else(|| "this workspace".to_string());
    let verb = if minimum_access.is_write() {
        "read and write"
    } else {
        "read"
    };
    let allows = if log_target {
        format!(
            "ahma's log tools, and every command in {workspace_label}, may read {path} — until \
             this session ends (session), or until you revoke it (always). It is never writable, \
             and nothing else outside the workspace changes."
        )
    } else if harness {
        format!(
            "every command in {workspace_label} may {verb} {path} — until the agent's harness \
             exits (session: its hooked commands and edits, at most 12 hours), for 24 hours \
             (lease), or until you revoke it (always). Nothing else outside the workspace changes."
        )
    } else if saved_tiers {
        format!(
            "every command in {workspace_label} may {verb} {path} — for the next command \
             (once), until this session ends (session), for 24 hours (lease), or until you \
             revoke it (always). Nothing else outside the workspace changes."
        )
    } else {
        format!(
            "every command in {workspace_label} may {verb} {path} — for the next command \
             (once) or until this session ends (session). It is never saved. Nothing else \
             outside the workspace changes."
        )
    };

    // 6. Risk
    let risk = match &ctx.risk {
        Some(r) => {
            let mut s = r.class.to_uppercase();
            for w in &r.warnings {
                s.push_str(&format!("\n! {w}"));
            }
            for f in &r.facts {
                s.push_str(&format!("\n- {f}"));
            }
            s
        }
        None => "not assessed (no live scope to compare against)".to_string(),
    };

    // 7. The exact line `always` writes — or, for a session-only question,
    // that there is none.
    let always = if log_target {
        // A command to paste never carries a placeholder: with the workspace
        // unknown, the revoke runs from it (its default) instead.
        let (ws, revoke) = match workspace.clone() {
            Some(ws) => (
                ws.clone(),
                format!(
                    "Revoke any time with: ahma permissions revoke log-target {path} --workspace {ws}"
                ),
            ),
            None => (
                "<this workspace>".to_string(),
                format!(
                    "Revoke any time, from that workspace, with: ahma permissions revoke log-target {path}"
                ),
            ),
        };
        format!(
            "~/.ahma/settings.toml gets:\n[[log_targets.approvals]]\nworkspace = \"{ws}\"\ntargets = [\"{path}\"]\n{revoke}"
        )
    } else if saved_tiers {
        format!(
            "~/.ahma/settings.toml gets:\n[[sandbox.persistent_scopes]]\npath = \"{}\"\naccess = \"{}\"\nworkspace = \"{}\"\nRevoke any time with: ahma sandbox revoke {}",
            path,
            minimum_access.short(),
            workspace
                .clone()
                .unwrap_or_else(|| "<this workspace>".to_string()),
            path
        )
    } else if startup {
        "not offered: the temp directory is shared by the whole machine, so this answer is \
         never written to ~/.ahma/settings.toml. To stop being asked, start ahma without --tmp \
         and remove tmp_access from [sandbox] in ~/.ahma/settings.toml."
            .to_string()
    } else {
        "not offered for a question this version of ahma does not recognise: nothing is \
         written to ~/.ahma/settings.toml."
            .to_string()
    };

    let sections = vec![
        PromptSection {
            heading: "Who is asking".into(),
            body: who,
        },
        PromptSection {
            heading: "What was blocked".into(),
            body: blocked,
        },
        PromptSection {
            heading: "What the agent says it needs".into(),
            body: claim,
        },
        PromptSection {
            heading: "Minimum that would work".into(),
            body: minimum,
        },
        PromptSection {
            heading: "What a grant allows".into(),
            body: allows,
        },
        PromptSection {
            heading: "Risk".into(),
            body: risk,
        },
        PromptSection {
            heading: "If you choose always".into(),
            body: always,
        },
    ];

    PromptBody {
        title: format!("Allow {} access to {}?", minimum_access.label(), path),
        sections,
        options: options_for(req.reason),
    }
}

/// The choices a question raised for `reason` offers, deny first: every
/// option for a blocked path; only deny / once / session for a question
/// whose answer must not be saved ([`GrantReason::offers_saved_tiers`] — the
/// `--tmp` question); and only deny / read-only session / read-only always for
/// a log target; and everything but once for a terminal-hook refusal the TUI
/// answers ahead of the harness. Every surface builds its choices from this, so the form,
/// the TUI keys and the text body agree; [`crate::scope_grant::GrantCoordinator::resolve`]
/// holds any answer outside it to what it offers ([`GrantReason::offers`]).
pub fn options_for(reason: GrantReason) -> Vec<PromptOption> {
    options()
        .into_iter()
        // Deny is always offered (it is the default, R5.3.1).
        .filter(|o| reason.offers(o.decision))
        .collect()
}

/// The choices, deny first, narrowest tier first (SPEC R5.3.1: Enter never widens).
pub fn options() -> Vec<PromptOption> {
    vec![
        PromptOption {
            decision: GrantDecision::Deny,
            key: 'n',
            value: "deny",
            label: "Deny (default; Enter and Esc)".into(),
        },
        PromptOption {
            decision: GrantDecision::GrantRoOnce,
            key: 'o',
            value: "read-only-once",
            label: "read-only, next command only".into(),
        },
        PromptOption {
            decision: GrantDecision::GrantRwOnce,
            key: 'w',
            value: "read-write-once",
            label: "read-write, next command only".into(),
        },
        PromptOption {
            decision: GrantDecision::GrantRoSession,
            key: 'r',
            value: "read-only-session",
            label: "read-only, this session".into(),
        },
        PromptOption {
            decision: GrantDecision::GrantRwSession,
            key: 'y',
            value: "read-write-session",
            label: "read-write, this session".into(),
        },
        PromptOption {
            decision: GrantDecision::GrantRoLease,
            key: 'l',
            value: "read-only-24h",
            label: "read-only for 24 hours (saved; ends on its own)".into(),
        },
        PromptOption {
            decision: GrantDecision::GrantRwLease,
            key: 'L',
            value: "read-write-24h",
            label: "read-write for 24 hours (saved; ends on its own)".into(),
        },
        PromptOption {
            decision: GrantDecision::GrantRo,
            key: 'R',
            value: "read-only",
            label: "read-only, always (saved; bound to this workspace)".into(),
        },
        PromptOption {
            decision: GrantDecision::GrantRw,
            key: 'Y',
            value: "read-write",
            label: "read-write, always (saved; bound to this workspace)".into(),
        },
    ]
}

/// Map an elicitation form value (or a loose spelling of it) to a decision.
/// Anything unrecognised is a **deny**: an answer we cannot read is not consent.
pub fn parse_decision(s: &str) -> GrantDecision {
    match s.trim().to_ascii_lowercase().as_str() {
        "read-write" | "read_write" | "rw" | "write" | "read-write-always" => {
            GrantDecision::GrantRw
        }
        "read-only" | "read_only" | "ro" | "read" | "read-only-always" => GrantDecision::GrantRo,
        "read-write-session" | "rw-session" | "write-session" => GrantDecision::GrantRwSession,
        "read-only-session" | "ro-session" | "read-session" => GrantDecision::GrantRoSession,
        "read-write-once" | "rw-once" | "write-once" => GrantDecision::GrantRwOnce,
        "read-only-once" | "ro-once" | "read-once" => GrantDecision::GrantRoOnce,
        "read-write-24h" | "rw-24h" | "read-write-lease" => GrantDecision::GrantRwLease,
        "read-only-24h" | "ro-24h" | "read-only-lease" => GrantDecision::GrantRoLease,
        _ => GrantDecision::Deny,
    }
}

/// The body for a denial a terminal hook reports, where no coordinator request
/// exists: the same sections, built from what the hook knows. `context` carries
/// who asked and the risk; the first line of `details` becomes the evidence when
/// the context has none.
pub fn render_for_hook(
    path: &Path,
    access: ScopeAccess,
    details: &str,
    mut context: crate::scope_grant::GrantContext,
) -> PromptBody {
    if context.evidence.is_none() {
        context.evidence = Some(crate::scope_grant::GrantEvidence {
            raw_path: None,
            pattern: None,
            line: Some(details.lines().next().unwrap_or("").to_string()),
        });
    }
    context.write_denied = context.write_denied || access.is_write();
    let req = ScopeGrantRequest {
        decision_id: String::new(),
        path: path.to_path_buf(),
        access,
        reason: GrantReason::StderrHeuristic,
        tool: Some("a hooked shell command".into()),
        context,
    };
    render(&req)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_form_value_round_trips_and_garbage_denies() {
        for o in options() {
            assert_eq!(parse_decision(o.value), o.decision, "{}", o.value);
        }
        assert_eq!(parse_decision("yes please"), GrantDecision::Deny);
        assert_eq!(options()[0].decision, GrantDecision::Deny);
        let keys: Vec<char> = options().iter().map(|o| o.key).collect();
        let mut dedup = keys.clone();
        dedup.sort_unstable();
        dedup.dedup();
        assert_eq!(keys.len(), dedup.len(), "keys are unique: {keys:?}");
    }

    /// A request with every judgement aid filled in, the shape the MCP worker
    /// raises for a runtime denial.
    fn full_request() -> ScopeGrantRequest {
        use crate::scope_grant::{GrantContext, GrantEvidence, GrantRequester, GrantRiskSummary};
        ScopeGrantRequest {
            decision_id: "d-golden".into(),
            path: "/Users/u/Library/Caches/sccache".into(),
            access: ScopeAccess::Rw,
            reason: GrantReason::StderrHeuristic,
            tool: Some("cargo_build".into()),
            context: GrantContext {
                requester: Some(GrantRequester {
                    client: Some("claude-code".into()),
                    session_id: Some("8d387500-2ff3-4b2e-9a51".into()),
                    workspace: Some("/Users/u/github/proj".into()),
                    pid: 4242,
                }),
                op_id: Some("op_7".into()),
                command: Some("cargo build --release".into()),
                evidence: Some(GrantEvidence {
                    raw_path: Some("/Users/u/Library/Caches/sccache/0/1/obj".into()),
                    pattern: Some("Operation not permitted".into()),
                    line: Some(
                        "  error: failed to write /Users/u/Library/Caches/sccache/0/1/obj: \
                         Operation not permitted  "
                            .into(),
                    ),
                }),
                agent_claim: Some("sccache keeps its compiler cache there".into()),
                risk: Some(GrantRiskSummary {
                    class: "high".into(),
                    warnings: vec!["is a hidden-data directory in your home folder".into()],
                    facts: vec!["directory with 12 entries".into()],
                }),
                times_asked: 2,
                first_asked_at: Some(3_600 * 9 + 60 * 5),
                write_denied: true,
            },
        }
    }

    /// GOLDEN: the exact text a human reads at an elicitation prompt, a
    /// terminal hook and a relayed tool result (SPEC R-PERM.3.4). Substring
    /// assertions let a section quietly lose its content while the heading
    /// survives; a byte-for-byte comparison makes every change to what the
    /// human reads a reviewed diff. To accept an intended change, update
    /// `testdata/grant_prompt_full.txt`.
    #[test]
    fn the_full_prompt_reads_exactly_as_reviewed() {
        let got = render(&full_request()).to_message();
        let want = include_str!("testdata/grant_prompt_full.txt");
        assert_eq!(
            got, want,
            "the grant prompt changed; review it and update testdata/grant_prompt_full.txt:\n{got}"
        );
    }

    /// The choices are listed by name, deny first, with no TUI key letters:
    /// nobody can press a key in a terminal hook or a relayed tool result.
    #[test]
    fn the_text_form_names_choices_without_keys() {
        let text = render(&full_request()).to_text();
        assert!(text.contains("Choices (deny is the default):"), "{text}");
        let choices = text.split("Choices").nth(1).unwrap();
        assert!(
            choices
                .trim_start_matches(|c| c != '\n')
                .trim_start()
                .starts_with("deny")
        );
        assert!(!text.contains("[n]") && !text.contains("[Y]"), "{text}");
    }

    fn tmp_request() -> ScopeGrantRequest {
        ScopeGrantRequest {
            decision_id: "d-tmp".into(),
            path: "/private/var/folders/xy/T".into(),
            access: ScopeAccess::Rw,
            reason: GrantReason::StartupFlag,
            tool: Some("--tmp".into()),
            context: Default::default(),
        }
    }

    /// SPEC R5.3: the `--tmp` question says nothing was blocked, names the
    /// flag and the literal temp path, and asks for what the flag means.
    #[test]
    fn the_tmp_question_says_nothing_was_blocked_and_names_the_path() {
        let body = render(&tmp_request());
        let text = body.to_message();
        assert!(text.contains("nothing was blocked"), "{text}");
        assert!(text.contains("--tmp"), "{text}");
        assert!(text.contains("/private/var/folders/xy/T"), "{text}");
        assert!(
            !text.contains("tried to write"),
            "nothing tried anything: {text}"
        );
        assert!(
            body.title.contains(ScopeAccess::Rw.label()),
            "--tmp asks for read-write: {}",
            body.title
        );
        assert!(
            !text.contains("[[sandbox.persistent_scopes]]"),
            "no settings line is ever written for it: {text}"
        );
        assert_eq!(
            body.sections.len(),
            7,
            "the same seven sections (R-PERM.3.4)"
        );
    }

    /// The machine-wide temp directory is offered for the session at most:
    /// deny, once, session — never always, never 24 hours.
    #[test]
    fn the_tmp_question_offers_only_deny_once_and_session() {
        let offered = options_for(GrantReason::StartupFlag);
        assert_eq!(offered[0].decision, GrantDecision::Deny, "deny first");
        assert!(
            offered
                .iter()
                .filter(|o| o.decision.access().is_some())
                .all(|o| !o.decision.tier().is_persistent()),
            "{offered:#?}"
        );
        let values: Vec<&str> = offered.iter().map(|o| o.value).collect();
        assert_eq!(
            values,
            [
                "deny",
                "read-only-once",
                "read-write-once",
                "read-only-session",
                "read-write-session"
            ]
        );
        assert_eq!(render(&tmp_request()).options, offered);
        assert_eq!(options_for(GrantReason::Unknown), offered);
        assert_eq!(options_for(GrantReason::PreExecViolation), options());
        assert_eq!(options_for(GrantReason::StderrHeuristic), options());
    }

    fn log_target_request() -> ScopeGrantRequest {
        ScopeGrantRequest {
            decision_id: "d-log".into(),
            path: "/opt/app/logs/app.log".into(),
            access: ScopeAccess::Ro,
            reason: GrantReason::LogTarget,
            tool: Some("logs_approve".into()),
            context: Default::default(),
        }
    }

    /// SPEC R9.2: the `logs_approve` question says what the link is and what
    /// approving does, and an `always` answer names the `log-target` row it
    /// writes — never a `persistent_scopes` line.
    #[test]
    fn the_log_target_question_says_what_the_link_is_and_what_always_writes() {
        let body = render(&log_target_request());
        let text = body.to_message();
        assert!(
            text.contains(
                "a log file in .ahma/logs links to this file outside the workspace; approving \
                 lets ahma's log tools read it"
            ),
            "{text}"
        );
        assert!(text.contains("/opt/app/logs/app.log"), "{text}");
        assert!(!text.contains("tried to"), "nothing tried anything: {text}");
        assert!(
            body.title.contains(ScopeAccess::Ro.label()),
            "a log target is read-only: {}",
            body.title
        );
        assert!(text.contains("[[log_targets.approvals]]"), "{text}");
        assert!(
            text.contains("ahma permissions revoke log-target"),
            "{text}"
        );
        assert!(
            !text.contains("[[sandbox.persistent_scopes]]"),
            "an always answer is a log-target row, not an fs-scope grant: {text}"
        );
        assert_eq!(
            body.sections.len(),
            7,
            "the same seven sections (R-PERM.3.4)"
        );
    }

    /// Deny, read-only for the session, read-only always: nothing writable,
    /// no once, no 24 hours.
    #[test]
    fn the_log_target_question_offers_deny_session_and_always_read_only() {
        let offered = options_for(GrantReason::LogTarget);
        let values: Vec<&str> = offered.iter().map(|o| o.value).collect();
        assert_eq!(values, ["deny", "read-only-session", "read-only"]);
        assert_eq!(render(&log_target_request()).options, offered);
    }

    /// A command the human may paste never carries a placeholder: with the
    /// workspace unknown, the log-target revoke runs from that workspace.
    #[test]
    fn a_revoke_command_never_carries_a_placeholder() {
        let text = render(&log_target_request()).to_message();
        let revoke = text
            .lines()
            .find(|l| l.contains("ahma permissions revoke log-target"))
            .expect("the always section says how to revoke");
        assert!(!revoke.contains('<'), "{revoke}");
        assert!(revoke.contains("from that workspace"), "{revoke}");
    }

    // ── golden text, one per surface shape (SPEC R-PERM.3.4) ────────────────
    //
    // Every constant is the exact text a human reads. To accept an intended
    // change, review the diff the failing assertion prints and paste the new
    // text here; a byte-for-byte comparison is the point.

    /// A sender that knows nothing (no requester, no evidence, no claim, no
    /// risk) still yields all seven sections, each saying so.
    fn minimal_request() -> ScopeGrantRequest {
        ScopeGrantRequest {
            decision_id: "d-min".into(),
            path: "/srv/data".into(),
            access: ScopeAccess::Ro,
            reason: GrantReason::PreExecViolation,
            tool: None,
            context: Default::default(),
        }
    }

    /// A requester that is present but empty: every field reads "unknown",
    /// and pid 0 is omitted rather than printed.
    fn blank_requester_request() -> ScopeGrantRequest {
        let mut req = minimal_request();
        req.decision_id = "d-blank".into();
        req.context.requester = Some(crate::scope_grant::GrantRequester::default());
        req
    }

    /// A reason sent by a newer ahma: no saved tiers, and the always section
    /// says why.
    fn unknown_reason_request() -> ScopeGrantRequest {
        ScopeGrantRequest {
            decision_id: "d-unknown".into(),
            path: "/srv/data".into(),
            access: ScopeAccess::Ro,
            reason: GrantReason::Unknown,
            tool: Some("future_tool".into()),
            context: Default::default(),
        }
    }

    /// What a terminal hook knows, fully specified. The real hook's pid and
    /// risk come from the machine (`std::process::id()`, a stat of the
    /// target); here they are fixed so the text is the same everywhere.
    fn hook_body() -> PromptBody {
        use crate::scope_grant::{GrantContext, GrantRequester, GrantRiskSummary};
        render_for_hook(
            Path::new("/opt/cache"),
            ScopeAccess::Rw,
            "write to /opt/cache/x.bin: Operation not permitted\nsecond line is not evidence",
            GrantContext {
                requester: Some(GrantRequester {
                    client: Some("Claude Code (terminal hook)".into()),
                    session_id: Some("8d387500-2ff3-4b2e".into()),
                    workspace: Some("/home/u/proj".into()),
                    pid: 4242,
                }),
                command: Some("cargo build --release".into()),
                risk: Some(GrantRiskSummary {
                    class: "normal".into(),
                    warnings: vec![],
                    facts: vec!["directory with 3 entries".into()],
                }),
                ..Default::default()
            },
        )
    }

    /// Fail with the full text, so accepting a change is a copy, not a hunt.
    #[track_caller]
    fn assert_golden(what: &str, got: &str, want: &str) {
        assert_eq!(
            got, want,
            "the {what} grant prompt changed; review it and update its golden constant:\n{got}"
        );
    }

    const GOLDEN_FULL_TEXT: &str = r#"Allow read+write access to /Users/u/Library/Caches/sccache?

Who is asking
  claude-code · workspace /Users/u/github/proj · session 8d387500 · pid 4242

What was blocked
  cargo_build tried to write /Users/u/Library/Caches/sccache/0/1/obj
  command: cargo build --release
  read from the command's error output: double-check the path
  evidence: error: failed to write /Users/u/Library/Caches/sccache/0/1/obj: Operation not permitted
  asked 2 times this session (first at 09:05 UTC)

What the agent says it needs
  "sccache keeps its compiler cache there"
  (the agent's claim, in the agent's own words — it is the party asking, not a witness)

Minimum that would work
  read+write on /Users/u/Library/Caches/sccache (a write was refused, so read-only would not fix it)

What a grant allows
  every command in /Users/u/github/proj may read and write /Users/u/Library/Caches/sccache — for the next command (once), until this session ends (session), for 24 hours (lease), or until you revoke it (always). Nothing else outside the workspace changes.

Risk
  HIGH
  ! is a hidden-data directory in your home folder
  - directory with 12 entries

If you choose always
  ~/.ahma/settings.toml gets:
  [[sandbox.persistent_scopes]]
  path = "/Users/u/Library/Caches/sccache"
  access = "rw"
  workspace = "/Users/u/github/proj"
  Revoke any time with: ahma sandbox revoke /Users/u/Library/Caches/sccache

Choices (deny is the default):
  deny                 Deny (default; Enter and Esc)
  read-only-once       read-only, next command only
  read-write-once      read-write, next command only
  read-only-session    read-only, this session
  read-write-session   read-write, this session
  read-only-24h        read-only for 24 hours (saved; ends on its own)
  read-write-24h       read-write for 24 hours (saved; ends on its own)
  read-only            read-only, always (saved; bound to this workspace)
  read-write           read-write, always (saved; bound to this workspace)
"#;

    const GOLDEN_MINIMAL_MESSAGE: &str = r#"Allow read-only access to /srv/data?

Who is asking
  unknown session (a terminal hook, or an older ahma)

What was blocked
  a sandboxed command tried to read /srv/data
  blocked before it ran: the path is exact

What the agent says it needs
  nothing — the agent gave no reason

Minimum that would work
  read-only on /srv/data (no write was refused; read-only is enough until one is)

What a grant allows
  every command in this workspace may read /srv/data — for the next command (once), until this session ends (session), for 24 hours (lease), or until you revoke it (always). Nothing else outside the workspace changes.

Risk
  not assessed (no live scope to compare against)

If you choose always
  ~/.ahma/settings.toml gets:
  [[sandbox.persistent_scopes]]
  path = "/srv/data"
  access = "ro"
  workspace = "<this workspace>"
  Revoke any time with: ahma sandbox revoke /srv/data
"#;

    const GOLDEN_BLANK_REQUESTER_MESSAGE: &str = r#"Allow read-only access to /srv/data?

Who is asking
  unknown · workspace unknown · session unknown

What was blocked
  a sandboxed command tried to read /srv/data
  blocked before it ran: the path is exact

What the agent says it needs
  nothing — the agent gave no reason

Minimum that would work
  read-only on /srv/data (no write was refused; read-only is enough until one is)

What a grant allows
  every command in this workspace may read /srv/data — for the next command (once), until this session ends (session), for 24 hours (lease), or until you revoke it (always). Nothing else outside the workspace changes.

Risk
  not assessed (no live scope to compare against)

If you choose always
  ~/.ahma/settings.toml gets:
  [[sandbox.persistent_scopes]]
  path = "/srv/data"
  access = "ro"
  workspace = "<this workspace>"
  Revoke any time with: ahma sandbox revoke /srv/data
"#;

    const GOLDEN_UNKNOWN_REASON_TEXT: &str = r#"Allow read-only access to /srv/data?

Who is asking
  unknown session (a terminal hook, or an older ahma)

What was blocked
  future_tool tried to read /srv/data
  sent by a newer ahma for a reason this version does not recognise: double-check the path

What the agent says it needs
  nothing — the agent gave no reason

Minimum that would work
  read-only on /srv/data (no write was refused; read-only is enough until one is)

What a grant allows
  every command in this workspace may read /srv/data — for the next command (once) or until this session ends (session). It is never saved. Nothing else outside the workspace changes.

Risk
  not assessed (no live scope to compare against)

If you choose always
  not offered for a question this version of ahma does not recognise: nothing is written to ~/.ahma/settings.toml.

Choices (deny is the default):
  deny                 Deny (default; Enter and Esc)
  read-only-once       read-only, next command only
  read-write-once      read-write, next command only
  read-only-session    read-only, this session
  read-write-session   read-write, this session
"#;

    const GOLDEN_HOOK_MESSAGE: &str = r#"Allow read+write access to /opt/cache?

Who is asking
  Claude Code (terminal hook) · workspace /home/u/proj · session 8d387500 · pid 4242

What was blocked
  a hooked shell command tried to write /opt/cache
  command: cargo build --release
  read from the command's error output: double-check the path
  evidence: write to /opt/cache/x.bin: Operation not permitted

What the agent says it needs
  nothing — the agent gave no reason

Minimum that would work
  read+write on /opt/cache (a write was refused, so read-only would not fix it)

What a grant allows
  every command in /home/u/proj may read and write /opt/cache — for the next command (once), until this session ends (session), for 24 hours (lease), or until you revoke it (always). Nothing else outside the workspace changes.

Risk
  NORMAL
  - directory with 3 entries

If you choose always
  ~/.ahma/settings.toml gets:
  [[sandbox.persistent_scopes]]
  path = "/opt/cache"
  access = "rw"
  workspace = "/home/u/proj"
  Revoke any time with: ahma sandbox revoke /opt/cache
"#;

    const GOLDEN_TMP_TEXT: &str = r#"Allow read+write access to /private/var/folders/xy/T?

Who is asking
  unknown session (a terminal hook, or an older ahma)

What was blocked
  nothing was blocked: this server was started with --tmp (or [sandbox] tmp_access = true), which asks for read+write access to the system temp directory /private/var/folders/xy/T
  the path is exact: this machine's temp directory, shared by every program you run, and part of no project

What the agent says it needs
  nothing — the agent gave no reason

Minimum that would work
  read+write on /private/var/folders/xy/T (what --tmp asks for; deny and it stays out of this session's scope)

What a grant allows
  every command in this workspace may read and write /private/var/folders/xy/T — for the next command (once) or until this session ends (session). It is never saved. Nothing else outside the workspace changes.

Risk
  not assessed (no live scope to compare against)

If you choose always
  not offered: the temp directory is shared by the whole machine, so this answer is never written to ~/.ahma/settings.toml. To stop being asked, start ahma without --tmp and remove tmp_access from [sandbox] in ~/.ahma/settings.toml.

Choices (deny is the default):
  deny                 Deny (default; Enter and Esc)
  read-only-once       read-only, next command only
  read-write-once      read-write, next command only
  read-only-session    read-only, this session
  read-write-session   read-write, this session
"#;

    const GOLDEN_LOG_TARGET_TEXT: &str = r#"Allow read-only access to /opt/app/logs/app.log?

Who is asking
  unknown session (a terminal hook, or an older ahma)

What was blocked
  nothing was blocked: a log file in .ahma/logs links to this file outside the workspace; approving lets ahma's log tools read it: /opt/app/logs/app.log
  the path is exact: where the link resolves now. The agent can create links in .ahma/logs itself, so check this is a log you expect it to read

What the agent says it needs
  nothing — the agent gave no reason

Minimum that would work
  read-only on /opt/app/logs/app.log (a log target is only ever read, never written)

What a grant allows
  ahma's log tools, and every command in this workspace, may read /opt/app/logs/app.log — until this session ends (session), or until you revoke it (always). It is never writable, and nothing else outside the workspace changes.

Risk
  not assessed (no live scope to compare against)

If you choose always
  ~/.ahma/settings.toml gets:
  [[log_targets.approvals]]
  workspace = "<this workspace>"
  targets = ["/opt/app/logs/app.log"]
  Revoke any time, from that workspace, with: ahma permissions revoke log-target /opt/app/logs/app.log

Choices (deny is the default):
  deny                 Deny (default; Enter and Esc)
  read-only-session    read-only, this session
  read-only            read-only, always (saved; bound to this workspace)
"#;

    /// A terminal-hook refusal as the TUI lists it ahead of the harness
    /// dialog (SPEC R-PERM.10(e)): the harness process asks, the refused paths
    /// are the evidence, and there is no command or claim.
    fn harness_refusal_request() -> ScopeGrantRequest {
        use crate::scope_grant::{GrantContext, GrantEvidence, GrantRequester, GrantRiskSummary};
        ScopeGrantRequest {
            decision_id: "harness-ask-0123456789abcdef".into(),
            path: "/home/u/.cache/neubit".into(),
            access: ScopeAccess::Rw,
            reason: GrantReason::HarnessRefusal,
            tool: Some("an earlier hooked command".into()),
            context: GrantContext {
                requester: Some(GrantRequester {
                    client: Some("terminal hook".into()),
                    session_id: None,
                    workspace: Some("/home/u/proj".into()),
                    pid: 4242,
                }),
                evidence: Some(GrantEvidence {
                    raw_path: Some("/home/u/.cache/neubit/heavy.lock.holder".into()),
                    pattern: None,
                    line: Some(
                        "refused: /home/u/.cache/neubit/heavy.lock.holder, \
                         /home/u/.cache/neubit/db/index.db"
                            .into(),
                    ),
                }),
                risk: Some(GrantRiskSummary {
                    class: "normal".into(),
                    warnings: vec![],
                    facts: vec!["directory with 2 entries".into()],
                }),
                write_denied: true,
                ..Default::default()
            },
        }
    }

    const GOLDEN_HARNESS_REFUSAL_TEXT: &str = r#"Allow read+write access to /home/u/.cache/neubit?

Who is asking
  terminal hook · workspace /home/u/proj · session unknown · pid 4242

What was blocked
  an earlier hooked command tried to write /home/u/.cache/neubit/heavy.lock.holder
  the path is exact: the kernel refused it and ahma's terminal hook recorded it. Unless you answer here, the agent's harness asks you in its own dialog before its next command
  evidence: refused: /home/u/.cache/neubit/heavy.lock.holder, /home/u/.cache/neubit/db/index.db

What the agent says it needs
  nothing — the agent gave no reason

Minimum that would work
  read+write on /home/u/.cache/neubit (a write was refused, so read-only would not fix it)

What a grant allows
  every command in /home/u/proj may read and write /home/u/.cache/neubit — until the agent's harness exits (session: its hooked commands and edits, at most 12 hours), for 24 hours (lease), or until you revoke it (always). Nothing else outside the workspace changes.

Risk
  NORMAL
  - directory with 2 entries

If you choose always
  ~/.ahma/settings.toml gets:
  [[sandbox.persistent_scopes]]
  path = "/home/u/.cache/neubit"
  access = "rw"
  workspace = "/home/u/proj"
  Revoke any time with: ahma sandbox revoke /home/u/.cache/neubit

Choices (deny is the default):
  deny                 Deny (default; Enter and Esc)
  read-only-session    read-only, this session
  read-write-session   read-write, this session
  read-only-24h        read-only for 24 hours (saved; ends on its own)
  read-write-24h       read-write for 24 hours (saved; ends on its own)
  read-only            read-only, always (saved; bound to this workspace)
  read-write           read-write, always (saved; bound to this workspace)
"#;

    /// SPEC R-PERM.10(e): the TUI's copy of a harness question says the path
    /// is exact and who will ask if nobody answers here, and offers every
    /// tier but once — nothing would spend a once answer.
    #[test]
    fn golden_harness_refusal_question() {
        assert_golden(
            "harness refusal",
            &render(&harness_refusal_request()).to_text(),
            GOLDEN_HARNESS_REFUSAL_TEXT,
        );
        let offered = options_for(GrantReason::HarnessRefusal);
        assert_eq!(offered[0].decision, GrantDecision::Deny, "deny first");
        assert!(
            offered
                .iter()
                .all(|o| o.decision.tier() != crate::permissions::GrantTier::Once),
            "{offered:#?}"
        );
        assert_eq!(
            offered.len(),
            options().len() - 2,
            "only the two once tiers go"
        );
        assert_eq!(
            GrantDecision::GrantRwOnce.within_offer(GrantReason::HarnessRefusal),
            GrantDecision::Deny,
            "a once answer from anywhere is narrowed to deny, never widened"
        );
        assert_eq!(
            GrantDecision::GrantRwSession.within_offer(GrantReason::HarnessRefusal),
            GrantDecision::GrantRwSession
        );
        assert_eq!(
            GrantDecision::GrantRo.within_offer(GrantReason::HarnessRefusal),
            GrantDecision::GrantRo
        );
    }

    /// The body plus the choices by name — the shape a text-only surface
    /// would print. Deny first, every tier the reason offers, no key letters.
    #[test]
    fn golden_full_request_with_choices() {
        assert_golden(
            "full (to_text)",
            &render(&full_request()).to_text(),
            GOLDEN_FULL_TEXT,
        );
    }

    /// SPEC R-PERM.3.4 "Context is required": nothing known is still seven
    /// sections, never a shorter body.
    #[test]
    fn golden_minimal_request_is_still_complete() {
        assert_golden(
            "minimal",
            &render(&minimal_request()).to_message(),
            GOLDEN_MINIMAL_MESSAGE,
        );
        assert_golden(
            "blank requester",
            &render(&blank_requester_request()).to_message(),
            GOLDEN_BLANK_REQUESTER_MESSAGE,
        );
    }

    #[test]
    fn golden_unknown_reason_offers_no_saved_tier() {
        assert_golden(
            "unknown reason",
            &render(&unknown_reason_request()).to_text(),
            GOLDEN_UNKNOWN_REASON_TEXT,
        );
    }

    /// The body inside a terminal hook's denial (the hook wraps it in its own
    /// first line and tier commands, pinned in `grant_channel.rs`). Only the
    /// first line of `details` becomes the evidence.
    #[test]
    fn golden_hook_body() {
        assert_golden("hook", &hook_body().to_message(), GOLDEN_HOOK_MESSAGE);
    }

    #[test]
    fn golden_tmp_question() {
        assert_golden("--tmp", &render(&tmp_request()).to_text(), GOLDEN_TMP_TEXT);
    }

    #[test]
    fn golden_log_target_question() {
        assert_golden(
            "log target",
            &render(&log_target_request()).to_text(),
            GOLDEN_LOG_TARGET_TEXT,
        );
    }

    /// SPEC R-PERM.3.4 "Only the TUI shows key letters": the text every
    /// non-TUI surface sends — the elicitation message, the hook body, the
    /// relayed tool result — names choices by value, never by a key nobody
    /// can press there. Checked for every key, every reason, both renderings.
    #[test]
    fn no_tui_key_letter_reaches_a_text_surface() {
        let bodies = [
            ("full", render(&full_request())),
            ("minimal", render(&minimal_request())),
            ("blank requester", render(&blank_requester_request())),
            ("unknown reason", render(&unknown_reason_request())),
            ("hook", hook_body()),
            ("--tmp", render(&tmp_request())),
            ("log target", render(&log_target_request())),
            ("harness refusal", render(&harness_refusal_request())),
        ];
        for (what, body) in &bodies {
            for text in [body.to_message(), body.to_text()] {
                for o in options() {
                    let key = format!("[{}]", o.key);
                    assert!(!text.contains(&key), "{what}: {key} leaked into:\n{text}");
                }
            }
        }
        // ...while the TUI still has a distinct key for every choice it binds.
        for (what, body) in &bodies {
            let mut keys: Vec<char> = body.options.iter().map(|o| o.key).collect();
            let n = keys.len();
            keys.sort_unstable();
            keys.dedup();
            assert_eq!(keys.len(), n, "{what}: duplicate TUI keys");
        }
    }

    // ------------------------------------------------------------ END PART 1 ---
}
