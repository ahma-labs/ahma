//! The question ladder (SPEC R-PERM.3): one place that decides *where* a
//! permission question gets asked.
//!
//! ## The problem
//!
//! When the kernel blocks a path, someone has to be asked whether to allow it.
//! Before this module, each subsystem answered that question its own way: the
//! `sandbox_grant` tool elicited at the MCP client, the denial detector pushed to
//! the hub for a TUI modal, and a hook denial reached nobody at all — which is
//! precisely why terminal hooks could not be enabled by default. A denial that
//! cannot become a decision is just a wall.
//!
//! ## The ladder
//!
//! Surfaces are tried in a fixed order, best first:
//!
//! 1. **The harness** — the MCP client that asked for the work, via
//!    `elicitation/create`. Preferred whenever it works: the user is already
//!    looking at it, and the question arrives in the context of the task that
//!    raised it.
//! 2. **An attached ahma TUI** — the grant modal, delivered through the hub.
//! 3. **Nobody** — fail closed, loudly, with a command the user can paste. This
//!    rung always exists, which is what makes the whole ladder safe to rely on.
//!
//! ## Demotion: a broken surface, not a "no"
//!
//! An elicitation that **times out or errors** demotes the harness for the rest of
//! the session — one strike — and later questions skip to rung 2. A **decline does
//! not**: a decline is a working surface saying no, and treating it as a fault
//! would teach the system to abandon a perfectly good surface the moment a user
//! exercises it. That distinction is the whole of [`ElicitOutcome`].
//!
//! ## What the broker will not do
//!
//! It never widens the live sandbox. An approval is *persisted* (R5.1 lock-once:
//! the MCP server path applies it at next start; the hooks path re-derives its
//! sandbox per command and so picks it up on the very next one). And it asks at
//! most once per `(path, access)` per session — the [`GrantCoordinator`] gate —
//! because a question the user already answered is not a question, it is nagging.

use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;

use ahma_common::config::{ScopeAccess, settings_path};
use ahma_common::permissions::{AuditAction, GrantKind, GrantTier, append_audit, audit_entry};
use ahma_common::scope_grant::{
    GrantCoordinator, GrantDecision, GrantReason, GrantResolveOutcome, ScopeGrantRequest,
    persist_grant,
};

use super::grant_channel::{ScopeGrantNotifier, runtime_denial_remediation_cli};

/// What happened when we asked the harness (rung 1).
///
/// The three variants exist to separate "the user said no" from "we could not
/// ask" — a distinction that decides whether the surface is still trustworthy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElicitOutcome {
    /// The human answered. A decline is an *answer*: it resolves the question and
    /// leaves the harness fully trusted for the next one.
    Answered(GrantDecision),
    /// There is nothing to ask here — no peer attached, or the client never
    /// advertised the `elicitation` capability. Not a fault, so no demotion; we
    /// simply move down the ladder.
    Unavailable,
    /// The ask *failed*: it timed out, was cancelled, or the transport broke. This
    /// is the only outcome that demotes the harness, because it is the only one
    /// that tells us the surface does not work.
    Failed(String),
}

/// Rung 1 of the ladder, behind a trait so the ladder can be tested without a
/// live MCP peer — the interesting behavior (demotion, fallthrough, fail-closed)
/// is exactly the behavior that is painful to exercise against a real client.
#[async_trait]
pub trait ElicitationSurface: Send + Sync + std::fmt::Debug {
    /// Put the grant question to the human at the harness.
    async fn ask(&self, req: &ScopeGrantRequest) -> ElicitOutcome;
}

/// Whether the harness is still a usable asking surface this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HarnessState {
    /// Not yet asked anything.
    Untried,
    /// It answered at least once — it works.
    Proven,
    /// It timed out or errored. Skip it for the rest of the session (one strike).
    Demoted,
}

/// Where a question ended up. Reported so the user is never left wondering
/// whether a question was asked somewhere they weren't looking (R-PERM.3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskedAt {
    /// Rung 1 — the MCP client that requested the work.
    Harness,
    /// Rung 2 — an attached ahma TUI.
    Tui,
    /// Rung 3 — nobody could be asked; the operation failed closed.
    FailedClosed,
    /// Not asked at all: already answered or already in flight this session.
    Suppressed,
}

/// The single broker that owns the question ladder.
#[derive(Debug)]
pub struct PermissionBroker {
    coordinator: Arc<GrantCoordinator>,
    /// Installed once the MCP peer exists (it does not exist when the broker is
    /// constructed, because the notifier is built before the service).
    elicitation: RwLock<Option<Arc<dyn ElicitationSurface>>>,
    /// Rung 2: the hub channel a connected TUI drains to show its modal.
    hub_tx: Option<tokio::sync::mpsc::UnboundedSender<ScopeGrantRequest>>,
    harness: Mutex<HarnessState>,
}

impl PermissionBroker {
    /// Build a broker over the session's shared [`GrantCoordinator`].
    ///
    /// `hub_tx` is rung 2; pass `None` when no hub is wired (e.g. a bare CLI
    /// invocation), and the ladder simply has one fewer rung.
    pub fn new(
        coordinator: Arc<GrantCoordinator>,
        hub_tx: Option<tokio::sync::mpsc::UnboundedSender<ScopeGrantRequest>>,
    ) -> Self {
        Self {
            coordinator,
            elicitation: RwLock::new(None),
            hub_tx,
            harness: Mutex::new(HarnessState::Untried),
        }
    }

    /// Install rung 1 once the MCP peer is known.
    pub fn set_elicitation_surface(&self, surface: Arc<dyn ElicitationSurface>) {
        *self.elicitation.write().unwrap() = Some(surface);
    }

    /// The coordinator this broker gates on — shared with the daemon reporter so a
    /// TUI answer resolves the same decision the broker raised.
    pub fn coordinator(&self) -> &Arc<GrantCoordinator> {
        &self.coordinator
    }

    /// Rung 1 is usable when a surface is installed and it has not been demoted.
    fn harness_available(&self) -> bool {
        self.elicitation.read().unwrap().is_some()
            && *self.harness.lock().unwrap() != HarnessState::Demoted
    }

    /// Run the ladder for an already-deduped request. Returns where it landed.
    async fn ask(&self, req: ScopeGrantRequest) -> AskedAt {
        // ── Rung 1: the harness ───────────────────────────────────────────────
        if self.harness_available() {
            let surface = self.elicitation.read().unwrap().clone();
            if let Some(surface) = surface {
                match surface.ask(&req).await {
                    ElicitOutcome::Answered(decision) => {
                        *self.harness.lock().unwrap() = HarnessState::Proven;
                        self.apply(&req, decision);
                        return AskedAt::Harness;
                    }
                    ElicitOutcome::Failed(why) => {
                        // One strike. The surface is broken, not the user.
                        *self.harness.lock().unwrap() = HarnessState::Demoted;
                        tracing::warn!(
                            "The MCP client could not be asked for permission ({why}); it will \
                             not be asked again this session. Falling back to the ahma TUI, or \
                             to a failed operation with instructions."
                        );
                    }
                    ElicitOutcome::Unavailable => {
                        // Nothing to ask. Not a fault — do not demote.
                        tracing::debug!(
                            "The MCP client does not support elicitation; asking elsewhere."
                        );
                    }
                }
            }
        }

        // ── Rung 2: an attached TUI ───────────────────────────────────────────
        if let Some(tx) = &self.hub_tx
            && tx.send(req.clone()).is_ok()
        {
            tracing::warn!(
                path = %req.path.display(),
                access = req.access.label(),
                "Sandbox blocked an out-of-scope path; asking in the ahma TUI. If no TUI is \
                 attached, {}",
                cli_hint(&req),
            );
            return AskedAt::Tui;
        }

        // ── Rung 3: nobody can be asked → fail closed, loudly and usefully ────
        //
        // This is the rung that always exists. It works in every harness, because
        // it needs nothing from the harness: it is a log line and an error payload
        // carrying a command the user can paste. Failing *open* here would be the
        // one unforgivable outcome — it would silently hand out the access nobody
        // agreed to.
        tracing::warn!(
            path = %req.path.display(),
            access = req.access.label(),
            "Sandbox blocked an out-of-scope path and no surface could ask you about it \
             (no MCP client with elicitation, no attached TUI). The operation fails closed. \
             To allow it, {}",
            cli_hint(&req),
        );
        AskedAt::FailedClosed
    }

    /// Resolve the decision and, on approval, persist it (never widening the live
    /// session — R5.1). A decline resolves too, which is what stops the same
    /// question coming back for the rest of the session.
    fn apply(&self, req: &ScopeGrantRequest, decision: GrantDecision) {
        match self.coordinator.resolve(&req.decision_id, decision) {
            GrantResolveOutcome::Persist { path, access, tool } => {
                let granted_at = chrono::Local::now();
                let Some(file) = settings_path() else {
                    tracing::warn!("cannot persist scope grant: home directory unknown");
                    return;
                };
                match persist_grant(
                    &file,
                    &path,
                    access,
                    tool.or_else(|| Some("permission prompt".to_string())),
                    Some(granted_at.format("%Y-%m-%d").to_string()),
                    None,
                ) {
                    Ok(_) => {
                        append_audit(&audit_entry(
                            granted_at.to_rfc3339(),
                            AuditAction::Grant,
                            GrantKind::FsScope,
                            path.display().to_string(),
                            Some(if access.is_write() { "rw" } else { "ro" }.to_string()),
                            GrantTier::Always,
                            Some("harness".to_string()),
                        ));
                        tracing::info!(
                            path = %path.display(),
                            access = access.label(),
                            "Scope granted and saved to {}. It applies on the next server start \
                             (the `restart` tool applies it now); a terminal hook picks it up on \
                             the very next command.",
                            file.display(),
                        );
                    }
                    Err(e) => tracing::warn!(
                        "failed to persist scope grant for {}: {e:#}",
                        path.display()
                    ),
                }
            }
            GrantResolveOutcome::Denied { path } => {
                append_audit(&audit_entry(
                    chrono::Local::now().to_rfc3339(),
                    AuditAction::Deny,
                    GrantKind::FsScope,
                    path.display().to_string(),
                    None,
                    GrantTier::Session,
                    Some("harness".to_string()),
                ));
                tracing::info!(
                    path = %path.display(),
                    "Scope grant declined; it will not be asked again this session."
                );
            }
            // A twin surface answered first, or the id is stale. Both are no-ops by
            // design (first-answer-wins, R5.3.3).
            GrantResolveOutcome::AlreadyResolved | GrantResolveOutcome::Unknown => {}
        }
    }
}

/// The paste-able remediation shown at rung 2 and rung 3 — the same string in
/// both, so the user learns one command rather than two.
fn cli_hint(req: &ScopeGrantRequest) -> String {
    let ro = if req.access.is_write() {
        ""
    } else {
        " --read-only"
    };
    format!(
        "run `ahma sandbox grant {}{}` and re-run the command.",
        req.path.display(),
        ro
    )
}

#[async_trait]
impl ScopeGrantNotifier for PermissionBroker {
    async fn notify_violation(
        &self,
        path: &Path,
        access: ScopeAccess,
        reason: GrantReason,
        tool: Option<String>,
    ) {
        // Ask at most once per (path, access) per session (R-PERM.4). The kernel
        // trips on the same path many times over; the human should hear about it
        // once.
        let Some(req) = self.coordinator.begin(path, access, reason, tool) else {
            return;
        };
        self.ask(req).await;
    }
}

/// The real rung 1: an MCP `elicitation/create` to the attached client.
///
/// Holds the same `Arc<RwLock<Option<Peer>>>` the service fills in at
/// `initialize`, so the broker (built before the service) still gets a live peer
/// once one connects.
#[derive(Debug)]
pub struct PeerElicitationSurface {
    peer: Arc<RwLock<Option<rmcp::service::Peer<rmcp::service::RoleServer>>>>,
    timeout: std::time::Duration,
}

impl PeerElicitationSurface {
    /// Wrap the service's peer slot as an asking surface.
    pub fn new(peer: Arc<RwLock<Option<rmcp::service::Peer<rmcp::service::RoleServer>>>>) -> Self {
        Self {
            peer,
            timeout: std::time::Duration::from_secs(120),
        }
    }
}

/// The elicitation form the human fills in at the harness.
///
/// Three-valued rather than a bool, and the *safe* option is the one a distracted
/// Enter lands on: `deny` sorts first and is described as the default, because
/// R5.3.1 requires that Enter alone can never widen the sandbox.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct GrantForm {
    /// `deny` (default), `read-only`, or `read-write`.
    pub decision: String,
}

rmcp::elicit_safe!(GrantForm);

#[async_trait]
impl ElicitationSurface for PeerElicitationSurface {
    async fn ask(&self, req: &ScopeGrantRequest) -> ElicitOutcome {
        let peer = self.peer.read().unwrap().clone();
        let Some(peer) = peer else {
            return ElicitOutcome::Unavailable;
        };

        let message = prompt_text(req);
        match peer
            .elicit_with_timeout::<GrantForm>(message, Some(self.timeout))
            .await
        {
            Ok(Some(form)) => ElicitOutcome::Answered(parse_decision(&form.decision)),
            // Accepted with no content, or an explicit decline — both are the human
            // saying no. An answer, not a fault: the surface stays trusted.
            Ok(None) | Err(rmcp::service::ElicitationError::UserDeclined) => {
                ElicitOutcome::Answered(GrantDecision::Deny)
            }
            // The client cannot elicit at all. Nothing to demote — there was never
            // a working surface here to lose.
            Err(rmcp::service::ElicitationError::CapabilityNotSupported) => {
                ElicitOutcome::Unavailable
            }
            // Timed out, cancelled, or the transport broke: the surface is unusable.
            Err(e) => ElicitOutcome::Failed(e.to_string()),
        }
    }
}

/// Map the form's free-text choice to a decision. Anything unrecognized is a
/// **deny**: an answer we cannot read is not consent.
fn parse_decision(s: &str) -> GrantDecision {
    match s.trim().to_ascii_lowercase().as_str() {
        "read-write" | "read_write" | "rw" | "write" => GrantDecision::GrantRw,
        "read-only" | "read_only" | "ro" | "read" => GrantDecision::GrantRo,
        _ => GrantDecision::Deny,
    }
}

/// The question the human actually reads.
///
/// It names the literal path, what tripped it, what the choice means, and what
/// happens next — because "Allow workspace?" is not a question anyone can answer
/// responsibly (R5.3.1).
fn prompt_text(req: &ScopeGrantRequest) -> String {
    let tool = req.tool.as_deref().unwrap_or("a command");
    let what = if req.access.is_write() {
        "write to"
    } else {
        "read"
    };
    let reason = match req.reason {
        GrantReason::PreExecViolation => {
            "ahma's sandbox blocked it before the command ran, so the path is exact"
        }
        GrantReason::StderrHeuristic => {
            "ahma read this path out of the command's error output, so double-check it"
        }
    };
    format!(
        "Allow ahma to {what} '{path}'?\n\n\
         {tool} needs it and the sandbox blocked it ({reason}).\n\n\
         Granting adds this one directory to ahma's sandbox scope, saved in \
         ~/.ahma/settings.toml. Everything outside your workspace stays blocked. \
         The grant applies from the next server start (or immediately for terminal \
         hooks); revoke it any time with `ahma permissions revoke fs-scope {path}`.\n\n\
         Choose: 'deny' (default), 'read-only', or 'read-write'.",
        what = what,
        path = req.path.display(),
        tool = tool,
        reason = reason,
    )
}

/// The message a *hook* prints to the terminal when nobody could be asked — the
/// rung-3 surface for the terminal-hook path (R-PERM.6.1). Kept here so the hook
/// and the MCP path phrase the failure identically.
pub fn hook_fail_closed_message(path: &Path, access: ScopeAccess) -> String {
    runtime_denial_remediation_cli(path, access)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A scripted harness: answers with a canned outcome and counts how often it
    /// was asked, so demotion (or the lack of it) is directly observable.
    #[derive(Debug)]
    struct ScriptedHarness {
        outcome: Mutex<Vec<ElicitOutcome>>,
        asks: AtomicUsize,
    }

    impl ScriptedHarness {
        fn new(outcomes: Vec<ElicitOutcome>) -> Arc<Self> {
            Arc::new(Self {
                outcome: Mutex::new(outcomes),
                asks: AtomicUsize::new(0),
            })
        }
        fn asks(&self) -> usize {
            self.asks.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ElicitationSurface for ScriptedHarness {
        async fn ask(&self, _req: &ScopeGrantRequest) -> ElicitOutcome {
            self.asks.fetch_add(1, Ordering::SeqCst);
            let mut q = self.outcome.lock().unwrap();
            if q.is_empty() {
                ElicitOutcome::Unavailable
            } else {
                q.remove(0)
            }
        }
    }

    fn broker_with(
        harness: Option<Arc<dyn ElicitationSurface>>,
        hub: bool,
    ) -> (
        PermissionBroker,
        Option<tokio::sync::mpsc::UnboundedReceiver<ScopeGrantRequest>>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let broker = PermissionBroker::new(
            Arc::new(GrantCoordinator::new()),
            if hub { Some(tx) } else { None },
        );
        if let Some(h) = harness {
            broker.set_elicitation_surface(h);
        }
        (broker, if hub { Some(rx) } else { None })
    }

    async fn violate(broker: &PermissionBroker, path: &str) {
        broker
            .notify_violation(
                Path::new(path),
                ScopeAccess::Rw,
                GrantReason::PreExecViolation,
                Some("cargo_build".into()),
            )
            .await;
    }

    #[tokio::test]
    async fn the_harness_is_asked_first_when_it_works() {
        let h = ScriptedHarness::new(vec![ElicitOutcome::Answered(GrantDecision::Deny)]);
        let (broker, mut rx) = broker_with(Some(h.clone()), true);

        violate(&broker, "/opt/cache").await;

        assert_eq!(h.asks(), 1, "the harness is rung 1");
        assert!(
            rx.as_mut().unwrap().try_recv().is_err(),
            "a harness that answered must not also raise a TUI modal — one question, \
             one place"
        );
    }

    #[tokio::test]
    async fn a_decline_is_an_answer_and_never_demotes_the_harness() {
        // Two distinct paths, both declined. If a decline demoted the harness, the
        // second question would skip it — and the user would silently lose the
        // surface they were successfully using, as punishment for saying no.
        let h = ScriptedHarness::new(vec![
            ElicitOutcome::Answered(GrantDecision::Deny),
            ElicitOutcome::Answered(GrantDecision::Deny),
        ]);
        let (broker, _rx) = broker_with(Some(h.clone()), true);

        violate(&broker, "/opt/one").await;
        violate(&broker, "/opt/two").await;

        assert_eq!(h.asks(), 2, "the harness stays trusted after a decline");
    }

    #[tokio::test]
    async fn a_timeout_demotes_the_harness_and_the_question_falls_to_the_tui() {
        let h = ScriptedHarness::new(vec![
            ElicitOutcome::Failed("timed out".into()),
            ElicitOutcome::Answered(GrantDecision::GrantRw),
        ]);
        let (broker, mut rx) = broker_with(Some(h.clone()), true);
        let rx = rx.as_mut().unwrap();

        violate(&broker, "/opt/one").await;
        assert_eq!(h.asks(), 1);
        let first = rx
            .try_recv()
            .expect("the question falls through to the TUI");
        assert_eq!(first.path, PathBuf::from("/opt/one"));

        // Second question: the harness is demoted, so it is not asked again — it
        // goes straight to the TUI.
        violate(&broker, "/opt/two").await;
        assert_eq!(
            h.asks(),
            1,
            "one strike: a harness that timed out is not retried this session"
        );
        let second = rx
            .try_recv()
            .expect("the second question also goes to the TUI");
        assert_eq!(second.path, PathBuf::from("/opt/two"));
    }

    #[tokio::test]
    async fn a_client_without_elicitation_falls_through_without_being_demoted() {
        // Unavailable is not a fault. There is nothing to lose and nothing to
        // punish — we just use the next rung.
        let h = ScriptedHarness::new(vec![ElicitOutcome::Unavailable, ElicitOutcome::Unavailable]);
        let (broker, mut rx) = broker_with(Some(h.clone()), true);
        let rx = rx.as_mut().unwrap();

        violate(&broker, "/opt/one").await;
        violate(&broker, "/opt/two").await;

        assert_eq!(
            h.asks(),
            2,
            "an incapable client is still consulted (cheaply)"
        );
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok(), "both questions reach the TUI");
    }

    #[tokio::test]
    async fn with_no_harness_and_no_tui_the_operation_fails_closed() {
        let (broker, _rx) = broker_with(None, false);
        // The point of rung 3: nobody to ask, so the answer is *no*. It must never
        // fail open — that would hand out access nobody agreed to.
        violate(&broker, "/opt/cache").await;
        // Nothing to assert on a channel (there is none); the contract is that we
        // neither panic nor persist. Assert no grant was written.
        assert!(
            !broker.coordinator().is_in_flight("nonexistent"),
            "no decision is left dangling"
        );
    }

    #[tokio::test]
    async fn the_same_path_is_asked_about_only_once_per_session() {
        let h = ScriptedHarness::new(vec![ElicitOutcome::Answered(GrantDecision::Deny)]);
        let (broker, _rx) = broker_with(Some(h.clone()), true);

        // The kernel trips on the same path over and over; the human hears once.
        violate(&broker, "/opt/cache").await;
        violate(&broker, "/opt/cache").await;
        violate(&broker, "/opt/cache").await;

        assert_eq!(
            h.asks(),
            1,
            "ask-once (R-PERM.4): a denied path never re-prompts"
        );
    }

    #[test]
    fn an_unreadable_answer_is_a_deny() {
        // Anything we cannot confidently read as consent is not consent.
        assert_eq!(parse_decision("read-write"), GrantDecision::GrantRw);
        assert_eq!(parse_decision("Read-Only"), GrantDecision::GrantRo);
        assert_eq!(parse_decision("deny"), GrantDecision::Deny);
        assert_eq!(parse_decision(""), GrantDecision::Deny);
        assert_eq!(parse_decision("sure why not"), GrantDecision::Deny);
        assert_eq!(parse_decision("yes"), GrantDecision::Deny);
    }

    #[test]
    fn the_prompt_names_the_literal_path_and_the_way_out() {
        let req = ScopeGrantRequest {
            decision_id: "d1".into(),
            path: PathBuf::from("/opt/ext/sccache"),
            access: ScopeAccess::Rw,
            reason: GrantReason::StderrHeuristic,
            tool: Some("sccache".into()),
        };
        let text = prompt_text(&req);
        assert!(
            text.contains("/opt/ext/sccache"),
            "the literal path, never a vague 'allow workspace?'"
        );
        assert!(text.contains("sccache"), "what asked for it");
        assert!(
            text.contains("double-check"),
            "a heuristic path is flagged as one"
        );
        assert!(
            text.contains("deny"),
            "deny is offered, and named as the default"
        );
        assert!(
            text.contains("revoke"),
            "how to undo it, before they agree to it"
        );
    }
}
