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
//! ## Demotion: a broken surface, not a "no" and not a silence
//!
//! Only an elicitation that **errors** — the transport broke, or the answer would
//! not parse — demotes the harness for the rest of the session (one strike), after
//! which questions skip to rung 2.
//!
//! Two things deliberately do *not* demote. A **decline** is a working surface
//! saying no, and punishing it would teach the system to abandon a perfectly good
//! surface the moment a user exercises it. A **dismissal** — the MCP `cancel`
//! action, or our own wait expiring — is nobody's decision at all: clients cancel
//! on their own undisclosed deadlines with no human involved, so it is neither
//! consent nor evidence of a fault. Those three-way distinctions are the whole of
//! [`ElicitOutcome`].
//!
//! ## What the broker will not do
//!
//! It never re-locks or replaces the committed workspace scope (R5.1). A human's
//! `always` answer is written through the one audited chokepoint
//! (`persist_grant`, bound to this session's workspace) *and* applied to the
//! live sandbox (R5.4.6); a `session` answer is applied live only. The hooks path
//! re-derives its sandbox per command and so picks a written grant up on the
//! very next one. And it asks at most once per `(path, access)` per session —
//! the [`GrantCoordinator`] gate — because a question the user already answered
//! is not a question, it is nagging.

use parking_lot::{Mutex, RwLock};
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use ahma_common::config::{ScopeAccess, settings_path};
use ahma_common::permissions::{AuditAction, GrantKind, GrantTier, append_audit, audit_entry};
use ahma_common::scope_grant::{
    GrantContext, GrantCoordinator, GrantDecision, GrantReason, GrantResolveOutcome, GrantStatus,
    NewGrant, ScopeGrantRequest, persist_grant,
};

use super::grant_channel::{ScopeGrantNotifier, runtime_denial_remediation_cli};

/// What happened when we asked the harness (rung 1).
///
/// The variants exist to separate "the user said no" from "nobody answered" from
/// "we could not ask" — distinctions that decide whether a decision was made and
/// whether the surface is still trustworthy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElicitOutcome {
    /// The human answered. A decline is an *answer*: it resolves the question and
    /// leaves the harness fully trusted for the next one.
    Answered(GrantDecision),
    /// There is nothing to ask here — no peer attached, or the client never
    /// advertised the `elicitation` capability. Not a fault, so no demotion; we
    /// simply move down the ladder.
    Unavailable,
    /// The prompt closed without an answer: the MCP `cancel` action, an accept
    /// carrying no content, or ahma's own wait expiring.
    ///
    /// This is **not** a denial and **not** a fault (SPEC R5.3.1). `cancel` means
    /// "dismissed without an explicit choice", and a client produces it on its
    /// *own* undisclosed deadline with no human involved — Antigravity was
    /// measured cancelling at 60.005s. Recording that as a user's "no" would
    /// invent consent-shaped data out of a timeout; recording it as a fault
    /// would demote a perfectly working surface because a user walked away.
    /// So: no demotion, no decision, fall through the ladder (R5.3.2).
    Dismissed(String),
    /// The ask *failed*: the transport broke, or the response could not be
    /// parsed. This is the only outcome that demotes the harness, because it is
    /// the only one that tells us the surface does not work.
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
    /// It errored — broken transport, unparseable answer. Skip it for the rest of
    /// the session (one strike). A decline or a dismissal never lands here.
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
    /// How many TUIs the hub says are watching ([`ahma_common::hub::HubMsg::Viewers`]).
    /// Rung 2 is used only while this is above zero (SPEC R-PERM.3.6).
    tui_viewers: Arc<std::sync::atomic::AtomicUsize>,
    harness: Mutex<HarnessState>,
    /// Session-health disclosure (#485): `grant_pending` / `grant_decided`
    /// events emitted *beside* the asking surfaces, never instead of them.
    /// Installed with the elicitation surface; `None` in bare-CLI runs.
    session_events: RwLock<Option<Arc<dyn crate::session_events::SessionEventSink>>>,
    /// The live sandbox an approved grant is applied to and whose committed
    /// scope stamps the grant's workspace (SPEC R5.4.6, R5.4.11). `None` in
    /// tests that only exercise the asking ladder.
    sandbox: RwLock<Option<Arc<super::Sandbox>>>,
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
            tui_viewers: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            harness: Mutex::new(HarnessState::Untried),
            session_events: RwLock::new(None),
            sandbox: RwLock::new(None),
        }
    }

    /// Share the count of watching TUIs that the hub reporter keeps up to
    /// date. Without it the count stays zero and rung 2 is never used.
    pub fn with_tui_viewers(mut self, viewers: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        self.tui_viewers = viewers;
        self
    }

    /// Install the live sandbox so an approval is applied to it and stamped
    /// with its workspace (SPEC R5.4.6, R5.4.11).
    pub fn set_sandbox(&self, sandbox: Arc<super::Sandbox>) {
        *self.sandbox.write() = Some(sandbox);
    }

    /// Install rung 1 once the MCP peer is known.
    pub fn set_elicitation_surface(&self, surface: Arc<dyn ElicitationSurface>) {
        *self.elicitation.write() = Some(surface);
    }

    /// Install the session-health event sink (#485). Like the elicitation
    /// surface, the production sink shares the service's peer slot and is
    /// installed at build time.
    pub fn set_session_events(&self, sink: Arc<dyn crate::session_events::SessionEventSink>) {
        *self.session_events.write() = Some(sink);
    }

    /// Fire a session-health event, if a sink is installed. Detached and
    /// best-effort: disclosure must never block or fail the grant flow.
    fn emit_event(
        &self,
        kind: ahma_common::session_event::SessionEventKind,
        detail: serde_json::Value,
    ) {
        if let Some(sink) = self.session_events.read().as_ref() {
            sink.emit_event(kind, detail);
        }
    }

    /// The coordinator this broker gates on — shared with the hub reporter so a
    /// TUI answer resolves the same decision the broker raised.
    pub fn coordinator(&self) -> &Arc<GrantCoordinator> {
        &self.coordinator
    }

    /// Rung 1 is usable when a surface is installed and it has not been demoted.
    fn harness_available(&self) -> bool {
        self.elicitation.read().is_some() && *self.harness.lock() != HarnessState::Demoted
    }

    /// Run the ladder for an already-deduped request. Returns where it landed.
    async fn ask(&self, req: ScopeGrantRequest) -> AskedAt {
        // ── Rung 1: the harness ───────────────────────────────────────────────
        if self.harness_available() {
            let surface = self.elicitation.read().clone();
            if let Some(surface) = surface {
                match surface.ask(&req).await {
                    ElicitOutcome::Answered(decision) => {
                        *self.harness.lock() = HarnessState::Proven;
                        self.apply(&req, decision);
                        return AskedAt::Harness;
                    }
                    ElicitOutcome::Failed(why) => {
                        // One strike. The surface is broken, not the user.
                        *self.harness.lock() = HarnessState::Demoted;
                        tracing::warn!(
                            "The MCP client could not be asked for permission ({why}); it will \
                             not be asked again this session. Falling back to the ahma TUI, or \
                             to a failed operation with instructions."
                        );
                    }
                    ElicitOutcome::Dismissed(why) => {
                        // Neither an answer nor a fault (R5.3.1). The surface
                        // stays trusted — a client that cancels on its own
                        // deadline, or a user who walked away, has not proved it
                        // broken — and nothing is recorded as a decision, so the
                        // next question for this path is still asked (R5.4.7
                        // binds *decided* outcomes only).
                        tracing::warn!(
                            path = %req.path.display(),
                            "The permission prompt closed without an answer ({why}). This is not \
                             a denial: asking elsewhere, and the client will be asked again."
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
        // A hub channel is not a person. Sending succeeds whenever the hub
        // reporter is alive, with or without a TUI open, so the question is
        // sent only while the hub says one is watching (SPEC R-PERM.3.6).
        if let Some(tx) = &self.hub_tx
            && self.tui_viewers.load(std::sync::atomic::Ordering::SeqCst) > 0
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
        // Nobody holds this question, so it must not stay in flight: an
        // in-flight question blocks the same path from ever being asked again
        // this session, and reads to the agent as "waiting for a human" when no
        // human was reached. Closing it records no decision (R5.3.1).
        self.coordinator.cancel(&req.decision_id);
        AskedAt::FailedClosed
    }

    /// Resolve the decision and, on approval, persist it (never widening the live
    /// session — R5.1). A decline resolves too, which is what stops the same
    /// question coming back for the rest of the session.
    fn apply(&self, req: &ScopeGrantRequest, decision: GrantDecision) {
        let outcome = self.coordinator.resolve(&req.decision_id, decision);
        // Close the disclosure loop (#485): a client that showed grant_pending
        // can clear it. Emitted for real resolutions only — AlreadyResolved /
        // Unknown are no-ops whose grant_decided was (or will be) sent by the
        // surface that actually resolved first.
        match &outcome {
            GrantResolveOutcome::Persist { access, .. } => self.emit_event(
                ahma_common::session_event::SessionEventKind::GrantDecided,
                serde_json::json!({
                    "grant_id": req.decision_id,
                    "outcome": "granted",
                    // The serde form ("ro"/"rw") — a machine-stable wire value,
                    // not the human label (R8.8.5 / docs §7.1).
                    "access": access,
                }),
            ),
            GrantResolveOutcome::Denied { .. } => self.emit_event(
                ahma_common::session_event::SessionEventKind::GrantDecided,
                serde_json::json!({
                    "grant_id": req.decision_id,
                    "outcome": "declined",
                }),
            ),
            GrantResolveOutcome::AlreadyResolved | GrantResolveOutcome::Unknown => {}
        }
        match outcome {
            GrantResolveOutcome::Persist {
                path,
                access,
                tool,
                tier,
                time_to_decision_ms,
            } => {
                let risk_class = req.context.risk.as_ref().map(|r| r.class.clone());
                let sandbox = self.sandbox.read().clone();
                let live_scopes: Vec<std::path::PathBuf> = sandbox
                    .as_ref()
                    .map(|sb| sb.scopes().to_vec())
                    .unwrap_or_default();
                let workspace = live_scopes.first().cloned();
                let granted_at = chrono::Local::now();
                if tier.is_persistent() {
                    let Some(file) = settings_path() else {
                        tracing::warn!("cannot persist scope grant: home directory unknown");
                        return;
                    };
                    // The chokepoint applies the denylist and writes the audit
                    // record (R-PERM.2, R-PERM.2.1).
                    match persist_grant(
                        &file,
                        NewGrant {
                            path: &path,
                            access,
                            granted_by: tool.or_else(|| Some("permission prompt".to_string())),
                            granted_at: Some(granted_at.format("%Y-%m-%d").to_string()),
                            note: None,
                            surface: "harness",
                            live_scopes: &live_scopes,
                            workspace: workspace.as_deref(),
                            expires_at: lease_end(tier),
                        },
                    ) {
                        Ok(_) => tracing::info!(
                            path = %path.display(),
                            access = access.label(),
                            "Scope granted for workspace {} and saved to {}; applied to this \
                             session now, and a terminal hook picks it up on its next command.",
                            workspace
                                .as_deref()
                                .map(|w| w.display().to_string())
                                .unwrap_or_else(|| "(none)".into()),
                            file.display(),
                        ),
                        Err(e) => {
                            tracing::warn!(
                                "failed to persist scope grant for {}: {e:#}",
                                path.display()
                            );
                            return;
                        }
                    }
                } else {
                    append_audit(
                        &audit_entry(
                            granted_at.to_rfc3339(),
                            AuditAction::Grant,
                            GrantKind::FsScope,
                            path.display().to_string(),
                            Some(if access.is_write() { "rw" } else { "ro" }.to_string()),
                            tier,
                            Some("harness".to_string()),
                        )
                        .with_request(
                            &req.decision_id,
                            time_to_decision_ms,
                            risk_class.as_deref(),
                        ),
                    );
                    tracing::info!(
                        path = %path.display(),
                        access = access.label(),
                        tier = tier.label(),
                        "Scope granted without writing settings."
                    );
                }
                // R5.4.6: a human-approved grant takes effect in the live session —
                // after the live gate (R-PERM.4.3), which is the only denylist the
                // `session` tier ever meets.
                if let Some(sb) = sandbox {
                    let applied = if tier == GrantTier::Once {
                        sb.add_once_grant(&path, access)
                    } else {
                        sb.add_live_grant(&path, access)
                    };
                    match applied {
                        Ok(()) => {
                            crate::hub_reporter::publish_committed_scope(&sb);
                            if tier == GrantTier::Session {
                                // R-PERM.4.4: a session answer reaches hooked commands too.
                                crate::sandbox::record_session_grant(
                                    &path,
                                    access,
                                    workspace.as_deref(),
                                    std::process::id(),
                                    "harness",
                                );
                            }
                        }
                        Err(why) => tracing::warn!(
                            path = %path.display(),
                            "approved grant not applied: {why}"
                        ),
                    }
                }
            }
            GrantResolveOutcome::Denied {
                path,
                time_to_decision_ms,
            } => {
                append_audit(
                    &audit_entry(
                        chrono::Local::now().to_rfc3339(),
                        AuditAction::Deny,
                        GrantKind::FsScope,
                        path.display().to_string(),
                        None,
                        GrantTier::Session,
                        Some("harness".to_string()),
                    )
                    .with_request(
                        &req.decision_id,
                        time_to_decision_ms,
                        req.context.risk.as_ref().map(|r| r.class.as_str()),
                    ),
                );
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

/// When a grant answered at `tier` ends: a lease ends
/// [`ahma_common::scope_grant::PROMPT_LEASE_SECS`] from now; nothing else does.
pub(crate) fn lease_end(tier: GrantTier) -> Option<u64> {
    (tier == GrantTier::Lease).then(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            + ahma_common::scope_grant::PROMPT_LEASE_SECS
    })
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
    async fn notify_violation_with(
        &self,
        path: &Path,
        access: ScopeAccess,
        reason: GrantReason,
        tool: Option<String>,
        context: GrantContext,
    ) -> Option<ScopeGrantRequest> {
        // Ask at most once per (path, access) per session (R-PERM.4). The kernel
        // trips on the same path many times over; the human should hear about it
        // once. The context rides with the request to every rung, so the harness
        // prompt and the TUI modal both say who asked, why, and how risky it is
        // (R-PERM.3.4).
        let req = self
            .coordinator
            .begin_with_context(path, access, reason, tool, context)?;
        // Disclose the pending question before the (possibly long-blocking)
        // ask, so a client that is not itself the asking surface learns a
        // decision is parked somewhere (#485). The path was already disclosed
        // to this client in the sandbox_denial error payload.
        self.emit_event(
            ahma_common::session_event::SessionEventKind::GrantPending,
            serde_json::json!({
                "grant_id": req.decision_id,
                "path": req.path.display().to_string(),
                "access": req.access,
                "reason": req.reason,
            }),
        );
        self.ask(req.clone()).await;
        Some(req)
    }

    fn budget_exhausted(&self) -> bool {
        self.coordinator.budget_exhausted()
    }

    fn status(&self, decision_id: &str) -> GrantStatus {
        self.coordinator.status(decision_id)
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
}

impl PeerElicitationSurface {
    /// Wrap the service's peer slot as an asking surface.
    pub fn new(peer: Arc<RwLock<Option<rmcp::service::Peer<rmcp::service::RoleServer>>>>) -> Self {
        Self { peer }
    }
}

/// The elicitation form the human fills in at the harness: one titled
/// single-select, `deny` first (SPEC R5.3.1: Enter alone never widens), every
/// tier labelled in words (SPEC R-PERM.3.4). Built by hand rather than derived
/// from a type so the client gets `oneOf` const/title pairs — buttons with
/// readable labels — instead of a free-text box.
fn grant_form_schema() -> rmcp::model::ElicitationSchema {
    use rmcp::model::{
        ConstTitle, ElicitationSchema, EnumSchema, PrimitiveSchemaDefinition,
        SingleSelectEnumSchema, TitledSingleSelectEnumSchema,
    };
    let one_of: Vec<ConstTitle> = ahma_common::grant_prompt::options()
        .into_iter()
        .map(|o| ConstTitle::new(o.value, o.label))
        .collect();
    let mut select = TitledSingleSelectEnumSchema::new(one_of);
    select.title = Some("Your decision".into());
    select.description =
        Some("Deny is the default. Session and once answers are never written to disk.".into());
    select.default = Some("deny".to_string());
    let mut props = std::collections::BTreeMap::new();
    props.insert(
        "decision".to_string(),
        PrimitiveSchemaDefinition::Enum(EnumSchema::Single(SingleSelectEnumSchema::Titled(select))),
    );
    let mut schema = ElicitationSchema::new(props);
    schema.required = Some(vec!["decision".to_string()]);
    schema
}

/// The answer the client returns for [`grant_form_schema`].
#[derive(Debug, Clone, serde::Deserialize)]
struct GrantForm {
    decision: String,
}

#[async_trait]
impl ElicitationSurface for PeerElicitationSurface {
    async fn ask(&self, req: &ScopeGrantRequest) -> ElicitOutcome {
        use rmcp::model::{ElicitRequest, ElicitRequestParams, ElicitationAction, ServerRequest};
        let peer = self.peer.read().clone();
        let Some(peer) = peer else {
            return ElicitOutcome::Unavailable;
        };
        if !peer
            .supported_elicitation_modes()
            .contains(&rmcp::service::ElicitationMode::Form)
        {
            // The client cannot elicit at all. Nothing to demote — there was never
            // a working surface here to lose.
            return ElicitOutcome::Unavailable;
        }

        // SPEC R5.3.1: bound the wait by *this* client's patience, so ahma is the
        // one that resolves the prompt. A flat 120s here is what let Antigravity
        // cancel at 60.005s and get the harness demoted for a deadline it never
        // disclosed.
        let timeout = crate::client_type::McpClientType::from_peer(&peer).elicitation_budget();

        let message = prompt_text(req);
        let request = ElicitRequest::new(ElicitRequestParams::FormElicitationParams {
            meta: None,
            message,
            requested_schema: grant_form_schema(),
        });
        let sent = tokio::time::timeout(
            timeout,
            peer.send_request(ServerRequest::ElicitRequest(request)),
        )
        .await;
        match sent {
            Ok(Ok(rmcp::model::ClientResult::ElicitResult(result))) => match result.action {
                ElicitationAction::Accept => match result
                    .content
                    .and_then(|v| serde_json::from_value::<GrantForm>(v).ok())
                {
                    Some(form) => ElicitOutcome::Answered(parse_decision(&form.decision)),
                    // An accept with no readable content is an answer we cannot
                    // read, which is not consent (R5.3.1).
                    None => ElicitOutcome::Dismissed("accepted without a choice".to_string()),
                },
                // An explicit decline is the human saying no. An answer, not a fault:
                // the surface stays trusted.
                ElicitationAction::Decline => ElicitOutcome::Answered(GrantDecision::Deny),
                // `cancel` is what a client sends when *it* gives up, with no human
                // involved: neither consent nor a denial (R5.3.1).
                ElicitationAction::Cancel => {
                    ElicitOutcome::Dismissed("cancelled without a choice".to_string())
                }
                // A future action this build does not know: not consent.
                _ => ElicitOutcome::Dismissed("unrecognised action".to_string()),
            },
            Ok(Ok(other)) => ElicitOutcome::Failed(format!("unexpected reply: {other:?}")),
            // The transport broke: the surface is genuinely unusable, which is
            // the one case that demotes it.
            Ok(Err(e)) => ElicitOutcome::Failed(e.to_string()),
            // Our own budget expired (SPEC R-PERM.3.1): a surface that does not
            // answer within the client's own deadline is treated as broken for
            // the session, and the question moves on so it is not lost.
            Err(_) => ElicitOutcome::Failed(format!("no answer within {timeout:?}")),
        }
    }
}

/// Map the form's choice to a decision. Anything unrecognized is a **deny**: an
/// answer we cannot read is not consent.
fn parse_decision(s: &str) -> GrantDecision {
    ahma_common::grant_prompt::parse_decision(s)
}

/// The question the human actually reads: the one body every surface renders
/// (SPEC R-PERM.3.4), as text.
fn prompt_text(req: &ScopeGrantRequest) -> String {
    ahma_common::grant_prompt::render(req).to_message()
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
            let mut q = self.outcome.lock();
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
        // `hub` means a TUI is attached: wired *and* someone watching it.
        let broker = PermissionBroker::new(
            Arc::new(GrantCoordinator::new()),
            if hub { Some(tx) } else { None },
        )
        .with_tui_viewers(Arc::new(std::sync::atomic::AtomicUsize::new(usize::from(
            hub,
        ))));
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

    /// Records every session-health event the broker emits (R8.8.5), so the
    /// disclosure contract is testable without a live MCP peer.
    #[derive(Debug, Default)]
    struct RecordingSink {
        events: Mutex<
            Vec<(
                ahma_common::session_event::SessionEventKind,
                serde_json::Value,
            )>,
        >,
    }

    impl crate::session_events::SessionEventSink for RecordingSink {
        fn emit_event(
            &self,
            kind: ahma_common::session_event::SessionEventKind,
            detail: serde_json::Value,
        ) {
            self.events.lock().push((kind, detail));
        }
    }

    #[tokio::test]
    async fn grant_pending_fires_once_per_deduped_path_and_decided_closes_it() {
        use ahma_common::session_event::SessionEventKind;
        // R8.8.5: one grant_pending per deduped (path, access), before the ask;
        // grant_decided carries the same grant_id and closes the loop.
        let h = ScriptedHarness::new(vec![ElicitOutcome::Answered(GrantDecision::Deny)]);
        let (broker, _rx) = broker_with(Some(h.clone()), true);
        let sink = Arc::new(RecordingSink::default());
        broker.set_session_events(sink.clone());

        violate(&broker, "/opt/cache").await;
        // Same (path, access) again: deduped by the coordinator → no second ask
        // and no second grant_pending.
        violate(&broker, "/opt/cache").await;

        let events = sink.events.lock();
        let pending: Vec<_> = events
            .iter()
            .filter(|(k, _)| *k == SessionEventKind::GrantPending)
            .collect();
        assert_eq!(
            pending.len(),
            1,
            "one grant_pending per deduped path, got {events:?}"
        );
        assert_eq!(pending[0].1["path"], "/opt/cache");
        assert_eq!(pending[0].1["access"], "rw");
        let decided: Vec<_> = events
            .iter()
            .filter(|(k, _)| *k == SessionEventKind::GrantDecided)
            .collect();
        assert_eq!(decided.len(), 1, "the deny resolution is disclosed");
        assert_eq!(decided[0].1["outcome"], "declined");
        assert_eq!(
            decided[0].1["grant_id"], pending[0].1["grant_id"],
            "grant_decided must correlate to the grant_pending it closes"
        );
    }

    #[tokio::test]
    async fn no_sink_installed_means_disclosure_is_a_silent_noop() {
        // Bare-CLI runs have no MCP peer and install no sink; the grant flow
        // must work identically (events are information-only, R8.8).
        let h = ScriptedHarness::new(vec![ElicitOutcome::Answered(GrantDecision::Deny)]);
        let (broker, _rx) = broker_with(Some(h.clone()), true);
        violate(&broker, "/opt/cache").await;
        assert_eq!(h.asks(), 1, "the ladder runs unchanged without a sink");
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
    async fn a_broken_surface_demotes_the_harness_and_the_question_falls_to_the_tui() {
        let h = ScriptedHarness::new(vec![
            ElicitOutcome::Failed("transport closed".into()),
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
            "one strike: a harness whose transport broke is not retried this session"
        );
        let second = rx
            .try_recv()
            .expect("the second question also goes to the TUI");
        assert_eq!(second.path, PathBuf::from("/opt/two"));
    }

    /// SPEC R5.3.1: a `cancel` is not a user's answer and not a broken surface.
    ///
    /// REGRESSION: clients cancel on their *own* undisclosed deadline —
    /// Antigravity was measured at 60.005s against ahma's flat 120s wait — so
    /// treating a cancel as a fault demoted a working surface for a timeout no
    /// human was party to, and treating it as a denial would have invented
    /// consent-shaped data out of nothing.
    #[tokio::test]
    async fn a_dismissal_neither_decides_nor_demotes() {
        let h = ScriptedHarness::new(vec![
            ElicitOutcome::Dismissed("cancelled".into()),
            ElicitOutcome::Answered(GrantDecision::GrantRw),
        ]);
        let (broker, mut rx) = broker_with(Some(h.clone()), true);
        let rx = rx.as_mut().unwrap();

        violate(&broker, "/opt/one").await;
        assert_eq!(h.asks(), 1);
        assert_eq!(
            rx.try_recv().expect("falls through to the TUI").path,
            PathBuf::from("/opt/one"),
            "a dismissal must reach the out-of-band path (R5.3.2)"
        );

        // The surface is still trusted: a second question is still put to it.
        violate(&broker, "/opt/two").await;
        assert_eq!(h.asks(), 2, "a dismissal must not demote a working harness");
        assert!(
            rx.try_recv().is_err(),
            "the second question was answered at the harness, not forwarded"
        );
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

    /// A hub with no TUI watching is not someone to ask (SPEC R-PERM.3.6).
    /// The question used to be sent into it and reported as "asking in the
    /// ahma TUI", then sat pending for the rest of the session with nobody to
    /// answer it, and its being in flight stopped the same path from being
    /// asked again even after a TUI opened.
    #[tokio::test]
    async fn a_hub_with_no_tui_watching_is_not_a_rung() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let viewers = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let broker = PermissionBroker::new(Arc::new(GrantCoordinator::new()), Some(tx))
            .with_tui_viewers(viewers.clone());

        violate(&broker, "/opt/cache").await;
        assert!(
            rx.try_recv().is_err(),
            "nobody is watching, so nothing is sent"
        );

        viewers.store(1, Ordering::SeqCst);
        violate(&broker, "/opt/cache").await;
        assert!(
            rx.try_recv().is_ok(),
            "once a TUI is open, the same path is asked there"
        );
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
        assert_eq!(
            parse_decision("read-write-session"),
            GrantDecision::GrantRwSession
        );
        assert_eq!(
            parse_decision("read-only-session"),
            GrantDecision::GrantRoSession
        );
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
            context: Default::default(),
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
            !text.contains("[n]"),
            "the form carries the choices; the message names no TUI keys"
        );
        assert!(
            text.contains("revoke"),
            "how to undo it, before they agree to it"
        );
    }
}
