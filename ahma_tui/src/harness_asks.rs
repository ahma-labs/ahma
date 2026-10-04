//! Questions a refused terminal-hook command left for the harness's own
//! dialog, listed in the TUI so the human can answer them first (SPEC
//! R-PERM.10(e)).
//!
//! The records are `ahma_common::harness_asks`: owner-only files under the
//! runtime directory, one per workspace. The TUI reads them itself, every
//! [`POLL_EVERY`] on its existing tick, rather than through the hub: the hook
//! that records a refusal is a short-lived process with no hub connection to
//! relay through, and a read of a few small files (plus the settings file and
//! the session grants, only when a refusal is waiting) costs less than a
//! millisecond. A question joins the grant queue as a [`ScopeGrantGate`] built
//! with [`ScopeGrantGate::from_harness_ask`], so it waits its turn, renders
//! the one prompt body and honours the arming delay like any other (R-PERM.3.4,
//! R-PERM.3.5). The answer is applied here, the way the hook applies a yes
//! from the dialog, and recorded as answered so the dialog never asks it too.
//!
//! [`ScopeGrantGate`]: crate::state::ScopeGrantGate
//! [`ScopeGrantGate::from_harness_ask`]: crate::state::ScopeGrantGate::from_harness_ask

use std::path::{Path, PathBuf};
use std::time::Duration;

use ahma_common::config::{PersistentScope, ScopeAccess};
use ahma_common::harness_asks::{self, Question, WorkspaceAsks};
use ahma_common::permissions::{AuditAction, GrantKind, GrantTier};
use ahma_common::scope_grant::{
    GrantContext, GrantDecision, GrantEvidence, GrantReason, GrantRequester, ScopeGrantRequest,
};
use anyhow::{Context, Result};

/// How often the TUI re-reads the records.
pub const POLL_EVERY: Duration = Duration::from_secs(2);

/// One question waiting for a harness dialog, with what an answer binds to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessAsk {
    /// The workspace the refused command ran in; every grant is bound to it.
    pub workspace: PathBuf,
    pub question: Question,
    /// The live harness process that was refused. A session answer lasts as
    /// long as it does, like a yes in its own dialog.
    pub harness_pid: u32,
}

impl HarnessAsk {
    /// The same for the same question on every poll, so a repeat is
    /// recognised and a question that stopped waiting can be withdrawn.
    pub fn decision_id(&self) -> String {
        let key = format!(
            "{}\0{}\0{}\0{}",
            self.workspace.display(),
            self.question.dir.display(),
            self.question.access.short(),
            self.harness_pid
        );
        let digest = ahma_common::digest::sha256_hex(key.as_bytes());
        format!("harness-ask-{}", &digest[..16])
    }

    /// The question as the request every grant surface renders (SPEC
    /// R-PERM.3.4): the harness process asks, the refused paths are the
    /// evidence, the risk is judged against the workspace.
    pub fn request(&self) -> ScopeGrantRequest {
        let q = &self.question;
        let mut covers: Vec<String> = q
            .covers
            .iter()
            .take(4)
            .map(|p| p.display().to_string())
            .collect();
        if q.covers.len() > 4 {
            covers.push(format!("and {} more", q.covers.len() - 4));
        }
        ScopeGrantRequest {
            decision_id: self.decision_id(),
            path: q.dir.clone(),
            access: q.access,
            reason: GrantReason::HarnessRefusal,
            tool: Some("an earlier hooked command".into()),
            context: GrantContext {
                requester: Some(GrantRequester {
                    client: Some("terminal hook".into()),
                    session_id: None,
                    workspace: Some(self.workspace.clone()),
                    pid: self.harness_pid,
                }),
                evidence: Some(GrantEvidence {
                    raw_path: q.covers.first().cloned(),
                    pattern: None,
                    line: Some(format!("refused: {}", covers.join(", "))),
                }),
                risk: Some(ahma_mcp::sandbox::grant_channel::risk_summary(
                    &q.dir,
                    std::slice::from_ref(&self.workspace),
                )),
                write_denied: q.access.is_write(),
                ..GrantContext::default()
            },
        }
    }
}

/// Every question waiting on this machine for a live harness.
pub fn pending() -> Vec<HarnessAsk> {
    let Some(dir) = harness_asks::default_dir() else {
        return Vec::new();
    };
    let now = ahma_common::session_grants::now_secs();
    let all = harness_asks::load_all(&dir, now);
    // Nothing refused anywhere is the usual case: read nothing else.
    if all.iter().all(|a| a.refusals.is_empty()) {
        return Vec::new();
    }
    let persistent: Vec<PersistentScope> = ahma_common::config::AhmaSettings::load()
        .sandbox
        .persistent_scopes
        .into_iter()
        .filter(|p| p.applies_at(now))
        .collect();
    let granted = |workspace: &Path| {
        let mut g = persistent.clone();
        g.extend(ahma_mcp::sandbox::session_scopes_for(&[
            workspace.to_path_buf()
        ]));
        g
    };
    let home = ahma_common::config::ahma_home_dir().map(|h| dunce::canonicalize(&h).unwrap_or(h));
    pending_from(
        &all,
        home.as_deref(),
        &granted,
        &ahma_mcp::sandbox::session_tier::pid_alive,
    )
}

/// The questions waiting in `all`: what no grant covers (`granted`, per
/// workspace), no dialog has asked and no surface has answered
/// ([`harness_asks::pending_questions`]), each bound to the most recently
/// refused harness process that is still `alive`. A question whose harness has
/// exited is not listed: nothing could bound a session answer, and the next
/// session that is refused records it again.
pub fn pending_from(
    all: &[WorkspaceAsks],
    home: Option<&Path>,
    granted: &dyn Fn(&Path) -> Vec<PersistentScope>,
    alive: &dyn Fn(u32) -> bool,
) -> Vec<HarnessAsk> {
    let mut out = Vec::new();
    for asks in all.iter().filter(|a| !a.refusals.is_empty()) {
        let workspace = &asks.workspace;
        let grants = granted(workspace);
        let covered =
            |p: &Path, a: ScopeAccess| harness_asks::grant_covers(&grants, workspace, p, a);
        let scopes = std::slice::from_ref(workspace);
        for question in harness_asks::pending_questions(asks, &covered, home, scopes) {
            let harness_pid = asks
                .refusals
                .iter()
                .filter(|r| question.covers.contains(&r.path))
                .filter_map(|r| Some((r.at, r.harness_pid?)))
                .filter(|(_, pid)| alive(*pid))
                .max()
                .map(|(_, pid)| pid);
            if let Some(harness_pid) = harness_pid {
                out.push(HarnessAsk {
                    workspace: workspace.clone(),
                    question,
                    harness_pid,
                });
            }
        }
    }
    out
}

/// Apply the human's `decision` on `ask` and record it as answered, so the
/// harness dialog does not ask it too. Returns the line for the TUI's log.
pub fn answer(ask: &HarnessAsk, decision: GrantDecision) -> Result<String> {
    let dir = harness_asks::default_dir().context("ahma's runtime directory is unknown")?;
    answer_in(
        &dir,
        ahma_common::config::settings_path().as_deref(),
        ask,
        decision,
        ahma_common::session_grants::now_secs(),
    )
}

/// [`answer`] against the records in `dir` and the settings file
/// `settings_file`.
///
/// Deny records only the answer. A session answer records a session grant
/// bound to the harness process and the workspace, as a yes in the dialog
/// does (SPEC R-PERM.4.4). A 24-hour or `always` answer is written through
/// the one audited chokepoint, `persist_grant`, bound to the workspace
/// (R-PERM.2). An answer the question does not offer is narrowed first
/// (`within_offer`: a once answer is a deny). The denylist refuses what it
/// refuses, and then nothing is recorded, so the question stays.
pub fn answer_in(
    dir: &Path,
    settings_file: Option<&Path>,
    ask: &HarnessAsk,
    decision: GrantDecision,
    now: u64,
) -> Result<String> {
    let decision = decision.within_offer(GrantReason::HarnessRefusal);
    let q = &ask.question;
    let path = q.dir.display().to_string();
    let message = match decision.access() {
        None => {
            audit(ask, AuditAction::Deny, None, GrantTier::Session);
            format!(
                "Denied sandbox access to {path}; the agent's harness will not ask about it \
                 again this session"
            )
        }
        Some(access) => {
            if let Some(why) = ahma_common::scope_grant::refusal_reason(&q.dir) {
                anyhow::bail!("{path} cannot be granted ({why})");
            }
            let tier = decision.tier();
            let verb = if access.is_write() {
                "read+write"
            } else {
                "read-only"
            };
            if tier.is_persistent() {
                let file = settings_file
                    .context("cannot save the grant: ahma's settings file is unknown")?;
                ahma_common::scope_grant::persist_grant(
                    file,
                    ahma_common::scope_grant::NewGrant {
                        path: &q.dir,
                        access,
                        granted_by: Some("terminal hook refusal".into()),
                        granted_at: Some(chrono::Local::now().format("%Y-%m-%d").to_string()),
                        note: None,
                        surface: "tui",
                        live_scopes: std::slice::from_ref(&ask.workspace),
                        workspace: Some(&ask.workspace),
                        expires_at: (tier == GrantTier::Lease)
                            .then_some(now + ahma_common::scope_grant::PROMPT_LEASE_SECS),
                    },
                )?;
                if tier == GrantTier::Lease {
                    format!(
                        "Granted {verb} access to {path} for 24 hours in {} — saved, and ends on \
                         its own; the agent's next command has it",
                        ask.workspace.display()
                    )
                } else {
                    format!(
                        "Granted {verb} access to {path} for {} — saved to ~/.ahma/settings.toml; \
                         the agent's next command has it",
                        ask.workspace.display()
                    )
                }
            } else {
                ahma_mcp::sandbox::record_session_grant(
                    &q.dir,
                    access,
                    Some(&ask.workspace),
                    ask.harness_pid,
                    "tui",
                );
                audit(ask, AuditAction::Grant, Some(access), GrantTier::Session);
                format!(
                    "Granted {verb} access to {path} until the agent's harness exits (not saved); \
                     its next command has it"
                )
            }
        }
    };
    harness_asks::mark_answered(dir, &ask.workspace, q, Some(ask.harness_pid), now)?;
    Ok(message)
}

/// One audit line for an answer that `persist_grant` does not write itself.
fn audit(ask: &HarnessAsk, action: AuditAction, access: Option<ScopeAccess>, tier: GrantTier) {
    ahma_common::permissions::append_audit(
        &ahma_common::permissions::audit_entry(
            chrono::Local::now().to_rfc3339(),
            action,
            GrantKind::FsScope,
            ask.question.dir.display().to_string(),
            access.map(|a| a.short().to_string()),
            tier,
            Some("tui".to_string()),
        )
        .with_request(ask.decision_id(), None, None),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ahma_common::harness_asks::Refusal;

    struct Fixture {
        _root: tempfile::TempDir,
        dir: PathBuf,
        ws: PathBuf,
        cache: PathBuf,
    }

    /// A workspace whose hooked command was refused a write in a tool cache
    /// outside it, by this (live) process standing in for the harness.
    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("asks");
        let ws = root.path().join("ws");
        let cache = root.path().join("tool-cache");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let ws = dunce::canonicalize(&ws).unwrap();
        let cache = dunce::canonicalize(&cache).unwrap();
        harness_asks::record_refusal(
            &dir,
            &ws,
            Refusal {
                path: cache.join("heavy.lock.holder"),
                grant_dir: cache.clone(),
                access: ScopeAccess::Rw,
                at: 100,
                harness_pid: Some(std::process::id()),
                command_digest: None,
            },
        )
        .unwrap();
        Fixture {
            _root: root,
            dir,
            ws,
            cache,
        }
    }

    fn waiting(f: &Fixture, granted: &dyn Fn(&Path) -> Vec<PersistentScope>) -> Vec<HarnessAsk> {
        pending_from(
            &harness_asks::load_all(&f.dir, 200),
            None,
            granted,
            &|pid: u32| pid == std::process::id(),
        )
    }

    fn nothing(_: &Path) -> Vec<PersistentScope> {
        Vec::new()
    }

    #[test]
    fn a_refusal_waits_until_answered_and_its_id_is_stable() {
        let f = fixture();
        let asks = waiting(&f, &nothing);
        assert_eq!(asks.len(), 1, "{asks:?}");
        let ask = &asks[0];
        assert_eq!(ask.question.dir, f.cache);
        assert_eq!(ask.harness_pid, std::process::id());
        assert_eq!(ask.decision_id(), waiting(&f, &nothing)[0].decision_id());

        let req = ask.request();
        assert_eq!(req.reason, GrantReason::HarnessRefusal);
        assert_eq!(req.path, f.cache);
        assert!(req.context.write_denied);
        let body = ahma_common::grant_prompt::render(&req).to_message();
        assert!(body.contains("heavy.lock.holder"), "{body}");
        assert!(body.contains("terminal hook"), "{body}");
    }

    #[test]
    fn a_question_whose_harness_exited_is_not_listed() {
        let f = fixture();
        let none = pending_from(
            &harness_asks::load_all(&f.dir, 200),
            None,
            &nothing,
            &|_: u32| false,
        );
        assert!(none.is_empty(), "{none:?}");
    }

    #[test]
    fn a_covered_refusal_is_not_listed() {
        let f = fixture();
        let cache = f.cache.clone();
        let granted = move |_: &Path| {
            vec![PersistentScope {
                path: cache.clone(),
                access: ScopeAccess::Rw,
                workspace: None,
                granted_by: None,
                granted_at: None,
                note: None,
                expires_at: None,
            }]
        };
        assert!(waiting(&f, &granted).is_empty());
    }

    /// A deny is remembered: the TUI stops listing it and the harness dialog
    /// does not ask it (SPEC R-PERM.10(e)).
    #[test]
    fn a_deny_is_recorded_so_the_dialog_does_not_ask() {
        let f = fixture();
        let ask = waiting(&f, &nothing).remove(0);
        let line = answer_in(&f.dir, None, &ask, GrantDecision::Deny, 201).unwrap();
        assert!(line.starts_with("Denied"), "{line}");
        assert!(waiting(&f, &nothing).is_empty());
        let asks = harness_asks::load(&f.dir, &f.ws, 202);
        let none = |_: &Path, _: ScopeAccess| false;
        assert!(
            harness_asks::next_question(&asks, "s1", &none, None, std::slice::from_ref(&f.ws))
                .is_none()
        );
        assert!(
            !ahma_mcp::sandbox::session_scopes_for(std::slice::from_ref(&f.ws))
                .iter()
                .any(|g| g.path == f.cache),
            "a deny grants nothing"
        );
    }

    /// A session answer is the same session grant a yes in the dialog makes:
    /// bound to the harness process and the workspace (SPEC R-PERM.4.4).
    #[test]
    fn a_session_answer_is_a_session_grant_for_the_harness() {
        let f = fixture();
        let ask = waiting(&f, &nothing).remove(0);
        let line = answer_in(&f.dir, None, &ask, GrantDecision::GrantRwSession, 201).unwrap();
        assert!(line.contains("until the agent's harness exits"), "{line}");
        let granted = ahma_mcp::sandbox::session_scopes_for(std::slice::from_ref(&f.ws));
        assert!(
            granted
                .iter()
                .any(|g| g.path == f.cache && g.access.is_write()),
            "{granted:?}"
        );
        assert!(waiting(&f, &nothing).is_empty(), "answered");
    }

    /// `always` goes through `persist_grant`, bound to the workspace.
    #[test]
    fn an_always_answer_is_saved_for_the_workspace() {
        let f = fixture();
        let settings = f.dir.parent().unwrap().join("settings.toml");
        let ask = waiting(&f, &nothing).remove(0);
        answer_in(&f.dir, Some(&settings), &ask, GrantDecision::GrantRo, 201).unwrap();
        let saved = ahma_common::config::AhmaSettings::load_from_result(&settings).unwrap();
        let scope = saved
            .sandbox
            .persistent_scopes
            .iter()
            .find(|s| s.path == f.cache)
            .expect("saved");
        assert_eq!(scope.access, ScopeAccess::Ro);
        assert_eq!(scope.workspace.as_deref(), Some(f.ws.as_path()));
        assert!(scope.expires_at.is_none());
        assert!(waiting(&f, &nothing).is_empty(), "answered");
    }

    /// A once answer cannot be honoured here and is never widened: it is a deny.
    #[test]
    fn a_once_answer_is_a_deny() {
        let f = fixture();
        let ask = waiting(&f, &nothing).remove(0);
        let line = answer_in(&f.dir, None, &ask, GrantDecision::GrantRwOnce, 201).unwrap();
        assert!(line.starts_with("Denied"), "{line}");
    }
}
