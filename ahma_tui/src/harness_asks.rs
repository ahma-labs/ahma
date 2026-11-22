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
//! A link a hooked command was refused (SPEC R-LINK.6) is a question of its
//! own, a [`HarnessLinkAsk`], shown in the link question
//! ([`LinkAskGate`]) with the answers `links::policy` offers this surface:
//! deny, this site for this session, and the lasting ones; `ahma web` and
//! `ahma desktop`, which may not save, give only the first two
//! ([`HarnessLinkAsk::unsaved_choices`]). Its session answer
//! is a session grant file bound to the harness process
//! (`ahma_common::open_url::record_session`), which the hook reads as it
//! reads a saved grant.
//!
//! [`ScopeGrantGate`]: crate::state::ScopeGrantGate
//! [`ScopeGrantGate::from_harness_ask`]: crate::state::ScopeGrantGate::from_harness_ask
//! [`LinkAskGate`]: crate::state::LinkAskGate

use std::path::{Path, PathBuf};
use std::time::Duration;

use ahma_common::approver::ApproverSurface;
use ahma_common::config::ScopeAccess;
use ahma_common::grant::ResolvedGrant;
use ahma_common::harness_asks::LinkRefusal;
use ahma_common::harness_asks::{self, Question, WorkspaceAsks};
use ahma_common::open_url::{OpenUrlBinding, OpenUrlSessionGrant};
use ahma_common::permissions::{AuditAction, GrantKind, GrantTier};
use ahma_common::scope_grant::{
    GrantContext, GrantDecision, GrantEvidence, GrantReason, GrantRequester, ScopeGrantRequest,
};
use ahma_mcp::links::policy::{self as link_policy, Effect, LinkChoice, LinkGrants, LinkQuestion};
use anyhow::{Context, Result};

/// How often the TUI re-reads the records.
pub const POLL_EVERY: Duration = Duration::from_secs(2);

/// What every surface says under a hooked command's link question (SPEC
/// R-LINK.6): nothing waits on the refused command, so an answer applies when
/// it runs again, and settles the question for the harness dialog.
pub const LINK_NOT_PAUSED: &str = "The command was not paused: an answer here applies when the \
     agent runs it again, and Claude Code's dialog does not ask it too.";

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
    let persistent: Vec<ResolvedGrant> = ahma_common::config::AhmaSettings::load()
        .sandbox
        .persistent_scopes
        .iter()
        .filter(|p| p.applies_at(now))
        .map(ResolvedGrant::saved)
        .collect();
    let granted = |workspace: &Path| {
        let mut g = persistent.clone();
        g.extend(ahma_mcp::sandbox::session_grants_for(&[
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
    granted: &dyn Fn(&Path) -> Vec<ResolvedGrant>,
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
/// `surface` is where the human answered (`ahma tui`, `ahma web`, `ahma
/// desktop`): what the audit and the grant record (SPEC R-GUI.7).
pub fn answer(
    ask: &HarnessAsk,
    decision: GrantDecision,
    surface: &ApproverSurface,
) -> Result<String> {
    let dir = harness_asks::default_dir().context("ahma's runtime directory is unknown")?;
    answer_in(
        &dir,
        ahma_common::config::settings_path().as_deref(),
        ask,
        decision,
        surface,
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
    surface: &ApproverSurface,
    now: u64,
) -> Result<String> {
    let decision = decision.within_offer(GrantReason::HarnessRefusal);
    let q = &ask.question;
    let path = q.dir.display().to_string();
    let message = match decision.access() {
        None => {
            audit(ask, AuditAction::Deny, None, GrantTier::Session, surface);
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
                        granted_at: Some(ahma_common::timestamp::local_date_today()),
                        note: None,
                        surface,
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
                    surface.into(),
                );
                audit(
                    ask,
                    AuditAction::Grant,
                    Some(access),
                    GrantTier::Session,
                    surface,
                );
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
fn audit(
    ask: &HarnessAsk,
    action: AuditAction,
    access: Option<ScopeAccess>,
    tier: GrantTier,
    surface: &ApproverSurface,
) {
    ahma_common::permissions::append_audit(
        &ahma_common::permissions::AuditEntry::new(
            ahma_common::timestamp::rfc3339_local_now(),
            action,
            GrantKind::FsScope,
            ask.question.dir.display().to_string(),
            tier,
        )
        .with_access_opt(access.map(|a| a.short().to_string()))
        .with_surface(surface)
        .with_request(ask.decision_id(), None, None),
    );
}

/// A link a hooked command was refused, waiting for a harness dialog (SPEC
/// R-LINK.6), with what an answer binds to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessLinkAsk {
    /// The repo the refused command ran in: a "this repo only" answer and a
    /// session answer are bound to it.
    pub workspace: PathBuf,
    pub refusal: LinkRefusal,
    /// The live harness process that was refused. A session answer lasts as
    /// long as it does.
    pub harness_pid: u32,
}

impl HarnessLinkAsk {
    /// The same for the same question on every poll, so a repeat is
    /// recognised and a question that stopped waiting can be withdrawn.
    pub fn decision_id(&self) -> String {
        let key = format!(
            "{}\0{}\0{}",
            self.workspace.display(),
            self.refusal.host.to_ascii_lowercase(),
            self.harness_pid
        );
        let digest = ahma_common::digest::sha256_hex(key.as_bytes());
        format!("harness-link-{}", &digest[..16])
    }

    /// The question, worded as every surface words it (SPEC R-LINK.7).
    pub fn question(&self) -> LinkQuestion {
        LinkQuestion::from_refusal(&self.refusal)
    }

    /// What the TUI offers for it, deny first (SPEC R-LINK.6): nothing waits
    /// on a hooked command, so no "this time".
    pub fn choices(&self) -> Vec<LinkChoice> {
        self.question().choices(link_policy::Surface::TuiForHarness)
    }

    /// What a surface that may not save (SPEC R-GUI.9: `ahma web`, `ahma
    /// desktop`) can give for it, deny first: the answers it offers
    /// ([`Self::choices`]) that `links::policy` also offers such a surface
    /// (`Surface::Browser`) — deny and this site for this session. The
    /// browser's "this time" is for a link a command waits on, and nothing
    /// waits on a hooked command (R-LINK.6).
    pub fn unsaved_choices(&self) -> Vec<LinkChoice> {
        let browser = self.question().choices(link_policy::Surface::Browser);
        self.choices()
            .into_iter()
            .filter(|c| browser.contains(c))
            .collect()
    }

    /// The `ahma permissions grant open-url` line that gives a saving
    /// `choice` (SPEC R-LINK.11): where a surface that may not save says that
    /// answer is given. `None` for one that saves nothing.
    pub fn grant_command(&self, choice: LinkChoice) -> Option<String> {
        link_policy::grant_command(choice, &self.refusal.host, &self.workspace)
    }

    /// What a saving `choice` writes, for the step that shows it before it is
    /// taken (SPEC R-PERM.3.5); `None` for one that saves nothing.
    pub fn save_line(&self, choice: LinkChoice) -> Option<String> {
        let Effect::Persist(pattern, binding) =
            link_policy::effect(choice, &self.refusal.host, &self.workspace)
        else {
            return None;
        };
        Some(match binding {
            OpenUrlBinding::AllRepos => {
                format!(
                    "adds \"{pattern}\" to [open_url] allow (all repos) in ~/.ahma/settings.toml"
                )
            }
            OpenUrlBinding::Repo(repo) => format!(
                "adds \"{pattern}\" to [open_url] repos for {} (this repo only) in \
                 ~/.ahma/settings.toml",
                repo.display()
            ),
        })
    }
}

/// Every refused link waiting on this machine for a live harness.
pub fn pending_links() -> Vec<HarnessLinkAsk> {
    let Some(dir) = harness_asks::default_dir() else {
        return Vec::new();
    };
    let now = ahma_common::session_grants::now_secs();
    let all = harness_asks::load_all(&dir, now);
    // Nothing refused anywhere is the usual case: read nothing else.
    if all.iter().all(|a| a.link_refusals.is_empty()) {
        return Vec::new();
    }
    let settings = ahma_common::config::settings_path();
    let granted = |workspace: &Path| {
        ahma_mcp::links::consent::hook_grants(settings.as_deref(), workspace, Vec::new())
    };
    pending_links_from(&all, &granted, &ahma_mcp::sandbox::session_tier::pid_alive)
}

/// The refused links waiting in `all`: what no saved or session grant covers
/// (`granted`, per workspace, read as the hook reads it), not denied for its
/// harness and not being asked in its dialog
/// ([`harness_asks::pending_link_refusals`]), whose harness process is still
/// `alive`. A refusal whose harness has exited is not listed: nothing could
/// bound a session answer.
pub fn pending_links_from(
    all: &[WorkspaceAsks],
    granted: &dyn Fn(&Path) -> LinkGrants,
    alive: &dyn Fn(u32) -> bool,
) -> Vec<HarnessLinkAsk> {
    let mut out = Vec::new();
    for asks in all.iter().filter(|a| !a.link_refusals.is_empty()) {
        let grants = granted(&asks.workspace);
        let covered = |r: &LinkRefusal| grants.covers(&r.host, r.local, r.outside_egress);
        for refusal in harness_asks::pending_link_refusals(asks, &covered) {
            let Some(harness_pid) = refusal.harness_pid.filter(|pid| alive(*pid)) else {
                continue;
            };
            out.push(HarnessLinkAsk {
                workspace: asks.workspace.clone(),
                refusal,
                harness_pid,
            });
        }
    }
    out
}

/// Apply the human's `choice` on `ask` and settle the refusal, so the harness
/// dialog does not ask it too. Returns the line for the TUI's log. `surface`
/// is where the human answered: what the audit and the grant record (SPEC
/// R-GUI.7, R-LINK.9).
pub fn answer_link(
    ask: &HarnessLinkAsk,
    choice: LinkChoice,
    surface: &ApproverSurface,
) -> Result<String> {
    let dir = harness_asks::default_dir().context("ahma's runtime directory is unknown")?;
    let sessions =
        ahma_common::open_url::session_dir().context("ahma's runtime directory is unknown")?;
    answer_link_in(
        &dir,
        &sessions,
        ahma_common::config::settings_path().as_deref(),
        ask,
        choice,
        surface,
        ahma_common::session_grants::now_secs(),
    )
}

/// [`answer_link`] against the records in `dir`, the session grants in
/// `sessions` and the settings file `settings_file`.
///
/// An answer the question does not offer here is a deny: never widened. Deny
/// is remembered for the harness's session (SPEC R-LINK.6). "This site, this
/// session" is a session grant bound to the harness process and the repo
/// (R-LINK.4). A lasting answer is written by the one audited writer,
/// `persist_open_url_allow` (R-LINK.9). The refusal is settled only once its
/// answer took, so a grant that could not be written leaves the question.
pub fn answer_link_in(
    dir: &Path,
    sessions: &Path,
    settings_file: Option<&Path>,
    ask: &HarnessLinkAsk,
    choice: LinkChoice,
    surface: &ApproverSurface,
    now: u64,
) -> Result<String> {
    let host = ask.refusal.host.to_ascii_lowercase();
    let choice = if ask.choices().contains(&choice) {
        choice
    } else {
        LinkChoice::Deny
    };
    let (declined, message) = match link_policy::effect(choice, &host, &ask.workspace) {
        Effect::Refuse { .. } | Effect::OpenOnce => {
            ahma_common::permissions::append_audit(
                &ahma_common::permissions::AuditEntry::new(
                    ahma_common::timestamp::rfc3339_local_now(),
                    AuditAction::Deny,
                    GrantKind::OpenUrl,
                    host.clone(),
                    GrantTier::Session,
                )
                .with_surface(surface)
                .with_request(ask.decision_id(), None, None),
            );
            (
                true,
                format!(
                    "Denied opening {host} links; the agent's harness will not ask about it again \
                     this session"
                ),
            )
        }
        Effect::Session(_) => {
            ahma_common::open_url::record_session(
                sessions,
                &OpenUrlSessionGrant {
                    site: host.clone(),
                    workspace: ask.workspace.clone(),
                    owner_pid: ask.harness_pid,
                    granted_at: now,
                    granted_by: surface.into(),
                },
                surface,
            )?;
            (
                false,
                format!(
                    "Allowed {host} links for commands in {} until the agent's harness exits (not \
                     saved); re-run the command to open it",
                    ask.workspace.display()
                ),
            )
        }
        Effect::Persist(pattern, binding) => {
            let file =
                settings_file.context("cannot save the grant: ahma's settings file is unknown")?;
            ahma_common::open_url::persist_open_url_allow(
                file,
                &pattern.to_string(),
                &binding,
                surface,
            )?;
            (
                false,
                format!(
                    "Allowed {pattern} links for {} — saved to ~/.ahma/settings.toml (undo with \
                     `ahma permissions revoke open-url`); re-run the command to open it",
                    binding.describe()
                ),
            )
        }
    };
    harness_asks::answer_link(
        dir,
        &ask.workspace,
        &host,
        Some(ask.harness_pid),
        declined,
        now,
    )?;
    Ok(message)
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
                programs: Vec::new(),
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

    fn waiting(f: &Fixture, granted: &dyn Fn(&Path) -> Vec<ResolvedGrant>) -> Vec<HarnessAsk> {
        pending_from(
            &harness_asks::load_all(&f.dir, 200),
            None,
            granted,
            &|pid: u32| pid == std::process::id(),
        )
    }

    fn nothing(_: &Path) -> Vec<ResolvedGrant> {
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
            vec![ResolvedGrant::saved(
                &ahma_common::config::PersistentScope {
                    path: cache.clone(),
                    access: ScopeAccess::Rw,
                    workspace: None,
                    granted_by: None,
                    granted_at: None,
                    note: None,
                    expires_at: None,
                },
            )]
        };
        assert!(waiting(&f, &granted).is_empty());
    }

    /// A deny is remembered: the TUI stops listing it and the harness dialog
    /// does not ask it (SPEC R-PERM.10(e)).
    #[test]
    fn a_deny_is_recorded_so_the_dialog_does_not_ask() {
        let f = fixture();
        let ask = waiting(&f, &nothing).remove(0);
        let line = answer_in(
            &f.dir,
            None,
            &ask,
            GrantDecision::Deny,
            &ApproverSurface::Tui,
            201,
        )
        .unwrap();
        assert!(line.starts_with("Denied"), "{line}");
        assert!(waiting(&f, &nothing).is_empty());
        let asks = harness_asks::load(&f.dir, &f.ws, 202);
        let none = |_: &Path, _: ScopeAccess| false;
        assert!(
            harness_asks::next_question(&asks, "s1", &none, None, std::slice::from_ref(&f.ws))
                .is_none()
        );
        assert!(
            !ahma_mcp::sandbox::session_grants_for(std::slice::from_ref(&f.ws))
                .iter()
                .any(|g| g.path == f.cache),
            "a deny grants nothing"
        );
    }

    /// R-GUI.7: an answer given in the browser is recorded under `web`, not
    /// `tui`. The browser polls these questions too, and a session or deny
    /// answer there was recorded as the terminal's.
    #[test]
    fn a_session_answer_from_the_browser_is_recorded_as_web() {
        let f = fixture();
        let ask = waiting(&f, &nothing).remove(0);
        answer_in(
            &f.dir,
            None,
            &ask,
            GrantDecision::GrantRwSession,
            &ApproverSurface::Web,
            201,
        )
        .unwrap();
        let granted = ahma_mcp::sandbox::session_grants_for(std::slice::from_ref(&f.ws));
        let g = granted
            .iter()
            .find(|g| g.path == f.cache)
            .expect("session grant");
        assert_eq!(g.by.label().as_deref(), Some("web"), "{granted:?}");
    }

    /// A session answer is the same session grant a yes in the dialog makes:
    /// bound to the harness process and the workspace (SPEC R-PERM.4.4).
    #[test]
    fn a_session_answer_is_a_session_grant_for_the_harness() {
        let f = fixture();
        let ask = waiting(&f, &nothing).remove(0);
        let line = answer_in(
            &f.dir,
            None,
            &ask,
            GrantDecision::GrantRwSession,
            &ApproverSurface::Tui,
            201,
        )
        .unwrap();
        assert!(line.contains("until the agent's harness exits"), "{line}");
        let granted = ahma_mcp::sandbox::session_grants_for(std::slice::from_ref(&f.ws));
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
        answer_in(
            &f.dir,
            Some(&settings),
            &ask,
            GrantDecision::GrantRo,
            &ApproverSurface::Tui,
            201,
        )
        .unwrap();
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
        let line = answer_in(
            &f.dir,
            None,
            &ask,
            GrantDecision::GrantRwOnce,
            &ApproverSurface::Tui,
            201,
        )
        .unwrap();
        assert!(line.starts_with("Denied"), "{line}");
    }

    mod links {
        use super::*;
        use ahma_common::harness_asks::{CommandDigest, LinkRefusal};
        use ahma_mcp::links::policy::LinkChoice::*;

        struct LinkFixture {
            home: tempfile::TempDir,
            dir: PathBuf,
            sessions: PathBuf,
            settings: PathBuf,
            ws: PathBuf,
        }

        const CMD: &str = "gh pr view 12 --web";

        /// A hooked command in `ws`, run by this (live) process standing in for
        /// the harness, was refused a link to `host`. Audit and grants go to a
        /// private test home.
        fn link_fixture(host: &str, local: bool) -> LinkFixture {
            let home = tempfile::tempdir().unwrap();
            // SAFETY: nextest runs each test in its own process.
            unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
            let dir = home.path().join("asks");
            let ws = home.path().join("ws");
            harness_asks::record_link_refusal(
                &dir,
                &ws,
                LinkRefusal {
                    host: host.into(),
                    shown: format!("http://{host}/o/r/pull/12"),
                    local,
                    outside_egress: false,
                    at: 100,
                    harness_pid: Some(std::process::id()),
                    command_digest: Some(CommandDigest::of(CMD)),
                },
            )
            .unwrap();
            LinkFixture {
                sessions: home.path().join("open-url-sessions"),
                settings: home.path().join(".ahma").join("settings.toml"),
                dir,
                ws,
                home,
            }
        }

        fn no_grants(_: &Path) -> LinkGrants {
            LinkGrants::default()
        }

        fn waiting(f: &LinkFixture) -> Vec<HarnessLinkAsk> {
            pending_links_from(
                &harness_asks::load_all(&f.dir, 200),
                &no_grants,
                &|pid: u32| pid == std::process::id(),
            )
        }

        fn answer(f: &LinkFixture, choice: LinkChoice) -> String {
            let ask = waiting(f).remove(0);
            answer_link_in(
                &f.dir,
                &f.sessions,
                Some(&f.settings),
                &ask,
                choice,
                &ApproverSurface::Tui,
                201,
            )
            .unwrap()
        }

        /// The harness dialog would ask about it next: what a TUI answer must
        /// settle (SPEC R-PERM.3.8).
        fn dialog_asks(f: &LinkFixture) -> bool {
            let asks = harness_asks::load(&f.dir, &f.ws, 202);
            harness_asks::next_link_question(&asks, "s1", CMD, &|_| None, &|_| false).is_some()
        }

        fn audited(f: &LinkFixture) -> Vec<(AuditAction, GrantTier, Option<String>)> {
            ahma_common::permissions::read_audit_entries(
                &f.home.path().join(".ahma").join("permissions-audit.jsonl"),
            )
            .into_iter()
            .filter(|e| e.kind == GrantKind::OpenUrl)
            .map(|e| (e.action, e.tier, e.surface))
            .collect()
        }

        /// SPEC R-LINK.6: a hooked command's refused link is listed while its
        /// harness lives and no grant covers the site, with the question every
        /// surface shows (ending with the guidance, R-LINK.7) and this
        /// surface's answers, deny first: this site for this session, then
        /// always for this site in all repos before this repo, then any site
        /// only where it would cover the link.
        #[test]
        fn a_hooked_link_refusal_is_listed_with_session_and_always() {
            let f = link_fixture("github.com", false);
            let asks = waiting(&f);
            assert_eq!(asks.len(), 1, "{asks:?}");
            let ask = &asks[0];
            assert_eq!(ask.harness_pid, std::process::id());
            assert_eq!(ask.decision_id(), waiting(&f)[0].decision_id(), "stable");
            assert_eq!(
                ask.choices(),
                [
                    Deny,
                    Session,
                    AlwaysSiteAllRepos,
                    AlwaysSiteThisRepo,
                    AlwaysAny
                ]
            );
            let text = ask.question().text();
            assert!(text.contains("Site: github.com"), "{text}");
            assert!(text.ends_with(ahma_common::open_url::GUIDANCE), "{text}");
            assert!(ask.save_line(Session).is_none(), "nothing saved");
            assert!(
                ask.save_line(AlwaysSiteThisRepo)
                    .unwrap()
                    .contains(&f.ws.display().to_string())
            );

            let local = link_fixture("localhost", true);
            assert_eq!(
                waiting(&local)[0].choices(),
                [Deny, Session, AlwaysSiteAllRepos, AlwaysSiteThisRepo],
                "no any-site answer for a local site"
            );

            let gone = pending_links_from(
                &harness_asks::load_all(&f.dir, 200),
                &no_grants,
                &|_: u32| false,
            );
            assert!(gone.is_empty(), "its harness exited");
            let any = |_: &Path| LinkGrants {
                always: vec![ahma_mcp::egress::HostPattern::Any],
                ..LinkGrants::default()
            };
            let alive = |pid: u32| pid == std::process::id();
            assert!(
                pending_links_from(&harness_asks::load_all(&f.dir, 200), &any, &alive).is_empty(),
                "covered by a grant"
            );
            assert_eq!(
                pending_links_from(&harness_asks::load_all(&local.dir, 200), &any, &alive).len(),
                1,
                "any site never covers a local one"
            );
        }

        /// SPEC R-LINK.6, R-GUI.9: a surface that may not save (`ahma web`,
        /// `ahma desktop`) gives what both rules allow — deny and this site for
        /// this session; not "this time", since nothing waits — and names each
        /// lasting answer as the grant line a human runs instead.
        #[test]
        fn a_surface_that_may_not_save_gives_deny_and_this_session_only() {
            for (host, local) in [("github.com", false), ("localhost", true)] {
                let f = link_fixture(host, local);
                let ask = waiting(&f).remove(0);
                assert_eq!(ask.unsaved_choices(), [Deny, Session], "{host}");
                assert_eq!(ask.grant_command(Deny), None);
                assert_eq!(ask.grant_command(Session), None, "nothing saved");
                assert_eq!(
                    ask.grant_command(AlwaysSiteAllRepos).as_deref(),
                    Some(format!("ahma permissions grant open-url {host}").as_str())
                );
                let this_repo = ask.grant_command(AlwaysSiteThisRepo).unwrap();
                assert!(
                    this_repo.contains(" --workspace ")
                        && this_repo.contains(&f.ws.display().to_string()),
                    "{this_repo}"
                );
            }
        }

        /// SPEC R-LINK.4: "this site, this session" is a session grant file
        /// bound to the harness process and the repo — what the hook reads —
        /// audited from the TUI; the refusal is settled, so neither the TUI nor
        /// the harness dialog asks it again (R-PERM.3.8).
        #[test]
        fn a_session_answer_is_the_session_grant_the_hook_reads() {
            let f = link_fixture("github.com", false);
            assert!(dialog_asks(&f));
            let line = answer(&f, Session);
            assert!(line.contains("until the agent's harness exits"), "{line}");
            let grants = ahma_common::open_url::active_sessions(&f.sessions, 202, &|pid| {
                pid == std::process::id()
            });
            assert_eq!(grants.len(), 1, "{grants:?}");
            assert!(grants[0].covers("github.com", &f.ws));
            assert_eq!(grants[0].granted_by.as_str(), Some("tui"));
            assert!(waiting(&f).is_empty(), "answered");
            assert!(!dialog_asks(&f), "the dialog does not ask it too");
            assert_eq!(
                audited(&f),
                [(AuditAction::Grant, GrantTier::Session, Some("tui".into()))]
            );
        }

        /// The lasting answers are written by the one audited writer, for every
        /// repo or for this one, and settle the refusal.
        #[test]
        fn an_always_answer_is_saved_for_all_repos_or_this_repo() {
            let f = link_fixture("github.com", false);
            answer(&f, AlwaysSiteThisRepo);
            let saved = ahma_common::config::AhmaSettings::load_from_result(&f.settings).unwrap();
            assert!(saved.open_url.allow.is_empty());
            assert_eq!(saved.open_url.allowed_in(&f.ws), ["github.com"]);
            assert!(!dialog_asks(&f));

            let g = link_fixture("docs.rs", false);
            answer(&g, AlwaysSiteAllRepos);
            let saved = ahma_common::config::AhmaSettings::load_from_result(&g.settings).unwrap();
            assert_eq!(saved.open_url.allow, ["docs.rs"]);
            assert_eq!(
                audited(&g),
                [(AuditAction::Grant, GrantTier::Always, Some("tui".into()))]
            );
        }

        /// SPEC R-LINK.6: a deny is remembered for the harness's session — the
        /// site is not listed or asked in its dialog again, even when refused
        /// again — and an answer this surface does not offer ("this time") is
        /// a deny, never widened.
        #[test]
        fn a_deny_is_remembered_for_the_harness_session_and_once_is_a_deny() {
            for choice in [Deny, Once] {
                let f = link_fixture("github.com", false);
                let line = answer(&f, choice);
                assert!(line.starts_with("Denied"), "{choice:?}: {line}");
                let asks = harness_asks::load(&f.dir, &f.ws, 202);
                assert!(harness_asks::link_denied_for(
                    &asks,
                    Some(std::process::id()),
                    "github.com"
                ));
                assert!(waiting(&f).is_empty());
                assert!(!dialog_asks(&f));
                assert!(
                    !f.sessions.exists() && !f.settings.exists(),
                    "a deny grants nothing"
                );
                assert_eq!(
                    audited(&f),
                    [(AuditAction::Deny, GrantTier::Session, Some("tui".into()))]
                );
            }
        }
    }
}
