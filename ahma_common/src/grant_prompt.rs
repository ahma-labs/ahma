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
    let mut blocked = if startup {
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
        GrantReason::StartupFlag => blocked.push_str(
            "\nthe path is exact: this machine's temp directory, shared by every program you \
             run, and part of no project",
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
    let allows = if saved_tiers {
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
    let always = if saved_tiers {
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
/// option for a blocked path, and only deny / once / session for a question
/// whose answer must not be saved ([`GrantReason::offers_saved_tiers`] — the
/// `--tmp` question). Every surface builds its choices from this, so the form,
/// the TUI keys and the text body agree; [`crate::scope_grant::GrantCoordinator::resolve`]
/// holds any answer outside it to the session.
pub fn options_for(reason: GrantReason) -> Vec<PromptOption> {
    options()
        .into_iter()
        // Deny is always offered (it is the default, R5.3.1); a reason without
        // saved tiers drops only the grants that would persist.
        .filter(|o| {
            reason.offers_saved_tiers()
                || o.decision.access().is_none()
                || !o.decision.tier().is_persistent()
        })
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
}
