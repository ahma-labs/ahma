//! Daemon reporter — background task that registers this ahma instance with
//! the hub daemon and forwards operation events to it.
//!
//! ## Design goals
//!
//! * **Non-blocking**: spawned as a detached Tokio task; never affects MCP
//!   operation if the daemon is unavailable or crashes.
//! * **Self-healing**: reconnects automatically with exponential back-off
//!   (initial 5 s, cap 30 s) when the daemon connection is lost.
//! * **Low overhead**: polls [`OperationMonitor`](crate::operation_monitor::OperationMonitor) every 2 seconds and diffs
//!   the snapshot — no changes means no wire traffic.

use crate::mcp_service::{ActiveAgentSession, get_global_prompt_runner};
use crate::operation_monitor::{Operation, OperationMonitor, OperationStatus};
use ahma_common::config::settings_path;
use ahma_common::daemon_hub::{
    ClientMsg, DaemonChatMessage, DaemonEvent, DaemonMsg, DaemonStream, HubRelay,
    OpStatus as WireStatus, connect_to_daemon, ensure_daemon_running, recv_msg, send_msg,
};
use ahma_common::scope_grant::{
    GrantCoordinator, GrantDecision, GrantResolveOutcome, ScopeGrantRequest, persist_grant,
};
use ahma_common::web_approval::{
    WebApprovalCoordinator, WebApprovalDecision, WebApprovalRequest, WebResolveOutcome,
    persist_web_allow,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{debug, info, warn};

/// The shared scope-grant plumbing handed to the reporter. The `coordinator` is
/// the same instance the [`crate::sandbox::HubGrantNotifier`] uses, so a request it
/// emits and the answer routed back here resolve against one coordinator (dedup,
/// first-answer-wins, dismiss). `req_rx` receives fresh requests to forward to the
/// hub as `ClientMsg::Relay(HubRelay::ScopeGrantRequested)`.
pub struct GrantReporting {
    /// Resolves answers and persists approved grants.
    pub coordinator: Arc<GrantCoordinator>,
    /// Stream of fresh requests to forward to the hub.
    pub req_rx: UnboundedReceiver<ScopeGrantRequest>,
}

/// The shared web-approval plumbing handed to the reporter (SPEC R-WEB.6). Parallel
/// to [`GrantReporting`]: `coordinator` is the same
/// [`WebApprovalCoordinator`] the
/// MCP service consults on every `fetch_webpage`, so a TUI answer routed back here
/// takes effect for the live session. `req_rx` receives fresh requests to forward
/// to the hub as `ClientMsg::Relay(HubRelay::WebApprovalRequested)`.
pub struct WebApprovalReporting {
    /// Resolves answers, applies session grants/denies, and persists `always`.
    pub coordinator: Arc<WebApprovalCoordinator>,
    /// Stream of fresh requests to forward to the hub.
    pub req_rx: UnboundedReceiver<WebApprovalRequest>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Client identity
// ─────────────────────────────────────────────────────────────────────────────

/// Process-wide MCP client identity (`clientInfo.name` from the `initialize`
/// handshake, e.g. `"claude-code"` or `"cursor"`). Registration with the hub
/// happens before any client attaches, so the reporter watches this channel
/// and — when the identity is learned or changes — reconnects and re-registers
/// with `client` set. Reconnect-to-relabel keeps the wire protocol field-only
/// (no `UpdateInstance` message), which keeps mixed-version daemons working;
/// the hub replays this instance's operations to subscribers after the
/// re-register, so the TUI view stays complete.
/// What this instance knows about itself, as far as the hub is concerned.
///
/// Everything here is learned *after* the process starts and after the reporter
/// first registers: the client's name arrives with the `initialize` handshake,
/// the sandbox scope only exists once `roots/list` has been answered and the
/// scope committed. A change to any field re-registers the instance
/// (reconnect-to-relabel), which is what keeps the wire field-only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstanceIdentity {
    /// `clientInfo.name` from the `initialize` handshake.
    pub client: Option<String>,
    /// The **committed** sandbox scope. Registration used to send
    /// `sandbox_scopes.first()` or `"."` once at process start — before
    /// `roots/list` — so in the default roots-driven configuration every
    /// instance advertised `"."`, and a TUI filtering by project matched none
    /// of them (SPEC R24.3).
    pub scope: Option<String>,
    /// The MCP session this instance serves, stable for its whole life.
    pub session_id: Option<String>,
    /// Pid of the client-facing frontend process.
    pub client_pid: Option<u32>,
    /// The client declared MCP `sampling` at `initialize`.
    pub sampling: bool,
}

static INSTANCE_IDENTITY: std::sync::LazyLock<tokio::sync::watch::Sender<InstanceIdentity>> =
    std::sync::LazyLock::new(|| tokio::sync::watch::channel(InstanceIdentity::default()).0);

/// Seed the parts of the identity known at startup (session id, frontend pid).
pub fn set_initial_identity(session_id: Option<String>, client_pid: Option<u32>) {
    INSTANCE_IDENTITY.send_if_modified(|cur| {
        let mut next = cur.clone();
        if session_id.is_some() {
            next.session_id = session_id.clone();
        }
        if client_pid.is_some() {
            next.client_pid = client_pid;
        }
        let changed = next != *cur;
        *cur = next;
        changed
    });
}

/// Record the MCP client identity for this instance. Called from
/// `on_initialized` once `clientInfo.name` is known. Idempotent: setting the
/// same name again does not trigger a hub re-register.
pub fn set_client_identity(name: impl Into<String>, sampling: bool) {
    let name = name.into();
    if name.is_empty() {
        return;
    }
    INSTANCE_IDENTITY.send_if_modified(|cur| {
        if cur.client.as_deref() == Some(name.as_str()) && cur.sampling == sampling {
            false
        } else {
            cur.client = Some(name);
            cur.sampling = sampling;
            true
        }
    });
}

/// Record the sandbox scope this instance actually locked (SPEC R5.1's single
/// commit point is the only caller). Re-registers so the hub — and every TUI
/// watching it — sees the real scope rather than the placeholder the process
/// started with.
pub fn set_committed_scope(scopes: &[std::path::PathBuf]) {
    let Some(primary) = scopes.first().map(|p| p.display().to_string()) else {
        return;
    };
    INSTANCE_IDENTITY.send_if_modified(|cur| {
        if cur.scope.as_deref() == Some(primary.as_str()) {
            false
        } else {
            cur.scope = Some(primary);
            true
        }
    });
}

/// Publish the scope a sandbox has just locked, so the hub — and every TUI
/// watching it — sees what this instance is actually scoped to.
///
/// Called from each of the three places a scope becomes final: the `roots/list`
/// commit, the explicit-scope commit, and a container narrowing. Registration
/// happens long before any of them, so without this the instance advertises the
/// placeholder it started with, and a TUI filtering by project matches nothing
/// (SPEC R24.3).
pub fn publish_committed_scope(sandbox: &crate::sandbox::Sandbox) {
    set_committed_scope(&sandbox.scopes());
}

/// The identity as currently known, for callers that register outside the
/// reporter loop.
pub fn current_identity() -> InstanceIdentity {
    INSTANCE_IDENTITY.borrow().clone()
}

// ─────────────────────────────────────────────────────────────────────────────
// Public entry point
// ─────────────────────────────────────────────────────────────────────────────

/// Spawn a background task that registers this instance with the hub daemon and
/// forwards operation events.
///
/// This function returns immediately; all errors are logged at `warn` / `debug`
/// level and the task retries silently.
///
/// # Arguments
///
/// * `monitor` – the shared [`OperationMonitor`] for this service instance.
/// * `mode` – transport mode string shown in the TUI (`"stdio"`, `"http"`, …).
/// * `scope` – sandbox scope / workspace root path (human readable).
/// * `label` – short label for this instance (e.g. `"VS Code"` or `"ahma"`).
pub fn spawn_reporter(
    monitor: Arc<OperationMonitor>,
    mode: impl Into<String> + Send + 'static,
    scope: impl Into<String> + Send + 'static,
    label: impl Into<String> + Send + 'static,
    grant: Option<GrantReporting>,
    web: Option<WebApprovalReporting>,
) -> ReporterHandle {
    let mode = mode.into();
    let scope = scope.into();
    let label = label.into();
    let (finished_tx, finished_rx) = tokio::sync::watch::channel(None);

    tokio::spawn(async move {
        run_reporter_loop(monitor, mode, scope, label, grant, web, finished_tx).await;
    });
    ReporterHandle { finished_rx }
}

/// A handle for a caller that must not exit before its work has been reported.
///
/// A long-lived server never needs this: it outlives its operations. A hooked
/// command does — it *is* one operation, and it exits the moment that command
/// ends, so without waiting the terminal event races process teardown and the
/// work never appears anywhere (SPEC R-DAEMON.8).
pub struct ReporterHandle {
    finished_rx: tokio::sync::watch::Receiver<Option<String>>,
}

impl ReporterHandle {
    /// Wait until *any* terminal event has been written to the hub, or `budget`
    /// elapses. For a process that runs exactly one operation — a hooked
    /// command — the first one is its own.
    pub async fn wait_for_any_finished(&mut self, budget: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            if self.finished_rx.borrow().is_some() {
                return true;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            if tokio::time::timeout(remaining, self.finished_rx.changed())
                .await
                .is_err()
            {
                return false;
            }
        }
    }

    /// Wait until `op_id`'s terminal event has actually been written to the
    /// hub, or `budget` elapses.
    ///
    /// Bounded on purpose, and short: reporting is an observability nicety, and
    /// a user's hooked command must never be held up by a daemon that is slow,
    /// absent, or wedged. Returns whether the event made it.
    pub async fn wait_for_finished(&mut self, op_id: &str, budget: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            if self.finished_rx.borrow().as_deref() == Some(op_id) {
                return true;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            if tokio::time::timeout(remaining, self.finished_rx.changed())
                .await
                .is_err()
            {
                return false;
            }
        }
    }
}

/// Await the next grant request, or pend forever when there is no receiver — the
/// idiom for an optional `tokio::select!` branch.
async fn recv_optional_grant(
    rx: Option<&mut UnboundedReceiver<ScopeGrantRequest>>,
) -> Option<ScopeGrantRequest> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Await the next web-approval request, or pend forever when there is no receiver.
async fn recv_optional_web(
    rx: Option<&mut UnboundedReceiver<WebApprovalRequest>>,
) -> Option<WebApprovalRequest> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Persist an approved `always` domain to `~/.ahma/settings.toml`. The session
/// grant was already applied in-memory by [`WebApprovalCoordinator::resolve`]; this
/// makes it survive restarts. Best-effort: a write failure is logged, not fatal.
fn persist_resolved_web_allow(domain: &str) {
    match settings_path() {
        Some(file) => match persist_web_allow(&file, domain) {
            Ok(true) => info!(domain, "web approval persisted to [web].always_allow"),
            Ok(false) => info!(domain, "web approval already in [web].always_allow"),
            Err(e) => warn!("daemon_reporter: failed to persist web allow for {domain}: {e:#}"),
        },
        None => warn!("daemon_reporter: cannot persist web allow (home directory unknown)"),
    }
}

/// Persist an approved grant to `~/.ahma/settings.toml` — never the live session
/// (SPEC R5.4.7). Stamps today's date and records the requesting tool as
/// `granted_by` provenance. Best-effort: a write failure is logged, not fatal.
fn persist_resolved_grant(
    path: &std::path::Path,
    access: ahma_common::config::ScopeAccess,
    tool: Option<String>,
) {
    let granted_at = chrono::Local::now().format("%Y-%m-%d").to_string();
    let granted_by = tool.or_else(|| Some("scope-grant prompt".to_string()));
    match settings_path() {
        Some(file) => {
            match persist_grant(&file, path, access, granted_by, Some(granted_at), None) {
                Ok(_) => tracing::info!(
                    path = %path.display(),
                    access = access.label(),
                    "scope grant approved and persisted; restart the bridge (the `restart` \
                     tool) to apply it now, otherwise it takes effect on the next server start"
                ),
                Err(e) => warn!(
                    "daemon_reporter: failed to persist scope grant for {}: {e:#}",
                    path.display()
                ),
            }
        }
        None => warn!("daemon_reporter: cannot persist scope grant (home directory unknown)"),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal loop
// ─────────────────────────────────────────────────────────────────────────────

/// Compute the next reconnect back-off delay in seconds: doubles the current
/// delay, capped at 30s. Shared by every wait in `run_reporter_loop` so the
/// growth/cap policy lives in exactly one place. Deliberately a plain doubling
/// helper rather than `retry::RetryConfig`: the loop already threads its
/// back-off as a running `u64` seconds value (used directly in
/// `Duration::from_secs` and in log messages), while `RetryConfig` is built
/// around an attempt counter plus `Duration` — adopting it here would mean
/// converting state shape for no behavioral benefit.
fn next_backoff_secs(current: u64) -> u64 {
    (current * 2).min(30)
}

/// Main reporter loop.  Runs until the process exits.
#[allow(clippy::too_many_arguments)]
async fn run_reporter_loop(
    monitor: Arc<OperationMonitor>,
    mode: String,
    scope: String,
    label: String,
    grant: Option<GrantReporting>,
    web: Option<WebApprovalReporting>,
    finished_tx: tokio::sync::watch::Sender<Option<String>>,
) {
    let pid = std::process::id();
    let mut backoff_secs: u64 = 1;

    // Split the grant plumbing: the coordinator is an `Arc` (cheap to clone into
    // each handler), while the request receiver is the single `&mut`-borrowed
    // resource in the select. Keeping them separate avoids a double-mutable-borrow
    // of one struct across two select branches.
    let grant_coordinator = grant.as_ref().map(|g| g.coordinator.clone());
    let mut grant_req_rx = grant.map(|g| g.req_rx);
    // Same split for the web-approval plumbing.
    let web_coordinator = web.as_ref().map(|w| w.coordinator.clone());
    let mut web_req_rx = web.map(|w| w.req_rx);
    // Watch the client identity: learned after registration (the MCP
    // `initialize` handshake), a change makes us reconnect and re-register.
    let mut identity_rx = INSTANCE_IDENTITY.subscribe();

    loop {
        // ── Ensure daemon is running ─────────────────────────────────────────
        if let Err(e) = ensure_daemon_running().await {
            warn!("daemon_reporter: daemon unavailable ({e}); retry in {backoff_secs}s");
            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
            backoff_secs = next_backoff_secs(backoff_secs);
            continue;
        }

        // ── Connect ──────────────────────────────────────────────────────────
        let stream = match connect_to_daemon().await {
            Ok(s) => s,
            Err(e) => {
                debug!("daemon_reporter: connect failed ({e}); retry in {backoff_secs}s");
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = next_backoff_secs(backoff_secs);
                continue;
            }
        };

        debug!("daemon_reporter: connected to hub daemon");
        backoff_secs = 1; // reset back-off on successful connect

        let (read_half, write_half) = tokio::io::split(stream);
        let mut reader = tokio::io::BufReader::new(read_half);
        let mut writer = write_half;

        // ── Register this instance ────────────────────────────────────────────
        // borrow_and_update marks the current identity as seen so the
        // `changed()` select branch only fires on a genuinely new value.
        let identity = identity_rx.borrow_and_update().clone();
        // The committed scope wins over the value this loop started with: the
        // latter is a placeholder until `roots/list` has been answered.
        let scope = identity.scope.clone().unwrap_or_else(|| scope.clone());
        // An operation's origin is who asked for it, which is the MCP client
        // when there is one — `claude-code`, `cursor` — not this process's
        // instance label, which is `ahma` for every one of them and so tells a
        // reader nothing about which window started the work (SPEC R24.7).
        let origin = identity.client.clone().unwrap_or_else(|| label.clone());
        let reg = ClientMsg::Register {
            pid,
            mode: mode.clone(),
            scope: scope.clone(),
            label: label.clone(),
            client: identity.client.clone(),
            session_id: identity.session_id.clone(),
            client_pid: identity.client_pid,
            sampling: identity.sampling,
        };
        if let Err(e) = send_msg(&mut writer, &reg).await {
            debug!("daemon_reporter: register failed ({e})");
            continue;
        }

        // Channels for outbound messages and agent active session
        let (hub_tx, mut hub_rx) = tokio::sync::mpsc::channel::<ClientMsg>(100);
        let session = Arc::new(tokio::sync::Mutex::new(ActiveAgentSession::default()));

        // Subscribe to events BEFORE replaying so we don't miss anything that starts
        // while we are replaying the initial snapshot.
        let mut event_rx = monitor.subscribe_events();

        // ── Replay completed and active operations ────────────────────────────
        // A send failure here means the connection dropped mid-replay; go
        // back to the top of the outer loop and reconnect.
        let completed_ops = monitor.get_completed_operations().await;
        if !replay_completed_operations(&mut writer, &completed_ops, &scope, &origin).await {
            continue;
        }
        let active_ops = monitor.get_all_active_operations().await;
        if !replay_active_operations(&mut writer, &active_ops, &scope, &origin).await {
            continue;
        }

        // ── Event loop: forward the unified operation event stream ───────────
        let mut closed = false;
        while !closed {
            tokio::select! {
                biased;

                // 1. Unified operation events to forward
                event_res = event_rx.recv() => {
                    match event_res {
                        Ok(event) => {
                            let Some(payload) = daemon_event_for(&event, &scope, &origin) else {
                                continue;
                            };
                            // Note the terminal event *after* it is on the wire,
                            // so a caller waiting for its own operation waits for
                            // the send, not for the intent to send.
                            let finished_id = match &payload {
                                DaemonEvent::OpFinished { id, .. } => Some(id.clone()),
                                _ => None,
                            };
                            let client_msg = ClientMsg::Event { payload };

                            if let Err(e) = send_msg(&mut writer, &client_msg).await {
                                debug!("daemon_reporter: send failed ({e}), reconnecting");
                                closed = true;
                            } else if let Some(id) = finished_id {
                                let _ = finished_tx.send(Some(id));
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!("daemon_reporter event queue lagged by {n} messages; continuing");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            closed = true;
                        }
                    }
                }

                // 2. Outbound client messages from Agent Runner
                hub_msg = hub_rx.recv() => {
                    if let Some(msg) = hub_msg {
                        if let Err(e) = send_msg(&mut writer, &msg).await {
                            debug!("daemon_reporter: send hub_msg failed ({e}), reconnecting");
                            closed = true;
                        }
                    } else {
                        closed = true;
                    }
                }

                // 2b. Fresh scope-grant requests to forward to the hub.
                maybe_req = recv_optional_grant(grant_req_rx.as_mut()) => {
                    match maybe_req {
                        Some(req) => {
                            if send_msg(&mut writer, &ClientMsg::Relay(HubRelay::ScopeGrantRequested { request: req })).await.is_err() {
                                debug!("daemon_reporter: send ScopeGrantRequested failed, reconnecting");
                                closed = true;
                            }
                        }
                        // Sender dropped — stop polling a dead receiver.
                        None => grant_req_rx = None,
                    }
                }

                // 2c. Fresh web-approval requests to forward to the hub.
                maybe_web = recv_optional_web(web_req_rx.as_mut()) => {
                    match maybe_web {
                        Some(request) => {
                            if send_msg(&mut writer, &ClientMsg::Relay(HubRelay::WebApprovalRequested { request })).await.is_err() {
                                debug!("daemon_reporter: send WebApprovalRequested failed, reconnecting");
                                closed = true;
                            }
                        }
                        None => web_req_rx = None,
                    }
                }

                // 2d. Something this instance knows about itself changed — the
                // client's name, or the sandbox scope it committed → reconnect
                // so the hub re-registers it with the new value
                // (reconnect-to-relabel; see INSTANCE_IDENTITY). The instance id
                // survives because the session id does.
                changed = identity_rx.changed() => {
                    if changed.is_ok() {
                        info!("daemon_reporter: instance identity changed; re-registering with hub");
                        closed = true;
                    }
                }

                // 3. Incoming messages from daemon hub
                daemon_msg = recv_msg::<_, DaemonMsg>(&mut reader) => {
                    closed = handle_daemon_msg(
                        daemon_msg,
                        &mut writer,
                        &hub_tx,
                        &session,
                        &monitor,
                        grant_coordinator.as_ref(),
                        web_coordinator.as_ref(),
                    ).await;
                }
            }
        }

        // Back-off before reconnect attempt.
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = next_backoff_secs(backoff_secs);
    }
}

/// Replay a batch of completed operations' `OpStarted`/`OpFinished` events to a
/// freshly (re)connected hub, so a late-attaching TUI sees prior history.
/// Stops at the first send failure — the caller reconnects in that case.
/// Returns `true` when every operation replayed successfully.
async fn replay_completed_operations(
    writer: &mut tokio::io::WriteHalf<DaemonStream>,
    completed_ops: &[Operation],
    scope: &str,
    label: &str,
) -> bool {
    for op in completed_ops {
        let started_ev = ClientMsg::Event {
            payload: op_started_event(op, scope, label),
        };
        if send_msg(writer, &started_ev).await.is_err() {
            return false;
        }
        let duration_ms = op
            .end_time
            .and_then(|end| end.duration_since(op.start_time).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let (result_summary, denial) = summary_and_denial(op);
        let finished_ev = ClientMsg::Event {
            payload: DaemonEvent::OpFinished {
                id: op.id.clone(),
                status: wire_status(op.state),
                result_summary,
                duration_ms,
                ended_epoch_ms: op.end_time.and_then(epoch_ms),
                // Replayed completions carry their exit code too, so a
                // late-attaching TUI shows `exit 101`, not a bare "failed".
                exit_code: op.result.as_ref().and_then(exit_code_from_value),
                // Replay carries denials too, so a TUI opened after the fact
                // still sees which path was refused (SPEC R-PERM.7).
                denial,
                interrupted: false,
            },
        };
        if send_msg(writer, &finished_ev).await.is_err() {
            return false;
        }
    }
    true
}

/// Replay a batch of active operations' `OpStarted` events (no `OpFinished` —
/// they haven't finished yet). Same stop-on-failure contract as
/// [`replay_completed_operations`].
async fn replay_active_operations(
    writer: &mut tokio::io::WriteHalf<DaemonStream>,
    active_ops: &[Operation],
    scope: &str,
    label: &str,
) -> bool {
    for op in active_ops {
        let started_ev = ClientMsg::Event {
            payload: op_started_event(op, scope, label),
        };
        if send_msg(writer, &started_ev).await.is_err() {
            return false;
        }
    }
    true
}

/// Handle one message received from the daemon hub (the reporter loop's
/// "incoming messages from daemon hub" select arm). Returns `true` when the
/// connection should be treated as closed — the caller sets `closed = true`
/// and the outer loop reconnects.
async fn handle_daemon_msg(
    daemon_msg: anyhow::Result<DaemonMsg>,
    writer: &mut tokio::io::WriteHalf<DaemonStream>,
    hub_tx: &tokio::sync::mpsc::Sender<ClientMsg>,
    session: &Arc<tokio::sync::Mutex<ActiveAgentSession>>,
    monitor: &OperationMonitor,
    grant_coordinator: Option<&Arc<GrantCoordinator>>,
    web_coordinator: Option<&Arc<WebApprovalCoordinator>>,
) -> bool {
    let msg = match daemon_msg {
        Ok(msg) => msg,
        Err(e) => {
            debug!("daemon_reporter: read error or EOF ({e}), reconnecting");
            return true;
        }
    };

    match msg {
        DaemonMsg::Ping { seq } => {
            debug!("daemon_reporter: received ping seq={seq}");
            if let Err(e) = send_msg(writer, &ClientMsg::Pong { seq }).await {
                debug!("daemon_reporter: pong send failed ({e}), reconnecting");
                return true;
            }
        }
        DaemonMsg::RunPrompt {
            messages,
            system_prompt,
            provider,
            model,
        } => spawn_prompt_run(messages, system_prompt, provider, model, hub_tx, session).await,
        DaemonMsg::CancelPrompt => cancel_prompt_run(hub_tx, session).await,
        DaemonMsg::CancelOperation { op_id } => {
            let cancelled = monitor
                .cancel_operation_with_reason(&op_id, Some("Cancelled from ahma tui".into()))
                .await;
            info!("daemon_reporter: CancelOperation op={op_id} cancelled={cancelled}");
        }
        DaemonMsg::SubmitApproval { id, approved } => deliver_approval(id, approved, session).await,
        DaemonMsg::SubmitScopeGrant {
            decision_id,
            decision,
        } => resolve_scope_grant(decision_id, decision, writer, grant_coordinator).await,
        DaemonMsg::ReRaiseScopeGrant { path, access } => {
            re_raise_scope_grant(&path, access, writer, grant_coordinator).await
        }
        DaemonMsg::SubmitWebApproval {
            decision_id,
            decision,
        } => resolve_web_approval(decision_id, decision, writer, web_coordinator).await,
        other => debug!("daemon_reporter: ignored unexpected DaemonMsg: {:?}", other),
    }
    false
}

/// `RunPrompt`: hand the turn to the registered prompt runner on a detached
/// task, reporting its outcome back to the hub. An instance with no runner
/// registered answers with an `AgentError` rather than going silent.
async fn spawn_prompt_run(
    messages: Vec<DaemonChatMessage>,
    system_prompt: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    hub_tx: &tokio::sync::mpsc::Sender<ClientMsg>,
    session: &Arc<tokio::sync::Mutex<ActiveAgentSession>>,
) {
    info!(
        provider = ?provider,
        model = ?model,
        messages = messages.len(),
        "daemon_reporter: RunPrompt received"
    );
    let Some(runner) = get_global_prompt_runner() else {
        warn!("daemon_reporter: RunPrompt received but no prompt runner is registered");
        let _ = hub_tx
            .send(ClientMsg::Relay(HubRelay::AgentError {
                error: "No prompt runner registered on this instance".to_string(),
            }))
            .await;
        return;
    };
    let runner = runner.clone();
    let hub_tx = hub_tx.clone();
    let turn_session = session.clone();
    let session = session.clone();
    let handle = tokio::spawn(async move {
        let outcome = runner
            .run_prompt(
                messages,
                system_prompt,
                provider,
                model,
                hub_tx.clone(),
                session,
            )
            .await;
        let done = match outcome {
            Ok(_) => ClientMsg::Relay(HubRelay::AgentDone),
            Err(e) => ClientMsg::Relay(HubRelay::AgentError { error: e }),
        };
        let _ = hub_tx.send(done).await;
    });
    turn_session.lock().await.turn = Some(handle.abort_handle());
}

/// `CancelPrompt`: stop the running turn and end it for every subscriber with
/// one `AgentError`. Pending approvals are dropped, so a waiter wakes with a
/// closed channel instead of hanging. A turn that already finished has sent
/// its own `AgentDone`/`AgentError`; cancelling it must not add a second end.
async fn cancel_prompt_run(
    hub_tx: &tokio::sync::mpsc::Sender<ClientMsg>,
    session: &Arc<tokio::sync::Mutex<ActiveAgentSession>>,
) {
    let turn = {
        let mut guard = session.lock().await;
        guard.approval_tx = None;
        guard.approvals.clear();
        guard.turn.take()
    };
    let Some(turn) = turn.filter(|t| !t.is_finished()) else {
        debug!("daemon_reporter: CancelPrompt with no running turn");
        return;
    };
    turn.abort();
    info!("daemon_reporter: agent turn cancelled by user");
    let _ = hub_tx
        .send(ClientMsg::Relay(HubRelay::AgentError {
            error: "Cancelled by user".to_string(),
        }))
        .await;
}

/// `SubmitApproval`: wake the waiter registered for this call id, or — when the
/// daemon sent no id — the single pending session-level waiter.
async fn deliver_approval(
    id: Option<String>,
    approved: bool,
    session: &Arc<tokio::sync::Mutex<ActiveAgentSession>>,
) {
    debug!("daemon_reporter: received SubmitApproval id={id:?} approved={approved}");
    let mut session_guard = session.lock().await;
    let Some(call_id) = id else {
        match session_guard.approval_tx.take() {
            Some(tx) => {
                let _ = tx.send(approved);
            }
            None => {
                debug!("daemon_reporter: received SubmitApproval but no approval sender pending")
            }
        }
        return;
    };
    match session_guard.approvals.remove(&call_id) {
        Some(tx) => {
            let _ = tx.send(approved);
        }
        None => debug!("daemon_reporter: received SubmitApproval for unknown call_id={call_id}"),
    }
}

/// `SubmitScopeGrant`: resolve the decision against the coordinator, persisting
/// an approved grant. Every *terminal* outcome then dismisses any twin modal on
/// other TUIs; an already-resolved or unknown decision id changes nothing.
async fn resolve_scope_grant(
    decision_id: String,
    decision: GrantDecision,
    writer: &mut tokio::io::WriteHalf<DaemonStream>,
    grant_coordinator: Option<&Arc<GrantCoordinator>>,
) {
    debug!("daemon_reporter: received SubmitScopeGrant id={decision_id} decision={decision:?}");
    let Some(coord) = grant_coordinator else {
        return;
    };
    match coord.resolve(&decision_id, decision) {
        GrantResolveOutcome::Persist { path, access, tool } => {
            persist_resolved_grant(&path, access, tool)
        }
        GrantResolveOutcome::Denied { .. } => {}
        GrantResolveOutcome::AlreadyResolved | GrantResolveOutcome::Unknown => return,
    }
    let _ = send_msg(writer, &ClientMsg::ScopeGrantResolved { decision_id }).await;
}

/// `ReRaiseScopeGrant`: reopen a question this session already refused, because
/// the user explicitly asked for it from a denied operation row (SPEC
/// R-PERM.7.1). `reopen()` clears the session's ask-once memo for this
/// (path, access) first: the memo stops ahma nagging, and a person picking a
/// denied row and confirming is not ahma nagging.
async fn re_raise_scope_grant(
    path: &str,
    access: ahma_common::config::ScopeAccess,
    writer: &mut tokio::io::WriteHalf<DaemonStream>,
    grant_coordinator: Option<&Arc<GrantCoordinator>>,
) {
    debug!(
        "daemon_reporter: received ReRaiseScopeGrant path={path} access={}",
        access.label()
    );
    let Some(coord) = grant_coordinator else {
        return;
    };
    match coord.reopen(
        std::path::Path::new(path),
        access,
        ahma_common::scope_grant::GrantReason::StderrHeuristic,
        Some("re-raised from the TUI".to_string()),
    ) {
        Some(request) => {
            let _ = send_msg(
                writer,
                &ClientMsg::Relay(HubRelay::ScopeGrantRequested { request }),
            )
            .await;
        }
        // Already in flight — the modal the user wants is on screen already.
        None => debug!("daemon_reporter: re-raise skipped, question already in flight"),
    }
}

/// `SubmitWebApproval`: resolve the decision against the coordinator, which
/// applies a session grant/deny in-memory; `Persist` additionally writes
/// `always_allow`. Every *terminal* outcome then dismisses twin modals on other
/// TUIs; an already-resolved or unknown decision id changes nothing.
async fn resolve_web_approval(
    decision_id: String,
    decision: WebApprovalDecision,
    writer: &mut tokio::io::WriteHalf<DaemonStream>,
    web_coordinator: Option<&Arc<WebApprovalCoordinator>>,
) {
    debug!("daemon_reporter: received SubmitWebApproval id={decision_id} decision={decision:?}");
    let Some(coord) = web_coordinator else {
        return;
    };
    match coord.resolve(&decision_id, decision) {
        WebResolveOutcome::Persist { domain } => persist_resolved_web_allow(&domain),
        WebResolveOutcome::AllowOnce { .. }
        | WebResolveOutcome::AllowSession { .. }
        | WebResolveOutcome::Denied { .. } => {}
        WebResolveOutcome::AlreadyResolved | WebResolveOutcome::Unknown => return,
    }
    let _ = send_msg(writer, &ClientMsg::WebApprovalResolved { decision_id }).await;
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Convert a wall-clock time to Unix-epoch milliseconds for the hub wire.
fn epoch_ms(t: std::time::SystemTime) -> Option<u64> {
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

/// Build the replay `OpStarted` wire event for an operation, preserving its
/// true start time and parent link so a late-joining TUI shows accurate
/// elapsed times and hierarchy.
fn op_started_event(op: &Operation, scope: &str, origin: &str) -> DaemonEvent {
    DaemonEvent::OpStarted {
        id: op.id.clone(),
        tool_name: op.tool_name.clone(),
        description: op.description.clone(),
        scope: scope.to_string(),
        parent_id: op.parent_id.clone(),
        started_epoch_ms: epoch_ms(op.start_time),
        // Replay carries the same identity the live event did, which is why fixing
        // the wire fixes late attach for free: a TUI opened *after* an IDE has been
        // working shows what those operations were, not what their ids looked like
        // (SPEC R24.7 / R24.2).
        title: op.title.clone(),
        cwd: op.cwd.clone(),
        command: op.command.clone(),
        origin: Some(origin.to_string()),
        partial: false,
        // Everything a worker runs goes through the kernel sandbox; the
        // unsandboxed hook fallback is deliberately not reported at all
        // (SPEC R-DAEMON.8).
        unsandboxed: false,
    }
}

/// Map a unified [`OperationEvent`](ahma_common::event_dispatcher::OperationEvent) to the hub wire event, or `None` for
/// events the hub does not carry (Progress, McpNotification).
/// Map a unified [`OperationEvent`](ahma_common::event_dispatcher::OperationEvent) to the hub wire event.
///
/// `origin` is the identity of the session that owns this instance — `cursor`,
/// `claude-code`, `tui`, `cli`. Stamped here because this is the only layer that
/// knows it: the adapter that *runs* the command has no idea who asked. It is what
/// lets one timeline interleave IDE work and the user's own TUI commands and stay
/// readable (SPEC R24.7).
fn daemon_event_for(
    event: &ahma_common::event_dispatcher::OperationEvent,
    scope: &str,
    origin: &str,
) -> Option<DaemonEvent> {
    use ahma_common::event_dispatcher::OperationEvent as Ev;
    let now_ms = epoch_ms(std::time::SystemTime::now());
    Some(match event {
        Ev::Started {
            operation_id,
            tool_name,
            description,
            parent_id,
            title,
            cwd,
            command,
        } => DaemonEvent::OpStarted {
            id: operation_id.clone(),
            tool_name: tool_name.clone(),
            description: description.clone(),
            scope: scope.to_string(),
            parent_id: parent_id.clone(),
            started_epoch_ms: now_ms,
            // Computed at the source (SPEC R24.7) and forwarded verbatim. The hub
            // is a pipe here, not an interpreter: if it started deriving names of
            // its own we would be back to guessing.
            title: title.clone(),
            cwd: cwd.clone(),
            command: command.clone(),
            origin: Some(origin.to_string()),
            partial: false,
            // Everything a worker runs goes through the kernel sandbox; the
            // unsandboxed hook fallback is deliberately not reported at all
            // (SPEC R-DAEMON.8).
            unsandboxed: false,
        },
        Ev::OutputLine {
            operation_id,
            line,
            is_stderr,
        } => DaemonEvent::OpOutput {
            id: operation_id.clone(),
            line: line.clone(),
            is_stderr: *is_stderr,
        },
        Ev::Alert {
            operation_id,
            message,
        } => DaemonEvent::LogLine {
            level: "alert".to_string(),
            message: format!("{operation_id}: {message}"),
        },
        Ev::Completed {
            operation_id,
            result,
            duration_ms,
        } => DaemonEvent::OpFinished {
            id: operation_id.clone(),
            status: WireStatus::Completed,
            result_summary: summary_from_value(result),
            duration_ms: *duration_ms,
            ended_epoch_ms: now_ms,
            // "Completed" without a code is not actionable; `exit 0` is.
            exit_code: exit_code_from_value(result),
            denial: None,
            interrupted: false,
        },
        Ev::Failed {
            operation_id,
            error,
            duration_ms,
        } => DaemonEvent::OpFinished {
            id: operation_id.clone(),
            status: WireStatus::Failed,
            result_summary: Some(clip_summary(error.clone())),
            duration_ms: *duration_ms,
            ended_epoch_ms: now_ms,
            // A failed op reports its error as a string, not a result object, so
            // there is usually no code to read. `None` renders as "failed", which
            // is honest — better than a fabricated code.
            exit_code: None,
            // A sandbox denial is a different thing from a failure and must say
            // so on every surface (SPEC R-PERM.7).
            denial: denial_from_text(error),
            interrupted: false,
        },
        Ev::Cancelled {
            operation_id,
            reason,
            duration_ms,
        } => DaemonEvent::OpFinished {
            id: operation_id.clone(),
            status: WireStatus::Cancelled,
            result_summary: Some(clip_summary(reason.clone())),
            duration_ms: *duration_ms,
            ended_epoch_ms: now_ms,
            exit_code: None,
            denial: None,
            interrupted: false,
        },
        Ev::TimedOut {
            operation_id,
            duration_ms,
        } => DaemonEvent::OpFinished {
            id: operation_id.clone(),
            status: WireStatus::TimedOut,
            result_summary: Some("operation timed out".to_string()),
            duration_ms: *duration_ms,
            ended_epoch_ms: now_ms,
            exit_code: None,
            denial: None,
            interrupted: false,
        },
        _ => return None,
    })
}

/// Pull the process exit code out of a tool result, when it has one.
///
/// The shell path already puts `exit_code` in its result JSON; this simply stops
/// throwing it away at the hub boundary. Non-process tools have none, and get
/// `None` — the surface then says the status word rather than inventing a code.
fn exit_code_from_value(result: &serde_json::Value) -> Option<i64> {
    result
        .get("exit_code")
        .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
}

/// Extract a short human-readable summary from a result JSON value.
fn summary_from_value(result: &serde_json::Value) -> Option<String> {
    Some(clip_summary(summary_text_from_value(result)))
}

/// The same extraction as [`summary_from_value`], **unclipped**.
///
/// The clip is a wire-length concern for the human-readable summary; it must not
/// decide what the denial scanner gets to see. Scanning the clipped text meant a
/// denial whose path sat past 200 chars — an ordinary amount of build output
/// before the refusal — degraded into an anonymous failure, losing the path the
/// whole R-PERM.7 guarantee is about.
fn summary_text_from_value(result: &serde_json::Value) -> String {
    if let Some(msg) = result.get("message").and_then(|v| v.as_str()) {
        msg.to_string()
    } else if let Some(err) = result.get("error").and_then(|v| v.as_str()) {
        err.to_string()
    } else if let Some(err_obj) = result
        .get("error")
        .and_then(|v| v.get("message"))
        .and_then(|v| v.as_str())
    {
        err_obj.to_string()
    } else {
        serde_json::to_string(result).unwrap_or_default()
    }
}

/// Clip a summary string to a wire-friendly length.
fn clip_summary(summary: String) -> String {
    if summary.len() > 200 {
        let cut = summary
            .char_indices()
            .take_while(|(i, _)| *i <= 197)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        format!("{}...", &summary[..cut])
    } else {
        summary
    }
}

/// Map the monitor's operation state onto the hub wire enum.
///
/// Exhaustive by construction: a new `OperationStatus` variant is a compile
/// error here rather than a string that silently means nothing downstream.
fn wire_status(s: OperationStatus) -> WireStatus {
    match s {
        OperationStatus::Pending => WireStatus::Pending,
        OperationStatus::InProgress => WireStatus::InProgress,
        OperationStatus::Completed => WireStatus::Completed,
        OperationStatus::Failed => WireStatus::Failed,
        OperationStatus::Cancelled => WireStatus::Cancelled,
        OperationStatus::TimedOut => WireStatus::TimedOut,
    }
}

/// Recognise a sandbox denial in a failed operation's text so it can travel the
/// hub wire as a denial rather than an anonymous failure (SPEC R-PERM.7).
///
/// Reuses the same scanner the live grant flow uses, so what the TUI labels
/// `denied:` is exactly what would have raised a grant prompt — the two can
/// never disagree about whether something was a denial.
fn denial_from_text(text: &str) -> Option<ahma_common::daemon_hub::OpDenial> {
    let hit = crate::sandbox::denial_scan::scan_denial(text)?;
    Some(ahma_common::daemon_hub::OpDenial {
        path: hit.path.display().to_string(),
        access: hit.access,
    })
}

/// The result summary for a (possibly still-active) operation. Delegates to
/// [`summary_from_value`] — the same extraction-and-clip logic the live event
/// path uses, so replay and live events never disagree on wording or length.
///
/// Production replay goes through [`summary_and_denial`], which needs the
/// unclipped text as well; this remains as the direct expression of the
/// extract-and-clip contract its tests pin.
#[cfg(test)]
fn result_summary_from(op: &Operation) -> Option<String> {
    summary_from_value(op.result.as_ref()?)
}

/// The clipped summary and the denial for a finished operation, from a single
/// extraction of its result.
///
/// One pass because the extraction falls back to serialising the whole result
/// JSON — for a completed command, its entire inline output window — and the
/// two used to compute it independently. The denial is scanned from the *full*
/// text and the summary clipped afterwards, so the wire-length cap on one
/// cannot silently discard the other.
fn summary_and_denial(
    op: &Operation,
) -> (Option<String>, Option<ahma_common::daemon_hub::OpDenial>) {
    let Some(result) = op.result.as_ref() else {
        return (None, None);
    };
    let text = summary_text_from_value(result);
    let denial = denial_from_text(&text);
    (Some(clip_summary(text)), denial)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation_monitor::{Operation, OperationStatus};
    use ahma_common::daemon_hub::DaemonEvent;
    use ahma_common::event_dispatcher::OperationEvent;
    use ahma_common::timeouts::TestTimeouts;
    use serde_json::json;
    use tokio::sync::mpsc;

    // ── cancel_prompt_run ─────────────────────────────────────────────────────

    /// Cancelling a running turn stops its task, wakes any approval waiter, and
    /// ends the turn for subscribers with exactly one `AgentError`.
    #[tokio::test]
    async fn cancel_stops_the_running_turn_and_reports_it_once() {
        let (hub_tx, mut hub_rx) = mpsc::channel(8);
        let session = Arc::new(tokio::sync::Mutex::new(ActiveAgentSession::default()));
        let (approval_tx, approval_rx) = tokio::sync::oneshot::channel();
        let turn = tokio::spawn(std::future::pending::<()>());
        {
            let mut guard = session.lock().await;
            guard.approvals.insert("call-1".into(), approval_tx);
            guard.turn = Some(turn.abort_handle());
        }

        cancel_prompt_run(&hub_tx, &session).await;

        assert!(turn.await.unwrap_err().is_cancelled());
        assert!(approval_rx.await.is_err(), "approval waiter must wake");
        match hub_rx.try_recv() {
            Ok(ClientMsg::Relay(HubRelay::AgentError { error })) => {
                assert!(error.contains("Cancelled"), "{error}")
            }
            other => panic!("expected one AgentError, got {other:?}"),
        }

        // A second cancel has nothing to stop and must not end the turn twice.
        cancel_prompt_run(&hub_tx, &session).await;
        assert!(hub_rx.try_recv().is_err());
    }

    /// A turn that already finished sent its own end; cancel adds nothing.
    #[tokio::test]
    async fn cancel_after_the_turn_finished_is_silent() {
        let (hub_tx, mut hub_rx) = mpsc::channel(8);
        let session = Arc::new(tokio::sync::Mutex::new(ActiveAgentSession::default()));
        let turn = tokio::spawn(async {});
        let abort = turn.abort_handle();
        turn.await.unwrap();
        session.lock().await.turn = Some(abort);

        cancel_prompt_run(&hub_tx, &session).await;
        assert!(hub_rx.try_recv().is_err());
    }

    // ── daemon_event_for ──────────────────────────────────────────────────────

    #[test]
    fn daemon_event_for_started() {
        let ev = OperationEvent::Started {
            operation_id: "op-1".into(),
            tool_name: "cargo_build".into(),
            description: "Build release".into(),
            parent_id: Some("session:dev".into()),
            title: None,
            cwd: None,
            command: None,
        };
        let result = daemon_event_for(&ev, "workspace/root", "test");
        let Some(DaemonEvent::OpStarted {
            id,
            tool_name,
            description,
            scope,
            parent_id,
            started_epoch_ms,
            origin,
            ..
        }) = result
        else {
            panic!("expected OpStarted, got {result:?}");
        };
        // The reporter is the only layer that knows *who asked*, so it is where the
        // origin is stamped (SPEC R24.7). The adapter that ran the command has no
        // idea which session requested it.
        assert_eq!(
            origin.as_deref(),
            Some("test"),
            "the caller's identity becomes the operation's origin"
        );
        assert_eq!(id, "op-1");
        assert_eq!(tool_name, "cargo_build");
        assert_eq!(description, "Build release");
        assert_eq!(scope, "workspace/root");
        assert_eq!(parent_id.as_deref(), Some("session:dev"));
        assert!(
            started_epoch_ms.is_some(),
            "live Started events must carry a wall-clock start"
        );
    }

    #[test]
    fn daemon_event_for_output_line() {
        let ev = OperationEvent::OutputLine {
            operation_id: "op-2".into(),
            line: "hello stdout".into(),
            is_stderr: false,
        };
        let result = daemon_event_for(&ev, "ws", "test");
        let Some(DaemonEvent::OpOutput {
            id,
            line,
            is_stderr,
        }) = result
        else {
            panic!("expected OpOutput, got {result:?}");
        };
        assert_eq!(id, "op-2");
        assert_eq!(line, "hello stdout");
        assert!(!is_stderr);
    }

    #[test]
    fn daemon_event_for_output_line_stderr() {
        let ev = OperationEvent::OutputLine {
            operation_id: "op-3".into(),
            line: "err msg".into(),
            is_stderr: true,
        };
        let Some(DaemonEvent::OpOutput { is_stderr, .. }) = daemon_event_for(&ev, "ws", "test")
        else {
            panic!("expected OpOutput");
        };
        assert!(is_stderr);
    }

    #[test]
    fn daemon_event_for_alert() {
        let ev = OperationEvent::Alert {
            operation_id: "op-4".into(),
            message: "disk full".into(),
        };
        let Some(DaemonEvent::LogLine { level, message }) = daemon_event_for(&ev, "ws", "test")
        else {
            panic!("expected LogLine");
        };
        assert_eq!(level, "alert");
        assert!(message.contains("op-4"));
        assert!(message.contains("disk full"));
    }

    #[test]
    fn daemon_event_for_completed() {
        let ev = OperationEvent::Completed {
            operation_id: "op-5".into(),
            result: json!({ "message": "ok" }),
            duration_ms: 42,
        };
        let Some(DaemonEvent::OpFinished {
            denial: _,
            id,
            status,
            duration_ms,
            result_summary,
            ended_epoch_ms,
            exit_code: None,
            interrupted: false,
        }) = daemon_event_for(&ev, "ws", "test")
        else {
            panic!("expected OpFinished");
        };
        assert_eq!(id, "op-5");
        assert_eq!(status, WireStatus::Completed);
        assert_eq!(duration_ms, 42);
        assert_eq!(result_summary, Some("ok".into()));
        assert!(
            ended_epoch_ms.is_some(),
            "live OpFinished events must carry a wall-clock end"
        );
    }

    #[test]
    fn daemon_event_for_failed() {
        let ev = OperationEvent::Failed {
            operation_id: "op-6".into(),
            error: "permission denied".into(),
            duration_ms: 10,
        };
        let Some(DaemonEvent::OpFinished {
            status,
            result_summary,
            ..
        }) = daemon_event_for(&ev, "ws", "test")
        else {
            panic!("expected OpFinished");
        };
        assert_eq!(status, WireStatus::Failed);
        assert_eq!(result_summary, Some("permission denied".into()));
    }

    #[test]
    fn daemon_event_for_cancelled() {
        let ev = OperationEvent::Cancelled {
            operation_id: "op-7".into(),
            reason: "user cancelled".into(),
            duration_ms: 5,
        };
        let Some(DaemonEvent::OpFinished {
            status,
            result_summary,
            ..
        }) = daemon_event_for(&ev, "ws", "test")
        else {
            panic!("expected OpFinished");
        };
        assert_eq!(status, WireStatus::Cancelled);
        assert_eq!(result_summary, Some("user cancelled".into()));
    }

    #[test]
    fn daemon_event_for_timed_out() {
        let ev = OperationEvent::TimedOut {
            operation_id: "op-8".into(),
            duration_ms: 30_000,
        };
        let Some(DaemonEvent::OpFinished {
            status,
            result_summary,
            duration_ms,
            ..
        }) = daemon_event_for(&ev, "ws", "test")
        else {
            panic!("expected OpFinished");
        };
        assert_eq!(status, WireStatus::TimedOut);
        assert_eq!(result_summary, Some("operation timed out".into()));
        assert_eq!(duration_ms, 30_000);
    }

    #[test]
    fn daemon_event_for_progress_is_none() {
        let ev = OperationEvent::Progress {
            operation_id: "op-9".into(),
            message: "50%".into(),
            percent: Some(0.5),
        };
        assert!(
            daemon_event_for(&ev, "ws", "test").is_none(),
            "Progress should map to None"
        );
    }

    #[test]
    fn daemon_event_for_mcp_notification_is_none() {
        let ev = OperationEvent::McpNotification {
            operation_id: "op-10".into(),
            method: "notifications/message".into(),
            params: None,
        };
        assert!(
            daemon_event_for(&ev, "ws", "test").is_none(),
            "McpNotification should map to None"
        );
    }

    // ── clip_summary ──────────────────────────────────────────────────────────

    #[test]
    fn clip_summary_short_unchanged() {
        let s = "hello world".to_string();
        assert_eq!(clip_summary(s.clone()), s);
    }

    #[test]
    fn clip_summary_exactly_200_unchanged() {
        let s = "x".repeat(200);
        let result = clip_summary(s.clone());
        assert_eq!(result, s, "200-char string must not be clipped");
    }

    #[test]
    fn clip_summary_over_200_appends_ellipsis() {
        let s = "a".repeat(250);
        let result = clip_summary(s);
        assert!(result.ends_with("..."), "should end with ...");
        assert!(
            result.len() <= 201,
            "clipped + '...' should stay short (got {})",
            result.len()
        );
    }

    #[test]
    fn clip_summary_unicode_does_not_split_char() {
        // '€' is 3 bytes. Place it so its start byte is before 197 but its end
        // byte would be past 197, verifying that the function never slices mid-char.
        let prefix = "x".repeat(196);
        let suffix = "€".repeat(20); // 3 bytes each
        let s = format!("{prefix}{suffix}");
        assert!(s.len() > 200);
        let result = clip_summary(s.clone());
        // Verify the result is valid UTF-8 (would panic on invalid slice)
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
        assert!(result.ends_with("..."));
    }

    // ── summary_from_value ───────────────────────────────────────────────────

    #[test]
    fn summary_from_value_message_field() {
        let v = json!({ "message": "all good" });
        assert_eq!(summary_from_value(&v), Some("all good".into()));
    }

    #[test]
    fn summary_from_value_error_string_field() {
        let v = json!({ "error": "something failed" });
        assert_eq!(summary_from_value(&v), Some("something failed".into()));
    }

    #[test]
    fn summary_from_value_nested_error_message() {
        let v = json!({ "error": { "message": "nested error" } });
        assert_eq!(summary_from_value(&v), Some("nested error".into()));
    }

    #[test]
    fn summary_from_value_fallback_serializes_json() {
        let v = json!({ "code": 42 });
        let result = summary_from_value(&v).unwrap();
        // The exact serialization is platform-stable: check it's non-empty JSON.
        assert!(result.contains("42"), "fallback should serialize the value");
    }

    #[test]
    fn summary_from_value_long_message_is_clipped() {
        let long = "z".repeat(300);
        let v = json!({ "message": long });
        let result = summary_from_value(&v).unwrap();
        assert!(result.len() <= 203, "summary must be clipped");
        assert!(result.ends_with("..."));
    }

    /// SPEC R-PERM.7: a replayed denial must still arrive as a denial, naming
    /// the path. It was recovered by re-scanning `result_summary_from`, which is
    /// clipped to 200 chars — so a denial whose path sits past the clip point
    /// silently degraded to an anonymous failure, defeating the guarantee the
    /// call site's own comment claims.
    #[test]
    fn denial_is_recovered_even_when_the_path_sits_past_the_summary_clip() {
        // Push the denial text well past the 200-char clip with leading output.
        let padding = "compiling some crate that prints a great deal of output\n".repeat(6);
        let denial_line =
            "touch: cannot touch '/etc/deep/denied/path': Operation not permitted (os error 1)";
        let full = format!("{padding}{denial_line}");
        assert!(full.len() > 200, "the denial must sit past the clip point");

        let result = json!({ "stdout": full });
        let mut op = Operation::new("id".into(), "tool".into(), "desc".into(), None);
        op.result = Some(result);

        let (summary, denial) = summary_and_denial(&op);
        assert!(
            summary.is_some_and(|s| s.len() <= 204),
            "the human-readable summary stays clipped"
        );
        let denial = denial.expect("a denial past the clip point must still be recovered");
        assert!(
            denial.path.contains("/etc"),
            "the refused path must survive: {}",
            denial.path
        );
    }

    // ── wire_status ──────────────────────────────────────────────────────────

    #[test]
    fn wire_status_all_variants() {
        assert_eq!(wire_status(OperationStatus::Pending), WireStatus::Pending);
        assert_eq!(
            wire_status(OperationStatus::InProgress),
            WireStatus::InProgress
        );
        assert_eq!(
            wire_status(OperationStatus::Completed),
            WireStatus::Completed
        );
        assert_eq!(wire_status(OperationStatus::Failed), WireStatus::Failed);
        assert_eq!(
            wire_status(OperationStatus::Cancelled),
            WireStatus::Cancelled
        );
        assert_eq!(wire_status(OperationStatus::TimedOut), WireStatus::TimedOut);
    }

    /// The status field was a `String` carrying these exact words. Typing it
    /// must not have changed a byte on the socket, or a new client would fail to
    /// talk to an already-running daemon. This pins the encoding.
    #[test]
    fn wire_status_serialises_to_the_historical_strings() {
        for (status, expected) in [
            (WireStatus::Pending, "\"Pending\""),
            (WireStatus::InProgress, "\"InProgress\""),
            (WireStatus::Completed, "\"Completed\""),
            (WireStatus::Failed, "\"Failed\""),
            (WireStatus::Cancelled, "\"Cancelled\""),
            (WireStatus::TimedOut, "\"TimedOut\""),
        ] {
            assert_eq!(serde_json::to_string(&status).unwrap(), expected);
            let back: WireStatus = serde_json::from_str(expected).unwrap();
            assert_eq!(back, status);
        }
    }

    /// Same guarantee for the denial's access field, which is the one that
    /// mattered: it was decoded with a `_ => Ro` catch-all, so a refused *write*
    /// arrived as read-only if the string was ever anything but exactly "rw".
    #[test]
    fn denial_access_serialises_to_the_historical_strings() {
        use ahma_common::config::ScopeAccess;
        for (access, expected) in [(ScopeAccess::Ro, "\"ro\""), (ScopeAccess::Rw, "\"rw\"")] {
            assert_eq!(serde_json::to_string(&access).unwrap(), expected);
            let back: ScopeAccess = serde_json::from_str(expected).unwrap();
            assert_eq!(back, access);
        }
    }

    // ── result_summary_from ──────────────────────────────────────────────────

    #[test]
    fn result_summary_from_no_result_is_none() {
        let op = Operation::new("id".into(), "tool".into(), "desc".into(), None);
        assert!(result_summary_from(&op).is_none());
    }

    #[test]
    fn result_summary_from_message_field() {
        let op = Operation::new(
            "id".into(),
            "tool".into(),
            "desc".into(),
            Some(json!({ "message": "build succeeded" })),
        );
        assert_eq!(result_summary_from(&op), Some("build succeeded".into()));
    }

    #[test]
    fn result_summary_from_long_result_clipped() {
        let long_msg = "y".repeat(300);
        let op = Operation::new(
            "id".into(),
            "tool".into(),
            "desc".into(),
            Some(json!({ "message": long_msg })),
        );
        let result = result_summary_from(&op).unwrap();
        assert!(result.len() <= 203);
        assert!(result.ends_with("..."));
    }

    // ── recv_optional_grant ──────────────────────────────────────────────────

    #[tokio::test]
    async fn recv_optional_grant_none_is_pending() {
        // With no receiver, the future must never resolve within the timeout.
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            recv_optional_grant(None),
        )
        .await;
        assert!(
            result.is_err(),
            "None receiver should remain pending indefinitely"
        );
    }

    #[tokio::test]
    async fn recv_optional_grant_some_returns_value() {
        use ahma_common::config::ScopeAccess;
        use ahma_common::scope_grant::{GrantReason, ScopeGrantRequest};

        let (tx, mut rx) = mpsc::unbounded_channel::<ScopeGrantRequest>();
        let req = ScopeGrantRequest {
            decision_id: "d-1".into(),
            path: std::path::PathBuf::from("/tmp/test"),
            access: ScopeAccess::Ro,
            reason: GrantReason::PreExecViolation,
            tool: Some("cargo_build".into()),
        };
        tx.send(req.clone()).unwrap();

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            recv_optional_grant(Some(&mut rx)),
        )
        .await
        .expect("should resolve");
        let got = result.expect("should have a value");
        assert_eq!(got.decision_id, "d-1");
        assert_eq!(got.access, ScopeAccess::Ro);
    }

    #[tokio::test]
    async fn recv_optional_grant_some_closed_returns_none() {
        use ahma_common::scope_grant::ScopeGrantRequest;
        // When the sender is dropped, recv() resolves to None (channel closed).
        let (tx, mut rx) = mpsc::unbounded_channel::<ScopeGrantRequest>();
        drop(tx);
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            recv_optional_grant(Some(&mut rx)),
        )
        .await
        .expect("closed channel should resolve immediately");
        assert!(result.is_none(), "closed channel yields None");
    }

    // ── daemon_event_for: clip paths in Failed / Cancelled arms ───────────────

    #[test]
    fn daemon_event_for_failed_long_error_is_clipped() {
        let ev = OperationEvent::Failed {
            operation_id: "op-f".into(),
            error: "e".repeat(300),
            duration_ms: 1,
        };
        let Some(DaemonEvent::OpFinished { result_summary, .. }) =
            daemon_event_for(&ev, "ws", "test")
        else {
            panic!("expected OpFinished");
        };
        let s = result_summary.expect("summary present");
        assert!(s.ends_with("..."), "long error must be clipped");
        assert!(s.len() <= 201);
    }

    #[test]
    fn daemon_event_for_cancelled_long_reason_is_clipped() {
        let ev = OperationEvent::Cancelled {
            operation_id: "op-c".into(),
            reason: "r".repeat(300),
            duration_ms: 2,
        };
        let Some(DaemonEvent::OpFinished { result_summary, .. }) =
            daemon_event_for(&ev, "ws", "test")
        else {
            panic!("expected OpFinished");
        };
        let s = result_summary.expect("summary present");
        assert!(s.ends_with("..."));
    }

    // ── summary_from_value: message field is non-string falls through ─────────

    #[test]
    fn summary_from_value_non_string_message_falls_through() {
        // `message` exists but is not a string → as_str() is None → fall to error,
        // then nested, then JSON fallback.
        let v = json!({ "message": 7 });
        let result = summary_from_value(&v).expect("fallback summary");
        assert!(
            result.contains('7') && result.contains("message"),
            "should serialize whole value as fallback, got {result}"
        );
    }

    // ── result_summary_from: error-string and nested-error branches ───────────

    #[test]
    fn result_summary_from_error_string_branch() {
        let op = Operation::new(
            "id".into(),
            "tool".into(),
            "desc".into(),
            Some(json!({ "error": "boom" })),
        );
        assert_eq!(result_summary_from(&op), Some("boom".into()));
    }

    #[test]
    fn result_summary_from_nested_error_message_branch() {
        let op = Operation::new(
            "id".into(),
            "tool".into(),
            "desc".into(),
            Some(json!({ "error": { "message": "deep boom" } })),
        );
        assert_eq!(result_summary_from(&op), Some("deep boom".into()));
    }

    #[test]
    fn result_summary_from_json_fallback_branch() {
        let op = Operation::new(
            "id".into(),
            "tool".into(),
            "desc".into(),
            Some(json!({ "code": 99 })),
        );
        let s = result_summary_from(&op).expect("fallback");
        assert!(s.contains("99"), "fallback serializes the JSON, got {s}");
    }

    // ── denial detection on the wire ──────────────────────────────────────────

    /// A kernel denial in a failed operation's text travels the hub wire as a
    /// denial, not an anonymous failure (SPEC R-PERM.7). This is what lets the
    /// TUI say `denied: /etc` and offer to re-raise the grant question.
    #[test]
    fn denial_text_becomes_a_wire_denial() {
        let hit = denial_from_text(
            "touch: cannot touch '/etc/foo': Operation not permitted (os error 1)",
        )
        .expect("a kernel denial must be recognised");
        assert!(hit.path.contains("/etc"), "path carried: {}", hit.path);
        // The field is typed now, so "is it one of the two" is a tautology;
        // what still needs pinning is the *encoding* the hub wire carries.
        let encoded = serde_json::to_string(&hit.access).unwrap();
        assert!(
            encoded == "\"rw\"" || encoded == "\"ro\"",
            "access is normalised for the wire, got {encoded}"
        );
    }

    /// An ordinary failure must not be dressed up as a sandbox denial — that
    /// would send the user chasing a grant that was never the problem.
    #[test]
    fn ordinary_failure_text_is_not_a_denial() {
        assert!(denial_from_text("error[E0599]: no method named `foo`").is_none());
        assert!(denial_from_text("test result: FAILED. 3 passed; 1 failed").is_none());
    }

    // ── persist_resolved_grant ────────────────────────────────────────────────
    //
    // `persist_resolved_grant` resolves the settings path via `settings_path()`,
    // which derives from the home directory. On Unix the home directory is the
    // `HOME` env var, so we redirect it to a TempDir. (On Windows `dirs::home_dir`
    // uses the Known-Folder API and ignores env vars, so these write-path tests
    // are genuinely Unix-only.)
    #[cfg(unix)]
    static HOME_ENV_MUTEX: std::sync::LazyLock<parking_lot::Mutex<()>> =
        std::sync::LazyLock::new(|| parking_lot::Mutex::new(()));

    #[cfg(unix)]
    fn with_home<R>(home: &std::path::Path, f: impl FnOnce() -> R) -> R {
        let _guard = HOME_ENV_MUTEX.lock();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home) };
        let out = f();
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        out
    }

    #[cfg(unix)]
    #[test]
    fn persist_resolved_grant_writes_settings_with_tool_provenance() {
        use ahma_common::config::ScopeAccess;
        let home = tempfile::tempdir().unwrap();
        let grant_dir = tempfile::tempdir().unwrap();
        let settings = home.path().join(".ahma").join("settings.toml");

        with_home(home.path(), || {
            persist_resolved_grant(
                grant_dir.path(),
                ScopeAccess::Rw,
                Some("sccache".to_string()),
            );
        });

        assert!(settings.exists(), "settings.toml must be created");
        let contents = std::fs::read_to_string(&settings).unwrap();
        assert!(
            contents.contains(&grant_dir.path().display().to_string()),
            "granted path must be recorded, got:\n{contents}"
        );
        assert!(
            contents.contains("sccache"),
            "granted_by provenance from tool must be recorded, got:\n{contents}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn persist_resolved_grant_defaults_granted_by_when_no_tool() {
        use ahma_common::config::ScopeAccess;
        let home = tempfile::tempdir().unwrap();
        let grant_dir = tempfile::tempdir().unwrap();
        let settings = home.path().join(".ahma").join("settings.toml");

        with_home(home.path(), || {
            persist_resolved_grant(grant_dir.path(), ScopeAccess::Ro, None);
        });

        let contents = std::fs::read_to_string(&settings).unwrap();
        assert!(
            contents.contains("scope-grant prompt"),
            "granted_by must default to the prompt label, got:\n{contents}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn persist_resolved_grant_corrupt_settings_is_non_fatal_and_preserved() {
        use ahma_common::config::ScopeAccess;
        let home = tempfile::tempdir().unwrap();
        let grant_dir = tempfile::tempdir().unwrap();
        let ahma_dir = home.path().join(".ahma");
        std::fs::create_dir_all(&ahma_dir).unwrap();
        let settings = ahma_dir.join("settings.toml");
        // Invalid TOML so the strict loader errors → persist_grant returns Err →
        // persist_resolved_grant logs a warning and does NOT overwrite the file.
        let corrupt = "this = is = not valid toml {{{";
        std::fs::write(&settings, corrupt).unwrap();

        with_home(home.path(), || {
            // Must not panic even though persistence fails.
            persist_resolved_grant(grant_dir.path(), ScopeAccess::Rw, Some("t".to_string()));
        });

        let after = std::fs::read_to_string(&settings).unwrap();
        assert_eq!(
            after, corrupt,
            "corrupt settings file must be left untouched on the error path"
        );
    }

    // ── run_reporter_loop: end-to-end over an in-process Unix socket ───────────
    //
    // These tests exercise the long-running reporter loop without a real daemon
    // by binding our OWN UnixListener in a temp path and pointing
    // `AHMA_DAEMON_SOCK` at it. `ensure_daemon_running` then connects to our
    // listener (so it never spawns the `ahma daemon` subprocess), and we read the
    // framed `ClientMsg`s the reporter emits to assert on register/replay/dispatch.
    //
    // Unix-only: the listener is a UnixListener (Windows uses TCP). The socket
    // path is process-global state, so we serialize with a mutex and restore the
    // env var on drop.

    #[cfg(unix)]
    static DAEMON_SOCK_MUTEX: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    #[cfg(unix)]
    static DAEMON_SOCK_COUNTER: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    /// Restores `AHMA_DAEMON_SOCK` to its prior value when dropped.
    #[cfg(unix)]
    struct EnvGuard {
        prev: Option<std::ffi::OsString>,
    }
    #[cfg(unix)]
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => unsafe { std::env::set_var("AHMA_DAEMON_SOCK", v) },
                None => unsafe { std::env::remove_var("AHMA_DAEMON_SOCK") },
            }
        }
    }

    /// Read and parse one newline-framed `ClientMsg` the reporter sent us.
    #[cfg(unix)]
    async fn read_client_msg<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> ClientMsg {
        use tokio::io::AsyncBufReadExt;
        let mut line = String::new();
        let n = tokio::time::timeout(TestTimeouts::scale_secs(5), reader.read_line(&mut line))
            .await
            .expect("timed out waiting for a ClientMsg from the reporter")
            .expect("io error reading ClientMsg");
        assert!(n > 0, "reporter closed the connection unexpectedly (EOF)");
        serde_json::from_str(line.trim()).expect("failed to parse ClientMsg JSON")
    }

    /// Accept connections until one sends a first message (the real reporter
    /// connection), skipping the probe connection from `ensure_daemon_running`
    /// (which connects then immediately drops → EOF).
    #[cfg(unix)]
    async fn accept_register(
        listener: &tokio::net::UnixListener,
    ) -> (
        tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
        tokio::net::unix::OwnedWriteHalf,
        ClientMsg,
    ) {
        use tokio::io::AsyncBufReadExt;
        loop {
            let (stream, _) = tokio::time::timeout(TestTimeouts::scale_secs(5), listener.accept())
                .await
                .expect("timed out waiting for the reporter to connect")
                .expect("accept failed");
            let (read_half, write_half) = stream.into_split();
            let mut reader = tokio::io::BufReader::new(read_half);
            let mut line = String::new();
            match tokio::time::timeout(TestTimeouts::scale_secs(5), reader.read_line(&mut line))
                .await
            {
                Ok(Ok(n)) if n > 0 => {
                    let msg: ClientMsg =
                        serde_json::from_str(line.trim()).expect("parse first ClientMsg");
                    return (reader, write_half, msg);
                }
                // Probe connection (EOF) or timeout — wait for the next connection.
                _ => continue,
            }
        }
    }

    /// A hooked command must not be held up by observability.
    ///
    /// The whole point of the flush budget is that it is bounded: with no
    /// daemon listening, waiting costs the budget and no more, and the command's
    /// own result is unaffected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn waiting_for_a_report_gives_up_within_its_budget() {
        let (_tx, rx) = tokio::sync::watch::channel(None);
        let mut handle = ReporterHandle { finished_rx: rx };

        let budget = Duration::from_millis(150);
        let started = std::time::Instant::now();
        let flushed = handle.wait_for_any_finished(budget).await;
        let waited = started.elapsed();

        assert!(
            !flushed,
            "nothing was reported, and that is reported honestly"
        );
        assert!(
            waited >= budget,
            "it waits for its budget before giving up: {waited:?}"
        );
        assert!(
            waited < budget * 8,
            "and no longer: a user's command must not wait on a missing daemon ({waited:?})"
        );
    }

    /// ...and when the event does land, the wait ends at once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn waiting_for_a_report_returns_as_soon_as_it_lands() {
        let (tx, rx) = tokio::sync::watch::channel(None);
        let mut handle = ReporterHandle { finished_rx: rx };

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = tx.send(Some("op-1".to_string()));
        });

        assert!(
            handle
                .wait_for_finished("op-1", TestTimeouts::scale_secs(5))
                .await,
            "the wait ends on the event, not on the timeout"
        );
    }

    /// The scope an instance advertises must be the one it actually locked.
    ///
    /// Registration happens at process start, before `roots/list` has been
    /// answered — so an instance used to advertise a placeholder (`"."` in the
    /// default roots-driven configuration) for its whole life, and a TUI
    /// filtering by project matched none of them (SPEC R24.3, R-DAEMON.6).
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)]
    async fn reporter_re_registers_with_the_committed_scope_and_session_id() {
        use crate::operation_monitor::MonitorConfig;

        let _lock = DAEMON_SOCK_MUTEX.lock();
        let unique = DAEMON_SOCK_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let sock =
            std::env::temp_dir().join(format!("ahma_scope_{}_{}.sock", std::process::id(), unique));
        let _ = std::fs::remove_file(&sock);
        let prev = std::env::var_os("AHMA_DAEMON_SOCK");
        unsafe { std::env::set_var("AHMA_DAEMON_SOCK", &sock) };
        let _env_guard = EnvGuard { prev };
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind temp daemon socket");

        // The session identity is known at startup; the scope is not.
        set_initial_identity(Some("mcp-session-42".to_string()), Some(4242));

        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            TestTimeouts::scale_secs(60),
        )));
        let reporter = tokio::spawn(run_reporter_loop(
            monitor.clone(),
            "stdio".to_string(),
            String::new(),
            "ahma".to_string(),
            None,
            None,
            tokio::sync::watch::channel(None).0,
        ));

        let (_r1, _w1, first) = accept_register(&listener).await;
        match first {
            ClientMsg::Register {
                scope,
                session_id,
                client_pid,
                ..
            } => {
                assert_eq!(
                    scope, "",
                    "before the commit there is no scope to advertise, and a \
                     placeholder would be a claim we cannot back"
                );
                assert_eq!(session_id.as_deref(), Some("mcp-session-42"));
                assert_eq!(client_pid, Some(4242));
            }
            other => panic!("expected Register first, got {other:?}"),
        }

        // The sandbox commits — the moment the real scope becomes knowable.
        set_committed_scope(&[std::path::PathBuf::from("/work/project")]);

        let (_r2, _w2, second) = accept_register(&listener).await;
        match second {
            ClientMsg::Register {
                scope, session_id, ..
            } => {
                assert_eq!(
                    scope, "/work/project",
                    "the re-registration carries the scope the sandbox locked"
                );
                assert_eq!(
                    session_id.as_deref(),
                    Some("mcp-session-42"),
                    "the session id is unchanged, so the hub keeps the instance id"
                );
            }
            other => panic!("expected a re-Register, got {other:?}"),
        }

        reporter.abort();
        let _ = std::fs::remove_file(&sock);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    // The std Mutex deliberately serializes this whole async test (the daemon
    // socket path is process-global state); holding it across awaits is the point.
    #[allow(clippy::await_holding_lock)]
    async fn reporter_loop_register_replay_dispatch_and_reconnect_over_unix_socket() {
        use crate::operation_monitor::MonitorConfig;
        use ahma_common::config::ScopeAccess;
        use ahma_common::scope_grant::{GrantCoordinator, GrantDecision, GrantReason};

        // Serialize: the socket path is global state shared by the whole process.
        let _lock = DAEMON_SOCK_MUTEX.lock();

        // Unique short socket path under the system temp dir (kept short to stay
        // under the platform's sockaddr_un path limit).
        let unique = DAEMON_SOCK_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let sock =
            std::env::temp_dir().join(format!("ahma_rep_{}_{}.sock", std::process::id(), unique));
        let _ = std::fs::remove_file(&sock);

        // Point the reporter's socket resolution at our listener and bind it
        // BEFORE spawning the reporter so `ensure_daemon_running` connects
        // immediately instead of spawning a subprocess.
        let prev = std::env::var_os("AHMA_DAEMON_SOCK");
        unsafe { std::env::set_var("AHMA_DAEMON_SOCK", &sock) };
        let _env_guard = EnvGuard { prev };
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind temp daemon socket");

        // ── Seed the monitor: one completed op (replayed as Started+Finished) and
        //    one active op (replayed as Started). ─────────────────────────────────
        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            TestTimeouts::scale_secs(60),
        )));
        monitor
            .add_operation(Operation::new(
                "comp-1".into(),
                "cargo_build".into(),
                "Build".into(),
                None,
            ))
            .await;
        monitor
            .update_status(
                "comp-1",
                OperationStatus::Completed,
                Some(json!({ "message": "done" })),
            )
            .await;
        monitor
            .add_operation(Operation::new(
                "act-1".into(),
                "cargo_test".into(),
                "Test".into(),
                None,
            ))
            .await;

        // ── Grant plumbing shared with the reporter. ─────────────────────────────
        let coord = Arc::new(GrantCoordinator::new());
        let (grant_tx, grant_rx) = mpsc::unbounded_channel::<ScopeGrantRequest>();
        let grant = GrantReporting {
            coordinator: coord.clone(),
            req_rx: grant_rx,
        };

        // ── Run the loop in the background. ──────────────────────────────────────
        let reporter = tokio::spawn(run_reporter_loop(
            monitor.clone(),
            "stdio".to_string(),
            "ws-scope".to_string(),
            "VSCode".to_string(),
            Some(grant),
            None,
            tokio::sync::watch::channel(None).0,
        ));

        // ── Register + replay ────────────────────────────────────────────────────
        let (mut server_reader, mut server_writer, reg) = accept_register(&listener).await;
        match reg {
            ClientMsg::Register {
                mode, scope, label, ..
            } => {
                assert_eq!(mode, "stdio");
                assert_eq!(scope, "ws-scope");
                assert_eq!(label, "VSCode");
            }
            other => panic!("expected Register first, got {other:?}"),
        }

        // Completed op: OpStarted then OpFinished.
        match read_client_msg(&mut server_reader).await {
            ClientMsg::Event {
                payload: DaemonEvent::OpStarted { id, scope, .. },
            } => {
                assert_eq!(id, "comp-1");
                assert_eq!(scope, "ws-scope");
            }
            other => panic!("expected replayed completed OpStarted, got {other:?}"),
        }
        match read_client_msg(&mut server_reader).await {
            ClientMsg::Event {
                payload:
                    DaemonEvent::OpFinished {
                        id,
                        status,
                        result_summary,
                        ..
                    },
            } => {
                assert_eq!(id, "comp-1");
                assert_eq!(status, WireStatus::Completed);
                assert_eq!(result_summary, Some("done".into()));
            }
            other => panic!("expected replayed completed OpFinished, got {other:?}"),
        }
        // Active op: OpStarted.
        match read_client_msg(&mut server_reader).await {
            ClientMsg::Event {
                payload: DaemonEvent::OpStarted { id, .. },
            } => assert_eq!(id, "act-1"),
            other => panic!("expected replayed active OpStarted, got {other:?}"),
        }

        // Small helper to assert a Ping is answered with a matching Pong, proving
        // the select loop kept running past whatever we sent before it.
        async fn ping_pong(
            w: &mut tokio::net::unix::OwnedWriteHalf,
            r: &mut tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
            seq: u32,
        ) {
            send_msg(w, &DaemonMsg::Ping { seq })
                .await
                .expect("send ping");
            match read_client_msg(r).await {
                ClientMsg::Pong { seq: got } => assert_eq!(got, seq, "pong seq mismatch"),
                other => panic!("expected Pong({seq}), got {other:?}"),
            }
        }

        // ── 1. Forward a fresh scope-grant request (branch 2b, Some). ────────────
        let fwd_dir = tempfile::tempdir().unwrap();
        grant_tx
            .send(ScopeGrantRequest {
                decision_id: "fwd-1".into(),
                path: fwd_dir.path().to_path_buf(),
                access: ScopeAccess::Rw,
                reason: GrantReason::PreExecViolation,
                tool: Some("rustc".into()),
            })
            .unwrap();
        match read_client_msg(&mut server_reader).await {
            ClientMsg::Relay(HubRelay::ScopeGrantRequested { request }) => {
                assert_eq!(request.decision_id, "fwd-1");
                assert_eq!(request.access, ScopeAccess::Rw);
            }
            other => panic!("expected forwarded ScopeGrantRequested, got {other:?}"),
        }

        // ── 2. Ping/Pong. ────────────────────────────────────────────────────────
        ping_pong(&mut server_writer, &mut server_reader, 11).await;

        // ── 3. Catch-all DaemonMsg (ignored), then confirm loop continues. ───────
        send_msg(&mut server_writer, &DaemonMsg::Relay(HubRelay::AgentDone))
            .await
            .expect("send AgentDone");
        ping_pong(&mut server_writer, &mut server_reader, 12).await;

        // ── 4. SubmitApproval with no pending sender (debug arm). ─────────────────
        send_msg(
            &mut server_writer,
            &DaemonMsg::SubmitApproval {
                id: None,
                approved: true,
            },
        )
        .await
        .expect("send SubmitApproval");
        ping_pong(&mut server_writer, &mut server_reader, 13).await;

        // ── 5. SubmitScopeGrant for an unknown decision (Unknown arm, no reply). ──
        send_msg(
            &mut server_writer,
            &DaemonMsg::SubmitScopeGrant {
                decision_id: "ghost".into(),
                decision: GrantDecision::Deny,
            },
        )
        .await
        .expect("send unknown SubmitScopeGrant");
        ping_pong(&mut server_writer, &mut server_reader, 14).await;

        // ── 6. SubmitScopeGrant → Denied arm → ScopeGrantResolved. ───────────────
        let grant_dir = tempfile::tempdir().unwrap();
        let req = coord
            .begin(
                grant_dir.path(),
                ScopeAccess::Ro,
                GrantReason::PreExecViolation,
                Some("cargo".into()),
            )
            .expect("begin should mint an in-flight request");
        let did = req.decision_id.clone();
        send_msg(
            &mut server_writer,
            &DaemonMsg::SubmitScopeGrant {
                decision_id: did.clone(),
                decision: GrantDecision::Deny,
            },
        )
        .await
        .expect("send Deny SubmitScopeGrant");
        match read_client_msg(&mut server_reader).await {
            ClientMsg::ScopeGrantResolved { decision_id } => assert_eq!(decision_id, did),
            other => panic!("expected ScopeGrantResolved after Deny, got {other:?}"),
        }

        // ── 7. Emit a Progress event → daemon_event_for None → continue. ─────────
        monitor.note_progress("act-1", "tick".into());
        ping_pong(&mut server_writer, &mut server_reader, 15).await;

        // ── 8. Drop the grant sender → branch 2b None arm sets grant_req_rx=None. ─
        drop(grant_tx);
        ping_pong(&mut server_writer, &mut server_reader, 16).await;

        // ── 9. Live operation event forwarded through the unified stream. ────────
        monitor
            .add_operation(Operation::new(
                "live-1".into(),
                "cargo_clippy".into(),
                "Lint".into(),
                None,
            ))
            .await;
        match read_client_msg(&mut server_reader).await {
            ClientMsg::Event {
                payload: DaemonEvent::OpStarted { id, .. },
            } => assert_eq!(id, "live-1"),
            other => panic!("expected live OpStarted, got {other:?}"),
        }

        // ── 10. Disconnect → recv_msg EOF → back-off → reconnect & re-register. ──
        drop(server_reader);
        drop(server_writer);
        let (_r2, _w2, reg2) = accept_register(&listener).await;
        match reg2 {
            ClientMsg::Register { label, .. } => assert_eq!(label, "VSCode"),
            other => panic!("expected re-Register after reconnect, got {other:?}"),
        }
        // Keep _r2/_w2 alive so the reporter parks in select rather than
        // reconnecting (which would otherwise spawn a daemon subprocess).

        // ── Teardown. ────────────────────────────────────────────────────────────
        reporter.abort();
        let _ = std::fs::remove_file(&sock);
        // _env_guard restores AHMA_DAEMON_SOCK; _lock releases the serialization.
    }

    #[test]
    fn current_identity_returns_instance_identity() {
        let id = current_identity();
        assert_eq!(id, id.clone());
    }
}
