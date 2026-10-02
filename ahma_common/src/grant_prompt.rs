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
    /// Plain text for a terminal or an elicitation message.
    pub fn to_text(&self) -> String {
        let mut out = format!("{}\n", self.title);
        for s in &self.sections {
            out.push_str(&format!(
                "\n{}\n  {}\n",
                s.heading,
                s.body.replace('\n', "\n  ")
            ));
        }
        out.push_str("\nChoices (deny is the default):\n");
        for o in &self.options {
            out.push_str(&format!("  [{}] {:<20} {}\n", o.key, o.value, o.label));
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
    let minimum_access = if ctx.write_denied {
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
    let mut blocked = format!(
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
    );
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
        if ctx.write_denied {
            " (a write was refused, so read-only would not fix it)"
        } else {
            " (no write was refused; read-only is enough until one is)"
        }
    );

    // 5. What a grant allows
    let allows = format!(
        "every command in {} may {} {} — for the next command (once), until this session \
         ends (session), or until you revoke it (always). Nothing else outside the workspace \
         changes.",
        workspace
            .clone()
            .unwrap_or_else(|| "this workspace".to_string()),
        if minimum_access.is_write() {
            "read and write"
        } else {
            "read"
        },
        path
    );

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

    // 7. The exact line `always` writes
    let always = format!(
        "~/.ahma/settings.toml gets:\n[[sandbox.persistent_scopes]]\npath = \"{}\"\naccess = \"{}\"\nworkspace = \"{}\"\nRevoke any time with: ahma sandbox revoke {}",
        path,
        minimum_access.short(),
        workspace
            .clone()
            .unwrap_or_else(|| "<this workspace>".to_string()),
        path
    );

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
        options: options(),
    }
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
        _ => GrantDecision::Deny,
    }
}

/// The body for a denial a terminal hook reports, where no coordinator request
/// exists: the same sections, built from what the hook knows.
pub fn render_for_hook(path: &Path, access: ScopeAccess, details: &str) -> PromptBody {
    let req = ScopeGrantRequest {
        decision_id: String::new(),
        path: path.to_path_buf(),
        access,
        reason: GrantReason::StderrHeuristic,
        tool: Some("a hooked shell command".into()),
        context: crate::scope_grant::GrantContext {
            evidence: Some(crate::scope_grant::GrantEvidence {
                raw_path: None,
                pattern: None,
                line: Some(details.lines().next().unwrap_or("").to_string()),
            }),
            write_denied: access.is_write(),
            ..Default::default()
        },
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
}
