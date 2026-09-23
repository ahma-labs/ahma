use super::common;
use crate::AhmaMcpService;
use crate::mcp_service::schema;
use crate::operation_monitor::Operation;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock, ErrorData as McpError};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::time::Instant;
use tracing;

/// Who issued this `await`, when that is known.
///
/// `await` is also called from the CLI and from tests, where there is no MCP
/// peer at all — hence `Default`. When a peer *is* present, two things follow
/// from it: the wait is bounded by what that client tolerates on one request
/// (SPEC R2.6.5), and progress for the awaited operations is redirected to this
/// request's token so the caller sees liveness (SPEC R2.5.3).
#[derive(Default, Clone)]
pub struct AwaitCaller {
    pub peer: Option<rmcp::service::Peer<rmcp::service::RoleServer>>,
    pub progress_token: Option<rmcp::model::ProgressToken>,
    pub client_type: Option<crate::client_type::McpClientType>,
    /// Whether the bridge (if any) has confirmed a live push channel to the
    /// real client (SPEC R2.6.5.3's liveness-probe redesign). `from_context`
    /// cannot fill this in — it has no `AhmaMcpService` to ask — so the caller
    /// must set it from `AhmaMcpService::push_channel_open()` after
    /// construction. Defaults to `false`, the safe assumption.
    pub push_channel_open: bool,
}

impl AwaitCaller {
    /// Build from a live MCP request. `push_channel_open` is left at its
    /// default (`false`); set it explicitly from
    /// `AhmaMcpService::push_channel_open()` if a live-probe wait is wanted.
    pub fn from_context(
        context: &rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> Self {
        Self {
            peer: Some(context.peer.clone()),
            progress_token: context.meta.get_progress_token(),
            client_type: Some(crate::client_type::McpClientType::from_peer(&context.peer)),
            push_channel_open: false,
        }
    }
}

/// Holds the progress targets an `await` displaced, and puts them back when the
/// await returns (SPEC R2.5.3).
///
/// Restoration is best-effort and happens on a spawned task, because `Drop`
/// cannot await — which matches the guarantee level of progress push itself
/// (R2.2): the store of record is the `OperationMonitor`, never a notification.
pub struct ProgressRedirect {
    router: Arc<crate::mcp_service::progress_push::ProgressPushRouter>,
    saved: Vec<(
        String,
        Option<crate::mcp_service::progress_push::PushTarget>,
    )>,
}

impl Drop for ProgressRedirect {
    fn drop(&mut self) {
        let router = Arc::clone(&self.router);
        let saved = std::mem::take(&mut self.saved);
        tokio::spawn(async move {
            for (op_id, previous) in saved {
                router.restore(&op_id, previous).await;
            }
        });
    }
}

/// How long this `await` will wait, and whether the client's single-request
/// budget cut it short (SPEC R2.6.5).
///
/// The clamp used to exist only as a `debug!` line. That is invisible to the
/// caller, and the caller is the one who has to act on it: a model that asked
/// for a 540s wait and got 20s of silence has no way to tell a clamp from a
/// hung operation, and "call `await` again" is the wrong conclusion to have to
/// guess at. So the reason travels with the timeout and is stated in the
/// result.
struct AwaitTimeout {
    secs: f64,
    /// `(requested_secs, client)` when the budget shortened the wait.
    clamped_from: Option<(f64, crate::client_type::McpClientType)>,
}

impl AwaitTimeout {
    fn unclamped(secs: f64) -> Self {
        Self {
            secs,
            clamped_from: None,
        }
    }

    /// The sentence appended to a timeout result when the wait was shortened.
    fn note(&self) -> Option<String> {
        let (requested, client) = self.clamped_from?;
        Some(format!(
            "\n\nNote: this wait was capped at {:.0}s rather than {:.0}s because \
             {} stops waiting on a single request at about that point (SPEC \
             R2.6.5). The cap is why the wait ended, not a problem with the \
             operation — it is still running.",
            self.secs,
            requested,
            client.display_name(),
        ))
    }
}

/// How often a live-probed wait checks the client is still there, and how long
/// a single probe is allowed to take before it's presumed dead (SPEC R2.6.5.3).
/// The probe itself is still a guessed constant — just a much smaller one,
/// repeated, instead of a single large guess about the whole wait up front.
const LIVENESS_PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);
const LIVENESS_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Sends a bare MCP `ping` and waits for the client to answer it, bounded by
/// `probe_timeout`. Any response counts as alive; a timed-out or failed send
/// means presumed dead. This rides the same JSON-RPC connection a
/// `notifications/ahma/pushChannelChanged`-confirmed session already has a live
/// path for (SPEC R2.6.5.3) — callers must check `AwaitCaller::push_channel_open`
/// before relying on this, since without it there is nowhere for the ping to go.
async fn probe_client_liveness(
    peer: &rmcp::service::Peer<rmcp::service::RoleServer>,
    probe_timeout: std::time::Duration,
) -> bool {
    use rmcp::model::{PingRequest, ServerRequest};
    use rmcp::service::PeerRequestOptions;

    let request = ServerRequest::PingRequest(PingRequest::default());
    match peer
        .send_request_with_option(request, PeerRequestOptions::with_timeout(probe_timeout))
        .await
    {
        Ok(handle) => handle.await_response().await.is_ok(),
        Err(_) => false,
    }
}

/// Why a bounded wait ended without the awaited future resolving.
///
/// The two are reported differently on purpose (SPEC R2.6.5.4): a deadline
/// means the requested time passed; a failed probe means the wait was cut
/// short — possibly a long way short — and the result must say how much time
/// actually passed and why, never the requested figure as if it had elapsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitEnded {
    /// The requested timeout expired.
    Deadline,
    /// A liveness probe went unanswered (SPEC R2.6.5.3): the client is
    /// presumed gone, so there is nobody to hand the result to.
    ClientUnresponsive,
}

impl WaitEnded {
    /// The reason stated in the result when the wait was cut short by a probe.
    fn explanation(self) -> Option<&'static str> {
        match self {
            WaitEnded::Deadline => None,
            WaitEnded::ClientUnresponsive => Some(
                "The wait ended because the client stopped answering liveness probes \
                 (SPEC R2.6.5.3), not because the requested timeout expired: ahma \
                 assumed nobody was left to receive the result.",
            ),
        }
    }
}

/// Waits for `fut` up to `total_timeout`. When `probe_peer` is `Some`,
/// periodically (every `probe_interval`, each probe bounded by
/// `probe_timeout`) probes the client's liveness while waiting and returns
/// early — as a soft timeout, since the operation keeps running either way —
/// the moment a probe fails, rather than only ever bailing at a single fixed
/// guessed duration regardless of whether the connection is actually still
/// healthy (SPEC R2.6.5.3). The [`WaitEnded`] reason travels with the error so
/// the result can say which of the two happened. When `probe_peer` is `None`
/// this is a plain bounded wait, identical to the pre-R2.6.5.3 behavior.
///
/// `probe_interval`/`probe_timeout` are parameters rather than baked-in
/// constants so tests can exercise the loop's logic (deadline math, probe
/// dispatch, early-exit on probe failure) in milliseconds instead of the real
/// [`LIVENESS_PROBE_INTERVAL`]. Production call sites pass the real constants.
async fn wait_with_optional_probe<F, T>(
    total_timeout: std::time::Duration,
    probe_peer: Option<&rmcp::service::Peer<rmcp::service::RoleServer>>,
    probe_interval: std::time::Duration,
    probe_timeout: std::time::Duration,
    fut: F,
) -> Result<T, WaitEnded>
where
    F: std::future::Future<Output = T>,
{
    let Some(peer) = probe_peer else {
        return tokio::time::timeout(total_timeout, fut)
            .await
            .map_err(|_| WaitEnded::Deadline);
    };

    let deadline = tokio::time::Instant::now() + total_timeout;
    tokio::pin!(fut);

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(WaitEnded::Deadline);
        }
        let slice = (deadline - now).min(probe_interval);
        match tokio::time::timeout(slice, &mut fut).await {
            Ok(value) => return Ok(value),
            Err(_) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(WaitEnded::Deadline);
                }
                if !probe_client_liveness(peer, probe_timeout).await {
                    tracing::warn!(
                        "Liveness probe failed mid-await; treating client as unresponsive (SPEC R2.6.5.3)"
                    );
                    return Err(WaitEnded::ClientUnresponsive);
                }
            }
        }
    }
}

impl AhmaMcpService {
    /// Point the awaited operations' progress at *this* request for as long as
    /// the returned guard lives (SPEC R2.5.3).
    async fn redirect_progress(
        &self,
        op_ids: &[String],
        caller: &AwaitCaller,
    ) -> Option<ProgressRedirect> {
        let peer = caller.peer.clone()?;
        let token = caller.progress_token.clone()?;
        let client_type = caller.client_type?;
        let progress_enabled = self.effective_supports_progress(client_type);

        let mut saved = Vec::with_capacity(op_ids.len());
        for op_id in op_ids {
            let previous = self
                .progress_push
                .redirect(op_id, peer.clone(), token.clone(), progress_enabled)
                .await;
            saved.push((op_id.clone(), previous));
        }
        Some(ProgressRedirect {
            router: Arc::clone(&self.progress_push),
            saved,
        })
    }

    /// Generates the specific input schema for the `await` tool.
    pub fn generate_input_schema_for_wait(&self) -> Arc<Map<String, Value>> {
        let mut properties = Map::new();
        properties.insert(
            "tools".to_string(),
            schema::string_property(
                "Comma-separated tool name prefixes to await for (optional; waits for all if omitted)",
            ),
        );
        properties.insert(
            "id".to_string(),
            schema::string_property("Specific operation ID to await for (optional)"),
        );
        properties.insert(
            "timeout_seconds".to_string(),
            serde_json::json!({
                "type": "integer",
                "description": format!(
                    "Maximum time to wait in seconds (optional; defaults to the configured \
                     await timeout, currently {}s). Expiry is a soft timeout: the operation \
                     keeps running and await can be called again.",
                    self.resolved_await_timeout_secs(None)
                )
            }),
        );
        schema::object_input_schema(properties, &[])
    }

    /// Resolve the await timeout per SPEC R2.5: call argument > `--await-timeout`
    /// flag / `tools.await_timeout_secs` setting > compiled-in default.
    ///
    /// The flag and the setting are already collapsed into [`AppConfig`] by
    /// `build_app_config`, so this is the single place the remaining precedence
    /// (argument over config) is decided.
    ///
    /// [`AppConfig`]: crate::shell::cli::AppConfig
    fn resolved_await_timeout_secs(&self, timeout_override: Option<u64>) -> u64 {
        timeout_override.unwrap_or_else(|| {
            self.app_config
                .read()
                .as_ref()
                .map(|c| c.await_timeout_secs)
                .unwrap_or_else(ahma_common::config::default_await_timeout_secs)
        })
    }

    /// The fallback single-request budget (SPEC R2.6.5), unless overridden via
    /// the `--request-budget-secs` flag / `tools.request_budget_override_secs`
    /// setting. Only consulted when there is no confirmed live push channel to
    /// verify liveness directly (SPEC R2.6.5.3) — `client_type` is accepted for
    /// call-site symmetry with that check, not because the fallback still
    /// varies by client identity; it no longer does (SPEC R2.6.5).
    pub(crate) fn effective_request_budget(
        &self,
        client_type: crate::client_type::McpClientType,
    ) -> std::time::Duration {
        let override_secs = self
            .app_config
            .read()
            .as_ref()
            .and_then(|c| c.request_budget_override_secs);
        match override_secs {
            Some(secs) => std::time::Duration::from_secs(secs),
            None => client_type.request_budget(),
        }
    }

    /// Bound a resolved await timeout by the fallback single-request budget
    /// (SPEC R2.5.1, R2.6.5) — only when there is no confirmed live push
    /// channel to verify liveness directly instead (SPEC R2.6.5.3).
    ///
    /// Expiry is soft — the operation keeps running and `await` can be called
    /// again — so clamping costs at most a cheap extra round-trip. Not clamping
    /// costs the session: a client that abandons the transport at 20s never
    /// receives the result of a 540s wait, and the operation's output is
    /// written into a connection nobody is reading. An explicit
    /// `timeout_seconds` argument is honoured as given; this bounds the
    /// *default*, which is what models actually use.
    ///
    /// SPEC R2.6.5.3: when the caller has a confirmed live push channel, this
    /// fallback clamp is unnecessary — `wait_with_optional_probe` verifies
    /// liveness directly instead, so the full resolved timeout is used and no
    /// clamp note is needed.
    fn bounded_await_timeout_secs(&self, resolved: f64, caller: &AwaitCaller) -> AwaitTimeout {
        if caller.push_channel_open && caller.peer.is_some() {
            return AwaitTimeout::unclamped(resolved);
        }
        let Some(client_type) = caller.client_type else {
            return AwaitTimeout::unclamped(resolved);
        };
        let budget = self.effective_request_budget(client_type).as_secs_f64();
        if resolved <= budget {
            return AwaitTimeout::unclamped(resolved);
        }
        tracing::debug!(
            client = client_type.display_name(),
            resolved,
            budget,
            "Clamping await timeout to the fallback single-request budget"
        );
        AwaitTimeout {
            secs: budget,
            clamped_from: Some((resolved, client_type)),
        }
    }

    /// How long a sync-mode tool call may wait for its operation (SPEC R2.1),
    /// and the note to add if that wait was shortened: exactly what a default
    /// `await` on it would get. Sync mode is "the call, then its `await`", so it
    /// must never hold a request open longer than the client tolerates — a
    /// result written into a connection nobody reads is a result lost.
    pub(crate) fn sync_call_wait(
        &self,
        client_type: crate::client_type::McpClientType,
        peer: Option<rmcp::service::Peer<rmcp::service::RoleServer>>,
    ) -> (std::time::Duration, Option<String>) {
        let caller = AwaitCaller {
            peer,
            progress_token: None,
            client_type: Some(client_type),
            push_channel_open: self.push_channel_open(),
        };
        let bound =
            self.bounded_await_timeout_secs(self.resolved_await_timeout_secs(None) as f64, &caller);
        (std::time::Duration::from_secs_f64(bound.secs), bound.note())
    }

    /// Handles the 'await' tool call with no caller context (CLI, tests).
    pub async fn handle_await(
        &self,
        params: CallToolRequestParams,
    ) -> Result<CallToolResult, McpError> {
        self.handle_await_for_caller(params, AwaitCaller::default())
            .await
    }

    /// Handles the 'await' tool call.
    pub async fn handle_await_for_caller(
        &self,
        params: CallToolRequestParams,
        caller: AwaitCaller,
    ) -> Result<CallToolResult, McpError> {
        let args = params.arguments.unwrap_or_default();

        let id_filter = common::parse_id(&args);
        let tool_filters = common::parse_tool_filters(&args);
        let timeout_override = args.get("timeout_seconds").and_then(|v| v.as_u64());

        // SPEC R2.6.5.3: only probeable when the bridge has confirmed a live
        // push channel exists to actually deliver a server-initiated ping on.
        let probe_peer = if caller.push_channel_open {
            caller.peer.as_ref()
        } else {
            None
        };

        // If id is specified, wait for that specific operation
        if let Some(op_id) = id_filter {
            let bound = match timeout_override {
                Some(t) => AwaitTimeout::unclamped(t as f64),
                None => self.bounded_await_timeout_secs(
                    self.resolved_await_timeout_secs(None) as f64,
                    &caller,
                ),
            };
            let _progress = self
                .redirect_progress(std::slice::from_ref(&op_id), &caller)
                .await;
            return self
                .handle_await_specific_operation(op_id, bound.secs as u64, bound.note(), probe_peer)
                .await;
        }

        // Original behavior: wait for operations by tool filter. An explicit
        // `timeout_seconds` is honoured as given; otherwise the resolved default is
        // raised to cover the longest-running pending operation.
        let bound = match timeout_override {
            Some(t) => AwaitTimeout::unclamped(t as f64),
            None => {
                let intelligent = self
                    .calculate_intelligent_timeout(
                        &tool_filters,
                        self.resolved_await_timeout_secs(None) as f64,
                    )
                    .await;
                self.bounded_await_timeout_secs(intelligent, &caller)
            }
        };
        let timeout_seconds = bound.secs;
        let clamp_note = bound.note();
        let timeout_duration = std::time::Duration::from_secs(timeout_seconds as u64);

        let pending_ops = self.pending_operations_for_filters(&tool_filters).await;

        if pending_ops.is_empty() {
            return self.handle_await_no_pending_ops(&tool_filters).await;
        }

        tracing::info!(
            "Waiting for {} pending operations (timeout: {}s): {:?}",
            pending_ops.len(),
            timeout_seconds,
            pending_ops.iter().map(|op| &op.id).collect::<Vec<_>>()
        );

        let wait_start = Instant::now();
        let (warning_task, mut warning_rx) = spawn_progress_warnings(timeout_seconds);

        let pending_ids: Vec<String> = pending_ops.iter().map(|op| op.id.clone()).collect();
        let _progress = self.redirect_progress(&pending_ids, &caller).await;

        let wait_result = self
            .wait_for_pending_operations(timeout_duration, &pending_ops, probe_peer)
            .await;

        warning_task.abort();
        while let Ok(warning) = warning_rx.try_recv() {
            tracing::info!("Wait progress: {}", warning);
        }

        match wait_result {
            Ok(contents) => Ok(build_completion_result(contents, wait_start)),
            Err(ended) => {
                self.handle_await_timeout(
                    wait_start,
                    timeout_seconds,
                    &pending_ops,
                    clamp_note,
                    ended,
                )
                .await
            }
        }
    }

    async fn handle_await_timeout(
        &self,
        wait_start: Instant,
        timeout_seconds: f64,
        pending_ops: &[Operation],
        clamp_note: Option<String>,
        ended: WaitEnded,
    ) -> Result<CallToolResult, McpError> {
        let elapsed = wait_start.elapsed();
        let still_running: Vec<Operation> = self
            .operation_monitor
            .get_all_active_operations()
            .await
            .into_iter()
            .filter(|op| !op.state.is_terminal())
            .collect();
        let completed_during_wait = pending_ops.len() - still_running.len();
        let remediation_steps = self.generate_remediation_suggestions(&still_running).await;

        let mut message = format_timeout_error_message(
            elapsed,
            timeout_seconds,
            completed_during_wait,
            pending_ops.len(),
            &still_running,
            &remediation_steps,
            ended,
        );
        if let Some(note) = clamp_note {
            message.push_str(&note);
        }
        Ok(common::text_result(message))
    }

    /// Calculate intelligent timeout based on operation timeouts and default await timeout
    pub async fn calculate_intelligent_timeout(
        &self,
        tool_filters: &[String],
        default_await_timeout: f64,
    ) -> f64 {
        let pending_ops = self.operation_monitor.get_all_active_operations().await;

        let max_op_timeout = pending_ops
            .iter()
            .filter(|op| {
                tool_filters.is_empty() || tool_filters.iter().any(|f| op.tool_name.starts_with(f))
            })
            .filter_map(|op| op.timeout_duration)
            .map(|t| t.as_secs_f64())
            .fold(0.0, f64::max);

        default_await_timeout.max(max_op_timeout)
    }

    async fn handle_await_no_pending_ops(
        &self,
        tool_filters: &[String],
    ) -> Result<CallToolResult, McpError> {
        if let Some(contents) = self.recently_completed_contents(tool_filters).await {
            return Ok(CallToolResult::success(contents));
        }

        Ok(common::text_result(if tool_filters.is_empty() {
            "No pending operations to await for.".to_string()
        } else {
            format!(
                "No pending operations for tools: {}",
                tool_filters.join(", ")
            )
        }))
    }

    async fn handle_await_specific_operation(
        &self,
        op_id: String,
        timeout_secs: u64,
        clamp_note: Option<String>,
        probe_peer: Option<&rmcp::service::Peer<rmcp::service::RoleServer>>,
    ) -> Result<CallToolResult, McpError> {
        if self.operation_monitor.get_operation(&op_id).await.is_none() {
            return Ok(self.format_already_completed_or_not_found(&op_id).await);
        }

        tracing::info!("Waiting for operation: {}", op_id);
        let timeout_duration = std::time::Duration::from_secs(timeout_secs);
        let wait_start = Instant::now();

        // `None` bound: `timeout_duration` below is the only deadline. The monitor's
        // own 300s cap would otherwise fire first for any configured await timeout
        // above it (the 540s default included) and surface as `Ok(None)` — reporting
        // a still-running operation as "completed but no result available".
        let wait_result = wait_with_optional_probe(
            timeout_duration,
            probe_peer,
            LIVENESS_PROBE_INTERVAL,
            LIVENESS_PROBE_TIMEOUT,
            self.operation_monitor
                .wait_for_operation_bounded(&op_id, None),
        )
        .await;

        match wait_result {
            Ok(Some(completed_op)) => {
                let contents =
                    common::serialize_operations_to_content(std::slice::from_ref(&completed_op));
                Ok(build_completion_result(contents, wait_start))
            }
            Ok(None) => Ok(common::text_result(format!(
                "Operation {} completed but no result available",
                op_id
            ))),
            Err(ended) => {
                let tool = self
                    .operation_monitor
                    .get_operation(&op_id)
                    .await
                    .map(|op| format!(" ({})", op.tool_name))
                    .unwrap_or_default();
                Ok(common::text_result(format_specific_timeout_message(
                    &op_id,
                    &tool,
                    timeout_secs,
                    wait_start.elapsed(),
                    ended,
                    clamp_note,
                )))
            }
        }
    }

    async fn format_already_completed_or_not_found(&self, op_id: &str) -> CallToolResult {
        let Some(completed_op) = self
            .operation_monitor
            .check_completion_history_pub(op_id)
            .await
        else {
            return common::text_result(format!("Operation {} not found", op_id));
        };
        let mut contents = vec![ContentBlock::text(format!(
            "Operation {} already completed",
            op_id
        ))];
        contents.extend(common::serialize_operations_to_content(
            std::slice::from_ref(&completed_op),
        ));
        CallToolResult::success(contents)
    }

    async fn pending_operations_for_filters(&self, tool_filters: &[String]) -> Vec<Operation> {
        self.operation_monitor
            .get_all_active_operations()
            .await
            .into_iter()
            .filter(|op| {
                !op.state.is_terminal() && common::operation_matches_filters(op, tool_filters, None)
            })
            .collect()
    }

    async fn wait_for_pending_operations(
        &self,
        timeout_duration: std::time::Duration,
        pending_ops: &[Operation],
        probe_peer: Option<&rmcp::service::Peer<rmcp::service::RoleServer>>,
    ) -> Result<Vec<ContentBlock>, WaitEnded> {
        wait_with_optional_probe(
            timeout_duration,
            probe_peer,
            LIVENESS_PROBE_INTERVAL,
            LIVENESS_PROBE_TIMEOUT,
            async {
                // `None` bound — see `handle_await_specific_operation`. Here the inner cap
                // was worse than a misreport: a `None` return is dropped by `.flatten()`
                // below, so past 300s a still-running operation silently vanished from an
                // otherwise successful result.
                let futures: Vec<_> = pending_ops
                    .iter()
                    .map(|op| {
                        self.operation_monitor
                            .wait_for_operation_bounded(&op.id, None)
                    })
                    .collect();
                let completed: Vec<Operation> = futures::future::join_all(futures)
                    .await
                    .into_iter()
                    .flatten()
                    .collect();
                common::serialize_operations_to_content(&completed)
            },
        )
        .await
    }

    async fn recently_completed_contents(
        &self,
        tool_filters: &[String],
    ) -> Option<Vec<ContentBlock>> {
        if tool_filters.is_empty() {
            return None;
        }

        let relevant_completed: Vec<Operation> = self
            .operation_monitor
            .get_completed_operations()
            .await
            .into_iter()
            .filter(|op| tool_filters.iter().any(|tn| op.tool_name.starts_with(tn)))
            .collect();

        if relevant_completed.is_empty() {
            return None;
        }

        let mut contents = vec![ContentBlock::text(format!(
            "No pending operations for tools: {}. However, these operations recently completed:",
            tool_filters.join(", ")
        ))];
        contents.extend(common::serialize_operations_to_content(&relevant_completed));
        Some(contents)
    }

    async fn generate_remediation_suggestions(&self, still_running: &[Operation]) -> Vec<String> {
        let mut steps = Vec::new();
        self.collect_lock_file_suggestions(&mut steps).await;
        collect_process_suggestions(still_running, &mut steps);
        collect_network_suggestions(still_running, &mut steps);
        collect_build_suggestions(still_running, &mut steps);
        append_default_remediation_steps(&mut steps);
        steps
    }

    async fn collect_lock_file_suggestions(&self, steps: &mut Vec<String>) {
        for dir in &["target", "node_modules", ".cargo", "tmp", "temp", ".git"] {
            scan_dir_for_lock_files(dir, steps).await;
        }
        if tokio::fs::metadata(".").await.is_ok() {
            steps.push("• Check available disk space: df -h .".to_string());
        }
    }
}

const LOCK_PATTERNS: &[&str] = &[
    ".cargo-lock",
    ".lock",
    "package-lock.json",
    "yarn.lock",
    ".npm-lock",
    "composer.lock",
    "Pipfile.lock",
    ".bundle-lock",
];

fn is_lock_file(name: &str) -> bool {
    LOCK_PATTERNS.iter().any(|p| name.contains(p))
}

async fn scan_dir_for_lock_files(dir: &str, steps: &mut Vec<String>) {
    for name in list_lock_files(dir).await {
        steps.push(format!(
            "• Remove potential stale lock file: rm {}/{}",
            dir, name
        ));
    }
}

async fn list_lock_files(dir: &str) -> Vec<String> {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return Vec::new();
    };
    let mut lock_files = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Some(name) = entry.file_name().to_str().map(String::from) else {
            continue;
        };
        if is_lock_file(&name) {
            lock_files.push(name);
        }
    }
    lock_files
}

fn spawn_progress_warnings(
    timeout_secs: f64,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::mpsc::UnboundedReceiver<String>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        for (pct, remaining_factor) in [(50, 0.5), (75, 0.25), (90, 0.1)] {
            tokio::time::sleep(std::time::Duration::from_secs_f64(
                timeout_secs * remaining_factor,
            ))
            .await;
            let _ = tx.send(format!(
                "Wait operation {}% complete ({:.0}s remaining)",
                pct,
                timeout_secs * remaining_factor
            ));
        }
    });
    (handle, rx)
}

fn build_completion_result(contents: Vec<ContentBlock>, wait_start: Instant) -> CallToolResult {
    let elapsed = wait_start.elapsed();
    if contents.is_empty() {
        return common::text_result("No operations completed within timeout period");
    }
    let mut result_contents = vec![ContentBlock::text(format!(
        "Completed {} operations in {:.2}s",
        contents.len(),
        elapsed.as_secs_f64()
    ))];
    result_contents.extend(contents);
    CallToolResult::success(result_contents)
}

fn collect_process_suggestions(still_running: &[Operation], steps: &mut Vec<String>) {
    let running_commands: HashSet<String> = still_running.iter().map(command_prefix).collect();
    for cmd in &running_commands {
        steps.push(format!(
            "• Check for competing {} processes: ps aux | grep {}",
            cmd, cmd
        ));
    }
}

fn collect_network_suggestions(still_running: &[Operation], steps: &mut Vec<String>) {
    const NETWORK_KEYWORDS: &[&str] = &[
        "network", "http", "https", "tcp", "udp", "socket", "curl", "wget", "git", "api", "rest",
        "graphql", "rpc", "ssh", "ftp", "scp", "rsync", "net", "audit", "update", "search", "add",
        "install", "fetch", "clone", "pull", "push", "download", "upload", "sync",
    ];
    push_keyword_suggestions(
        still_running,
        NETWORK_KEYWORDS,
        &[
            "• Network operations detected - check internet connection: ping 8.8.8.8",
            "• Try running with offline flags if tool supports them",
        ],
        steps,
    );
}

fn collect_build_suggestions(still_running: &[Operation], steps: &mut Vec<String>) {
    const BUILD_KEYWORDS: &[&str] = &[
        "build", "compile", "test", "lint", "clippy", "format", "check", "verify", "validate",
        "analyze",
    ];
    push_keyword_suggestions(
        still_running,
        BUILD_KEYWORDS,
        &[
            "• Build/compile operations can take time - consider increasing timeout_seconds",
            "• Check system resources: top or htop",
            "• Consider running operations with verbose flags to see progress",
        ],
        steps,
    );
}

fn command_prefix(op: &Operation) -> String {
    op.tool_name
        .split('_')
        .next()
        .unwrap_or(&op.tool_name)
        .to_string()
}

fn push_keyword_suggestions(
    still_running: &[Operation],
    keywords: &[&str],
    suggestions: &[&str],
    steps: &mut Vec<String>,
) {
    if has_keyword_match(still_running, keywords) {
        steps.extend(
            suggestions
                .iter()
                .map(|suggestion| (*suggestion).to_string()),
        );
    }
}

fn has_keyword_match(still_running: &[Operation], keywords: &[&str]) -> bool {
    still_running
        .iter()
        .any(|op| keywords.iter().any(|kw| op.tool_name.contains(kw)))
}

fn append_default_remediation_steps(steps: &mut Vec<String>) {
    if !steps.is_empty() {
        return;
    }

    steps.push("• Use the 'status' tool to check remaining operations".to_string());
    steps.push(
        "• Operations continue running in background - they may complete shortly".to_string(),
    );
    steps.push(
        "• Consider increasing timeout_seconds if operations legitimately need more time"
            .to_string(),
    );
}

/// Whether a [`soft_timeout_notice`] describes one operation or several.
#[derive(Clone, Copy)]
enum SoftTimeoutSubject {
    One,
    Many,
}

/// The SPEC R2.5.1 soft-timeout disclosure, shared by both await timeout paths.
///
/// This wording is the contract with the calling agent: it must say that the wait
/// ended but the work did not, so the agent waits again instead of re-running work
/// that is still in flight. Keep it in one place — the two await paths must never
/// tell the agent different things.
///
/// `resume_hint` is appended to "call the 'await' tool again", e.g.
/// `" with \`id: \"op-7\"\`"`, or empty for the tool-filter path.
fn soft_timeout_notice(subject: SoftTimeoutSubject, resume_hint: &str) -> String {
    let (noun, verb) = match subject {
        SoftTimeoutSubject::One => ("process has", "continues"),
        SoftTimeoutSubject::Many => ("processes have", "continue"),
    };
    let them = match subject {
        SoftTimeoutSubject::One => "its completion",
        SoftTimeoutSubject::Many => "them",
    };
    format!(
        "IMPORTANT: This is a soft timeout to prevent the IDE from disconnecting. \
         The {noun} NOT been cancelled and {verb} to run in the background. \
         If you have no other tasks to perform, you MUST call the 'await' tool again\
         {resume_hint} to continue waiting for {them}."
    )
}

/// The by-`id` soft-timeout result (SPEC R2.5.1, R2.6.5.4).
///
/// The headline states the time that **actually passed** — whole seconds,
/// rounded down, so it can never exceed the wall clock — and, when the wait
/// was cut short by a failed liveness probe, says so and states the requested
/// figure separately. The old text printed the requested timeout as if it had
/// elapsed: an `await` for 1500s that a probe ended after 79s reported
/// "after 1500s", which no reader could reconcile with the clock.
fn format_specific_timeout_message(
    op_id: &str,
    tool: &str,
    requested_secs: u64,
    elapsed: std::time::Duration,
    ended: WaitEnded,
    clamp_note: Option<String>,
) -> String {
    let waited = elapsed.as_secs().min(requested_secs);
    let headline = match ended {
        WaitEnded::Deadline => format!("Timeout waiting for operation {op_id} after {waited}s."),
        WaitEnded::ClientUnresponsive => format!(
            "Stopped waiting for operation {op_id} after {waited}s (requested {requested_secs}s). {}",
            ended.explanation().unwrap_or_default()
        ),
    };
    format!(
        "{headline}\n\n\
        The operation {op_id}{tool} is still running.\n\n\
        {}{}",
        soft_timeout_notice(SoftTimeoutSubject::One, &format!(" with `id: \"{op_id}\"`")),
        clamp_note.unwrap_or_default()
    )
}

fn format_timeout_error_message(
    elapsed: std::time::Duration,
    timeout_seconds: f64,
    completed_during_wait: usize,
    pending_count: usize,
    still_running: &[Operation],
    remediation_steps: &[String],
    ended: WaitEnded,
) -> String {
    // SPEC R2.6.5.4: the headline states the time that actually passed. A
    // probe-ended wait is "stopped", not "timed out" — it may be a long way
    // short of the configured figure, and the reason is stated right after.
    let headline = match ended {
        WaitEnded::Deadline => format!(
            "Wait operation timed out after {:.2}s (configured timeout: {:.0}s).",
            elapsed.as_secs_f64(),
            timeout_seconds
        ),
        WaitEnded::ClientUnresponsive => format!(
            "Wait operation stopped after {:.2}s of the configured {:.0}s timeout. {}",
            elapsed.as_secs_f64(),
            timeout_seconds,
            ended.explanation().unwrap_or_default()
        ),
    };
    let mut error_message = format!(
        "{headline}\n\n\
        Progress: {}/{} operations completed during await.\n\
        Still running: {} operations.\n\n\
        {}\n\n\
        Suggestions:",
        completed_during_wait,
        pending_count,
        still_running.len(),
        soft_timeout_notice(SoftTimeoutSubject::Many, "")
    );
    for step in remediation_steps {
        error_message.push_str(&format!("\n{}", step));
    }
    if !still_running.is_empty() {
        error_message.push_str("\n\nStill running operations:");
        for op in still_running {
            error_message.push_str(&format!("\n• {} ({})", op.id, op.tool_name));
        }
    }
    error_message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation_monitor::{Operation, OperationStatus};
    use ahma_common::timeouts::TestTimeouts;

    fn make_op(id: &str, tool: &str, status: OperationStatus) -> Operation {
        let mut op = Operation::new(id.to_string(), tool.to_string(), String::new(), None);
        op.state = status;
        op
    }

    #[test]
    fn test_is_lock_file_matches() {
        assert!(is_lock_file("Cargo.lock"));
        assert!(is_lock_file("package-lock.json"));
        assert!(is_lock_file("yarn.lock"));
        assert!(is_lock_file(".cargo-lock"));
    }

    #[test]
    fn test_is_lock_file_non_matches() {
        assert!(!is_lock_file("Cargo.toml"));
        assert!(!is_lock_file("src.rs"));
    }

    #[tokio::test]
    async fn test_spawn_progress_warnings_returns_handle_and_rx() {
        let (handle, mut rx) = spawn_progress_warnings(1.0);
        handle.abort();
        let _ = handle.await;
        let _ = rx.try_recv();
    }

    #[test]
    fn test_build_completion_result_empty_contents() {
        let start = Instant::now();
        let result = build_completion_result(vec![], start);
        assert!(!result.content.is_empty());
        let text = result.content.first().unwrap().as_text().unwrap();
        assert!(text.text.contains("No operations completed"));
    }

    #[test]
    fn test_build_completion_result_with_contents() {
        use rmcp::model::ContentBlock;
        let start = Instant::now();
        let contents = vec![ContentBlock::text("op output".to_string())];
        let result = build_completion_result(contents, start);
        assert_eq!(result.content.len(), 2);
        let first = result.content.first().unwrap().as_text().unwrap();
        assert!(first.text.contains("Completed"));
    }

    #[test]
    fn test_collect_process_suggestions() {
        let ops = vec![
            make_op("op1", "cargo_build", OperationStatus::InProgress),
            make_op("op2", "cargo_test", OperationStatus::InProgress),
        ];
        let mut steps = Vec::new();
        collect_process_suggestions(&ops, &mut steps);
        assert!(!steps.is_empty());
        assert!(steps.iter().any(|s| s.contains("cargo")));
    }

    #[test]
    fn test_collect_network_suggestions_with_network_op() {
        let ops = vec![make_op("op1", "git_clone", OperationStatus::InProgress)];
        let mut steps = Vec::new();
        collect_network_suggestions(&ops, &mut steps);
        assert!(steps.iter().any(|s| s.contains("Network")));
    }

    #[test]
    fn test_collect_network_suggestions_no_network_op() {
        let ops = vec![make_op("op1", "cargo_build", OperationStatus::InProgress)];
        let mut steps = Vec::new();
        collect_network_suggestions(&ops, &mut steps);
        assert!(steps.is_empty());
    }

    #[test]
    fn test_collect_build_suggestions_with_build_op() {
        let ops = vec![make_op("op1", "cargo_build", OperationStatus::InProgress)];
        let mut steps = Vec::new();
        collect_build_suggestions(&ops, &mut steps);
        assert!(steps.iter().any(|s| s.contains("Build")));
    }

    #[test]
    fn test_collect_build_suggestions_no_build_op() {
        let ops = vec![make_op("op1", "echo_tool", OperationStatus::InProgress)];
        let mut steps = Vec::new();
        collect_build_suggestions(&ops, &mut steps);
        assert!(steps.is_empty());
    }

    #[tokio::test]
    async fn test_scan_dir_for_lock_files_finds_lock() {
        let temp = tempfile::tempdir().unwrap();
        let target_dir = temp.path().join("target");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(target_dir.join("Cargo.lock"), "").unwrap();

        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(temp.path()).unwrap();

        let mut steps = Vec::new();
        scan_dir_for_lock_files("target", &mut steps).await;

        std::env::set_current_dir(&original_cwd).unwrap();

        assert!(!steps.is_empty(), "Should find Cargo.lock");
    }

    #[tokio::test]
    async fn test_scan_dir_for_lock_files_nonexistent_dir() {
        let mut steps = Vec::new();
        scan_dir_for_lock_files("nonexistent_dir_12345", &mut steps).await;
        assert!(steps.is_empty());
    }

    #[tokio::test]
    async fn test_scan_dir_for_lock_files_finds_git_index_lock() {
        let temp = tempfile::tempdir().unwrap();
        let git_dir = temp.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("index.lock"), "").unwrap();

        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(temp.path()).unwrap();

        let mut steps = Vec::new();
        scan_dir_for_lock_files(".git", &mut steps).await;

        std::env::set_current_dir(&original_cwd).unwrap();

        assert!(!steps.is_empty(), "Should find .git/index.lock");
        assert!(steps[0].contains(".git/index.lock"));
    }

    #[tokio::test]
    async fn test_collect_lock_file_suggestions_scans_git_dir() {
        let temp = tempfile::tempdir().unwrap();
        let git_dir = temp.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("index.lock"), "").unwrap();

        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(temp.path()).unwrap();

        let mut steps = Vec::new();
        for dir in &["target", "node_modules", ".cargo", "tmp", "temp", ".git"] {
            scan_dir_for_lock_files(dir, &mut steps).await;
        }

        std::env::set_current_dir(&original_cwd).unwrap();

        assert!(
            steps.iter().any(|s| s.contains(".git/index.lock")),
            "Should suggest removing stale .git/index.lock: {steps:?}"
        );
    }

    #[tokio::test]
    async fn test_calculate_intelligent_timeout() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        let timeout = service.calculate_intelligent_timeout(&[], 600.0).await;
        assert!(timeout >= 600.0);
    }

    // ============= format_timeout_error_message tests =============

    #[test]
    fn test_format_timeout_error_message_basic() {
        let elapsed = std::time::Duration::from_secs(30);
        let msg = format_timeout_error_message(elapsed, 60.0, 1, 3, &[], &[], WaitEnded::Deadline);
        assert!(msg.contains("timed out"));
        assert!(msg.contains("1/3"));
        assert!(msg.contains("60"));
    }

    /// SPEC R2.6.5.4: a probe-ended wait is not a timeout and must not be
    /// described as one — it states the time that passed and the reason.
    #[test]
    fn test_format_timeout_error_message_probe_ended_states_the_reason() {
        let elapsed = std::time::Duration::from_secs(25);
        let msg = format_timeout_error_message(
            elapsed,
            540.0,
            0,
            1,
            &[],
            &[],
            WaitEnded::ClientUnresponsive,
        );
        assert!(msg.contains("stopped after 25.00s"), "{msg}");
        assert!(!msg.contains("timed out after"), "{msg}");
        assert!(msg.contains("liveness probes"), "{msg}");
        assert!(msg.contains("NOT been cancelled"), "{msg}");
    }

    /// SPEC R2.6.5.4 on the by-`id` path: the headline never exceeds the wall
    /// clock, and a probe-ended wait names the requested figure separately.
    #[test]
    fn test_format_specific_timeout_message_never_claims_time_that_did_not_pass() {
        let probe_ended = format_specific_timeout_message(
            "op-7",
            " (run_terminal_command)",
            1500,
            std::time::Duration::from_secs_f64(79.6),
            WaitEnded::ClientUnresponsive,
            None,
        );
        assert!(probe_ended.contains("after 79s"), "{probe_ended}");
        assert!(probe_ended.contains("requested 1500s"), "{probe_ended}");
        assert!(!probe_ended.contains("after 1500s"), "{probe_ended}");
        assert!(probe_ended.contains("liveness probes"), "{probe_ended}");
        assert!(probe_ended.contains("op-7 (run_terminal_command) is still running"));

        // A deadline that fired a few ms late still reports the requested figure.
        let deadline = format_specific_timeout_message(
            "op-8",
            "",
            1,
            std::time::Duration::from_millis(1004),
            WaitEnded::Deadline,
            Some("\n\nNote: capped.".to_string()),
        );
        assert!(
            deadline.contains("Timeout waiting for operation op-8 after 1s."),
            "{deadline}"
        );
        assert!(deadline.ends_with("Note: capped."), "{deadline}");
        assert!(!deadline.contains("liveness"), "{deadline}");
    }

    #[test]
    fn test_format_timeout_error_message_with_running_ops() {
        let ops = vec![make_op("op1", "cargo_build", OperationStatus::InProgress)];
        let elapsed = std::time::Duration::from_secs(10);
        let msg = format_timeout_error_message(elapsed, 30.0, 0, 1, &ops, &[], WaitEnded::Deadline);
        assert!(msg.contains("op1"));
        assert!(msg.contains("cargo_build"));
        assert!(msg.contains("Still running"));
    }

    #[test]
    fn test_format_timeout_error_message_with_suggestions() {
        let elapsed = std::time::Duration::from_secs(5);
        let suggestions = vec!["• Try again".to_string()];
        let msg = format_timeout_error_message(
            elapsed,
            10.0,
            0,
            1,
            &[],
            &suggestions,
            WaitEnded::Deadline,
        );
        assert!(msg.contains("Try again"));
    }

    // ============= append_default_remediation_steps tests =============

    #[test]
    fn test_append_default_remediation_steps_empty_input() {
        let mut steps = Vec::new();
        append_default_remediation_steps(&mut steps);
        assert!(!steps.is_empty());
        assert!(steps.iter().any(|s| s.contains("status")));
    }

    #[test]
    fn test_append_default_remediation_steps_nonempty_input() {
        let mut steps = vec!["existing step".to_string()];
        append_default_remediation_steps(&mut steps);
        // Should NOT add defaults when there are already steps
        assert_eq!(steps.len(), 1);
    }

    // ============= command_prefix tests =============

    #[test]
    fn test_command_prefix_with_underscore() {
        let op = make_op("op1", "cargo_build", OperationStatus::InProgress);
        assert_eq!(command_prefix(&op), "cargo");
    }

    #[test]
    fn test_command_prefix_no_underscore() {
        let op = make_op("op1", "echo", OperationStatus::InProgress);
        assert_eq!(command_prefix(&op), "echo");
    }

    // ============= has_keyword_match tests =============

    #[test]
    fn test_has_keyword_match_found() {
        let ops = vec![make_op("op1", "git_clone", OperationStatus::InProgress)];
        assert!(has_keyword_match(&ops, &["git", "svn"]));
    }

    #[test]
    fn test_has_keyword_match_not_found() {
        let ops = vec![make_op("op1", "cargo_build", OperationStatus::InProgress)];
        assert!(!has_keyword_match(&ops, &["git", "svn"]));
    }

    #[test]
    fn test_has_keyword_match_empty_ops() {
        assert!(!has_keyword_match(&[], &["git"]));
    }

    // ============= push_keyword_suggestions tests =============

    #[test]
    fn test_push_keyword_suggestions_match() {
        let ops = vec![make_op("op1", "git_push", OperationStatus::InProgress)];
        let mut steps = Vec::new();
        push_keyword_suggestions(&ops, &["git"], &["• Check git remote"], &mut steps);
        assert_eq!(steps.len(), 1);
        assert!(steps[0].contains("git remote"));
    }

    #[test]
    fn test_push_keyword_suggestions_no_match() {
        let ops = vec![make_op("op1", "cargo_build", OperationStatus::InProgress)];
        let mut steps = Vec::new();
        push_keyword_suggestions(&ops, &["git"], &["• Check git remote"], &mut steps);
        assert!(steps.is_empty());
    }

    // ============= handle_await_no_pending_ops tests =============

    #[tokio::test]
    async fn test_handle_await_no_pending_ops_no_filters() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        let result = service.handle_await_no_pending_ops(&[]).await.unwrap();
        let text = result.content.first().unwrap().as_text().unwrap();
        assert!(text.text.contains("No pending operations"));
    }

    #[tokio::test]
    async fn test_handle_await_no_pending_ops_with_filters() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        let filters = vec!["cargo".to_string()];
        let result = service.handle_await_no_pending_ops(&filters).await.unwrap();
        let text = result.content.first().unwrap().as_text().unwrap();
        assert!(text.text.contains("cargo"));
    }

    // ============= recently_completed_contents tests =============

    #[tokio::test]
    async fn test_recently_completed_contents_empty_filters() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        let result = service.recently_completed_contents(&[]).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_recently_completed_contents_no_matches() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        let filters = vec!["nonexistent_tool".to_string()];
        let result = service.recently_completed_contents(&filters).await;
        assert!(result.is_none());
    }

    // ============= generate_input_schema_for_wait tests =============

    #[tokio::test]
    async fn test_generate_input_schema_for_wait() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        let schema = service.generate_input_schema_for_wait();
        let properties = schema.get("properties").unwrap().as_object().unwrap();
        assert!(properties.contains_key("tools"));
        assert!(properties.contains_key("id"));
        assert!(properties.contains_key("timeout_seconds"));
    }

    /// The schema must advertise the *resolved* default, not a baked-in number that
    /// drifts when `tools.await_timeout_secs` is set.
    #[tokio::test]
    async fn test_input_schema_reports_resolved_await_timeout() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        let schema = service.generate_input_schema_for_wait();
        let description = schema["properties"]["timeout_seconds"]["description"]
            .as_str()
            .expect("timeout_seconds needs a description");
        assert!(
            description.contains(&format!("{}s", service.resolved_await_timeout_secs(None))),
            "schema should state the resolved default, got: {description}"
        );
    }

    /// SPEC R2.5.1: both await timeout paths must give the agent the same guarantee.
    /// Pinning the shared helper keeps a reword from silently applying to only one.
    #[test]
    fn test_soft_timeout_notice_states_the_guarantee_for_both_subjects() {
        let one = soft_timeout_notice(SoftTimeoutSubject::One, " with `id: \"op-7\"`");
        assert!(one.contains("The process has NOT been cancelled"));
        assert!(one.contains("call the 'await' tool again with `id: \"op-7\"`"));

        let many = soft_timeout_notice(SoftTimeoutSubject::Many, "");
        assert!(many.contains("The processes have NOT been cancelled"));
        assert!(many.contains("call the 'await' tool again to continue waiting for them"));

        for notice in [&one, &many] {
            assert!(notice.contains("soft timeout"));
            assert!(notice.contains("continue"));
        }
    }

    // ===================================================================
    // Additional coverage: async handler paths exercising a real service +
    // operation_monitor. Targets branches in handle_await,
    // handle_await_specific_operation, format_already_completed_or_not_found,
    // handle_await_no_pending_ops (recently-completed branch),
    // handle_await_timeout, pending_operations_for_filters,
    // wait_for_pending_operations, calculate_intelligent_timeout (filter
    // branches), and spawn_progress_warnings message emission.
    // ===================================================================

    use crate::AhmaMcpService;

    fn await_params(args: serde_json::Value) -> CallToolRequestParams {
        let mut params = CallToolRequestParams::new("await".to_string());
        if let Some(a) = args.as_object().cloned() {
            params = params.with_arguments(a);
        }
        params
    }

    async fn add_active_op(service: &AhmaMcpService, id: &str, tool: &str) {
        let mut op = Operation::new(id.to_string(), tool.to_string(), String::new(), None);
        op.state = OperationStatus::InProgress;
        service.operation_monitor.add_operation(op).await;
    }

    async fn add_completed_op(service: &AhmaMcpService, id: &str, tool: &str) {
        // Add as Pending, then drive Pending -> Completed so the op lands in the
        // monitor's completion history (the path real completions take).
        let op = Operation::new(id.to_string(), tool.to_string(), String::new(), None);
        service.operation_monitor.add_operation(op).await;
        service
            .operation_monitor
            .update_status(
                id,
                OperationStatus::Completed,
                Some(serde_json::json!({"ok": true})),
            )
            .await;
    }

    // ----- handle_await: empty args, no pending ops, empty filters -----
    // Covers handle_await lines 34-53 (id None, empty pending) and
    // handle_await_no_pending_ops empty-filter text branch (136-137).
    #[tokio::test]
    async fn test_handle_await_empty_no_pending_ops() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        let result = service
            .handle_await(await_params(serde_json::json!({})))
            .await
            .unwrap();
        let text = result.content.first().unwrap().as_text().unwrap();
        assert!(text.text.contains("No pending operations to await for"));
    }

    // ----- handle_await with id that does not exist anywhere -----
    // Covers handle_await id branch (40-42), handle_await_specific_operation
    // early return (150-152), and format_already_completed_or_not_found
    // not-found branch (183-185).
    #[tokio::test]
    async fn test_handle_await_id_not_found() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        let result = service
            .handle_await(await_params(serde_json::json!({"id": "ghost-op-999"})))
            .await
            .unwrap();
        let text = result.content.first().unwrap().as_text().unwrap();
        assert!(text.text.contains("not found"));
        assert!(text.text.contains("ghost-op-999"));
    }

    // ----- handle_await with id of an already-completed (history) op -----
    // Covers format_already_completed_or_not_found already-completed branch
    // (181-193): get_operation returns None (op moved to history) and the op is
    // found in get_completed_operations.
    #[tokio::test]
    async fn test_handle_await_id_already_completed() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        add_completed_op(&service, "done-1", "cargo_build").await;
        let result = service
            .handle_await(await_params(serde_json::json!({"id": "done-1"})))
            .await
            .unwrap();
        let text = result.content.first().unwrap().as_text().unwrap();
        assert!(text.text.contains("already completed"));
        assert!(text.text.contains("done-1"));
    }

    // ----- handle_await id: op active at call time, completes during wait -----
    // Covers handle_await_specific_operation Ok(Some) branch (164-168) plus
    // build_completion_result success path via the specific-operation route.
    #[tokio::test]
    async fn test_handle_await_id_completes_during_wait() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        add_active_op(&service, "wait-1", "echo_demo").await;

        // Complete the op shortly after handle_await snapshots/subscribes.
        let mon = service.operation_monitor.clone();
        let completer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            mon.update_status(
                "wait-1",
                OperationStatus::Completed,
                Some(serde_json::json!({"ok": true})),
            )
            .await;
        });

        let result = service
            .handle_await(await_params(serde_json::json!({"id": "wait-1"})))
            .await
            .unwrap();
        completer.await.unwrap();

        let text = result.content.first().unwrap().as_text().unwrap();
        assert!(text.text.contains("Completed"));
    }

    // ----- handle_await id: `timeout_seconds` bounds the wait, and expiry is
    // SOFT — the operation keeps running (SPEC R2.5 / R2.5.1). -----
    #[tokio::test]
    async fn test_handle_await_id_timeout_is_soft_and_honors_override() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        add_active_op(&service, "slow-1", "cargo_build").await;

        // Without the override this would block on the configured default
        // (540s); with it the call must return in about a second.
        let start = Instant::now();
        let result = service
            .handle_await(await_params(
                serde_json::json!({"id": "slow-1", "timeout_seconds": 1}),
            ))
            .await
            .unwrap();
        assert!(
            start.elapsed() < std::time::Duration::from_secs(30),
            "timeout_seconds override was ignored; waited {:?}",
            start.elapsed()
        );

        let text = &result.content.first().unwrap().as_text().unwrap().text;
        assert!(text.contains("Timeout waiting for operation slow-1 after 1s"));
        assert!(text.contains("still running"));
        assert!(text.contains("NOT been cancelled"));
        assert!(text.contains("cargo_build"));

        // The operation itself must be untouched by the soft timeout.
        let op = service
            .operation_monitor
            .get_operation("slow-1")
            .await
            .expect("soft timeout must not remove the operation");
        assert_eq!(op.state, OperationStatus::InProgress);
    }

    // ----- handle_await with tool filter: op completes during wait -----
    // Covers handle_await wait path (55-81), pending_operations_for_filters
    // (196-205), wait_for_pending_operations (207-225), and the Ok(contents)
    // -> build_completion_result branch (74-75).
    #[tokio::test]
    async fn test_handle_await_filter_completes_during_wait() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        add_active_op(&service, "wf-1", "echo_demo").await;

        let mon = service.operation_monitor.clone();
        let completer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            mon.update_status(
                "wf-1",
                OperationStatus::Completed,
                Some(serde_json::json!({"ok": true})),
            )
            .await;
        });

        let result = service
            .handle_await(await_params(serde_json::json!({"tools": "echo"})))
            .await
            .unwrap();
        completer.await.unwrap();

        let text = result.content.first().unwrap().as_text().unwrap();
        assert!(text.text.contains("Completed"));
    }

    // ----- handle_await: no pending, but recently completed matches filter ---
    // Covers handle_await_no_pending_ops recently-completed branch (132-133)
    // and recently_completed_contents Some path (244-249).
    #[tokio::test]
    async fn test_handle_await_filter_recently_completed() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        add_completed_op(&service, "rc-1", "cargo_build").await;
        let result = service
            .handle_await(await_params(serde_json::json!({"tools": "cargo"})))
            .await
            .unwrap();
        let text = result.content.first().unwrap().as_text().unwrap();
        assert!(text.text.contains("recently completed"));
    }

    // ----- handle_await_timeout: direct (the 600s+ real timeout can't be
    // awaited in a fast unit test, so exercise the handler directly). -----
    // Covers handle_await_timeout (83-108), generate_remediation_suggestions
    // (252-260), collect_lock_file_suggestions (262-269), and the
    // format_timeout_error_message running-ops branch.
    #[tokio::test]
    async fn test_handle_await_timeout_direct() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        add_active_op(&service, "run-1", "git_clone").await;

        let pending = vec![
            make_op("run-1", "git_clone", OperationStatus::InProgress),
            make_op("gone-1", "cargo_build", OperationStatus::Completed),
        ];
        let start = Instant::now();
        let result = service
            .handle_await_timeout(start, 60.0, &pending, None, WaitEnded::Deadline)
            .await
            .unwrap();
        let text = result.content.first().unwrap().as_text().unwrap();
        assert!(text.text.contains("timed out"));
        // SPEC R2.5.1: the wait ended, the work did not.
        assert!(text.text.contains("NOT been cancelled"));
        // One of the two pending ops (gone-1) is no longer active -> "1/2".
        assert!(text.text.contains("1/2"));
        assert!(text.text.contains("run-1"));
        // git_clone is a network keyword -> network remediation suggestion.
        assert!(text.text.contains("Network") || text.text.contains("Suggestions"));
    }

    // ----- calculate_intelligent_timeout: filter matches an op with a long
    // per-op timeout, so the returned timeout exceeds the 600s floor. -----
    // Covers the filter (line 119), filter_map/map/fold chain (121-125) with a
    // non-zero max_op_timeout.
    #[tokio::test]
    async fn test_calculate_intelligent_timeout_with_matching_timeout_op() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        let mut op = Operation::new(
            "ct-1".to_string(),
            "cargo_build".to_string(),
            String::new(),
            None,
        );
        op.state = OperationStatus::InProgress;
        op.timeout_duration = Some(std::time::Duration::from_secs(900));
        service.operation_monitor.add_operation(op).await;

        let timeout = service
            .calculate_intelligent_timeout(&["cargo".to_string()], 600.0)
            .await;
        assert_eq!(timeout, 900.0);
    }

    // ----- calculate_intelligent_timeout: filter excludes the op, so the op's
    // long timeout is ignored and the 600s floor wins. -----
    // Covers the filter false branch (line 119 -> excluded).
    #[tokio::test]
    async fn test_calculate_intelligent_timeout_filter_excludes_op() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        let mut op = Operation::new(
            "ce-1".to_string(),
            "cargo_build".to_string(),
            String::new(),
            None,
        );
        op.state = OperationStatus::InProgress;
        op.timeout_duration = Some(std::time::Duration::from_secs(900));
        service.operation_monitor.add_operation(op).await;

        let timeout = service
            .calculate_intelligent_timeout(&["npm".to_string()], 600.0)
            .await;
        assert_eq!(timeout, 600.0);
    }

    // ----- pending_operations_for_filters: matching vs non-matching ops -----
    // Covers pending_operations_for_filters (196-205) directly, including the
    // terminal/non-matching filter exclusions.
    #[tokio::test]
    async fn test_pending_operations_for_filters_direct() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        add_active_op(&service, "p-cargo", "cargo_build").await;
        add_active_op(&service, "p-npm", "npm_install").await;

        let only_cargo = service
            .pending_operations_for_filters(&["cargo".to_string()])
            .await;
        assert_eq!(only_cargo.len(), 1);
        assert_eq!(only_cargo[0].id, "p-cargo");

        let all = service.pending_operations_for_filters(&[]).await;
        assert_eq!(all.len(), 2);
    }

    // ----- format_already_completed_or_not_found: direct not-found path -----
    // Covers format_already_completed_or_not_found 183-185 directly.
    #[tokio::test]
    async fn test_format_already_completed_or_not_found_missing() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        let result = service
            .format_already_completed_or_not_found("nope-1")
            .await;
        let text = result.content.first().unwrap().as_text().unwrap();
        assert!(text.text.contains("not found"));
    }

    // ----- spawn_progress_warnings: actually receive an emitted message -----
    // Covers the task body (320-330): sleep -> tx.send(format!(...)).
    #[tokio::test]
    async fn test_spawn_progress_warnings_emits_message() {
        // 0.04s total budget: first message fires at 0.02s (50%, factor 0.5).
        let (handle, mut rx) = spawn_progress_warnings(0.04);
        let msg = tokio::time::timeout(TestTimeouts::scale_secs(5), rx.recv())
            .await
            .expect("should not time out waiting for a progress message");
        handle.abort();
        let msg = msg.expect("channel should yield at least one message");
        assert!(msg.contains("complete"));
        assert!(msg.contains("remaining"));
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use crate::client_type::McpClientType;

    fn caller(client_type: Option<McpClientType>) -> AwaitCaller {
        AwaitCaller {
            peer: None,
            progress_token: None,
            client_type,
            push_channel_open: false,
        }
    }

    #[tokio::test]
    async fn a_long_default_is_clamped_to_what_the_client_tolerates() {
        // REGRESSION: the 540s default await outlives clients that abandon the
        // transport far sooner. In a captured session the operation's result was
        // written into a connection that had stopped reading ~50s earlier. Since
        // expiry is soft — "still running, call await again" — clamping costs a
        // cheap round-trip and saves the session.
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        let budget = McpClientType::Antigravity.request_budget().as_secs_f64();

        let bounded =
            service.bounded_await_timeout_secs(540.0, &caller(Some(McpClientType::Antigravity)));
        assert_eq!(bounded.secs, budget);

        // The clamp must be legible to the caller, not just to the log. A model
        // that asked for 540s and got 20s of silence cannot otherwise tell a
        // clamp from a hung operation.
        let note = bounded.note().expect("a clamped wait must explain itself");
        assert!(
            note.contains("20s"),
            "the actual wait must be stated: {note}"
        );
        assert!(
            note.contains("540s"),
            "the requested wait must be stated so the gap is visible: {note}"
        );
        assert!(
            note.contains("Antigravity"),
            "the client that imposed the cap must be named: {note}"
        );
        assert!(
            note.contains("still running"),
            "the note must say the operation survived the cap: {note}"
        );
    }

    #[tokio::test]
    async fn a_wait_within_the_fallback_budget_is_never_clamped() {
        // SPEC R2.6.5: the fallback budget is uniform now — there is no
        // "tolerant client" tier to test separately. Any resolved wait at or
        // under the fallback stays unclamped, for every client identity.
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        let budget = McpClientType::ClaudeDesktop.request_budget().as_secs_f64();
        let bounded =
            service.bounded_await_timeout_secs(budget, &caller(Some(McpClientType::ClaudeDesktop)));
        assert_eq!(bounded.secs, budget, "no clamp when already within budget");
        assert!(
            bounded.note().is_none(),
            "an unclamped wait must not explain a clamp that did not happen"
        );
    }

    #[tokio::test]
    async fn an_override_replaces_the_guess_for_every_client() {
        // SPEC R2.6.5.2: a wrong default (the conservative fallback is wrong
        // for a given deployment) must be correctable without a code change.
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        service.set_app_config(Arc::new(crate::shell::cli::AppConfig {
            request_budget_override_secs: Some(7),
            ..Default::default()
        }));

        // Antigravity would normally be clamped to the 20s fallback; the
        // override wins instead.
        let bounded =
            service.bounded_await_timeout_secs(30.0, &caller(Some(McpClientType::Antigravity)));
        assert_eq!(bounded.secs, 7.0);

        // The override applies to every client identity uniformly, including
        // ones that would not otherwise be clamped at all — a wait of 5s is
        // under the 20s fallback (would be unclamped by default) but the
        // operator's 7s override does not apply here since 5 < 7 anyway, so
        // assert the boundary explicitly: even a wait *within* the default
        // fallback still respects an override that happens to be smaller.
        let bounded =
            service.bounded_await_timeout_secs(10.0, &caller(Some(McpClientType::ClaudeDesktop)));
        assert_eq!(
            bounded.secs, 7.0,
            "override must clamp even a wait the default fallback (20s) would not have"
        );
    }

    #[tokio::test]
    async fn no_client_context_means_no_clamp() {
        // CLI and tests have no MCP peer, so there is nothing to protect.
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        let bounded = service.bounded_await_timeout_secs(540.0, &caller(None));
        assert_eq!(bounded.secs, 540.0);
        assert!(bounded.note().is_none());
    }

    /// An explicit `timeout_seconds` is honoured verbatim (SPEC R2.5.1), so it
    /// must never produce a clamp note — the caller chose that number.
    #[tokio::test]
    async fn an_explicit_timeout_is_never_reported_as_clamped() {
        let bound = AwaitTimeout::unclamped(5.0);
        assert_eq!(bound.secs, 5.0);
        assert!(bound.note().is_none());
    }

    async fn add_stuck_op(service: &AhmaMcpService, id: &str, tool: &str) {
        let mut op = Operation::new(id.to_string(), tool.to_string(), String::new(), None);
        op.state = crate::operation_monitor::OperationStatus::InProgress;
        service.operation_monitor.add_operation(op).await;
    }

    /// The clamp reaches the **result**, through the whole `await` path.
    ///
    /// The unit tests above prove the note is built correctly; this proves it is
    /// attached. Those are different failures, and the second one is the one
    /// that matters — a note that is computed and then dropped leaves the caller
    /// exactly as uninformed as the `debug!` line it replaced.
    ///
    /// `start_paused` because the whole point is a wait long enough to matter:
    /// the operation never completes and tokio auto-advances the 20s budget, so
    /// the test costs no wall-clock time. There is no real subprocess here for
    /// virtual time to desynchronise from.
    #[tokio::test(start_paused = true)]
    async fn a_clamped_await_says_so_in_the_result() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        add_stuck_op(&service, "op_stuck_1", "run_terminal_command").await;

        // No `timeout_seconds`: the default (540s) is what gets clamped, and the
        // default is what models actually use.
        let params = CallToolRequestParams::new("await");
        let result = service
            .handle_await_for_caller(params, caller(Some(McpClientType::Antigravity)))
            .await
            .expect("a soft timeout is a result, never an error");

        let text: String = result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect();

        assert!(
            text.contains("timed out"),
            "the wait must have expired for the note to apply, got: {text}"
        );
        assert!(
            text.contains("Antigravity"),
            "the result must name the client whose budget capped the wait \
             (SPEC R2.6.5) — otherwise the cap is invisible to the caller and \
             indistinguishable from a hung operation. Got: {text}"
        );
        assert!(
            text.contains("540s"),
            "the result must state the wait that was asked for, so the gap is \
             visible. Got: {text}"
        );
    }

    /// The complement: a client that can take the long wait gets no note, so the
    /// disclosure stays meaningful instead of becoming boilerplate.
    #[tokio::test(start_paused = true)]
    async fn an_unclamped_await_carries_no_note() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        add_stuck_op(&service, "op_stuck_2", "run_terminal_command").await;

        let mut args = serde_json::Map::new();
        args.insert("timeout_seconds".to_string(), serde_json::json!(5));
        let params = CallToolRequestParams::new("await").with_arguments(args);
        let result = service
            .handle_await_for_caller(params, caller(Some(McpClientType::Antigravity)))
            .await
            .expect("a soft timeout is a result, never an error");

        let text: String = result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect();
        assert!(text.contains("timed out"), "got: {text}");
        assert!(
            !text.contains("was capped at"),
            "an explicit `timeout_seconds` is honoured verbatim, so nothing was \
             capped and nothing may claim otherwise. Got: {text}"
        );
    }
}

#[cfg(test)]
mod liveness_probe_tests {
    use super::*;
    use crate::AhmaMcpService;
    use crate::operation_monitor::{Operation, OperationStatus};
    use crate::test_utils::in_process::create_in_process_mcp_empty;

    #[tokio::test]
    async fn no_probe_peer_is_a_plain_bounded_wait() {
        // Backward compatibility: `probe_peer: None` (no live channel confirmed,
        // or CLI/tests with no MCP peer at all) must behave exactly like the
        // pre-R2.6.5.3 `tokio::time::timeout` — no probing attempted.
        let start = Instant::now();
        let result = wait_with_optional_probe(
            std::time::Duration::from_millis(50),
            None,
            std::time::Duration::from_millis(10),
            std::time::Duration::from_millis(10),
            std::future::pending::<()>(),
        )
        .await;
        assert!(result.is_err(), "a never-resolving future must time out");
        assert!(
            start.elapsed() < std::time::Duration::from_millis(300),
            "must bail at the requested timeout, not hang: {:?}",
            start.elapsed()
        );

        let fast = wait_with_optional_probe(
            std::time::Duration::from_secs(5),
            None,
            std::time::Duration::from_millis(10),
            std::time::Duration::from_millis(10),
            async { 42 },
        )
        .await;
        assert_eq!(fast, Ok(42));
    }

    #[tokio::test]
    async fn a_live_peer_survives_multiple_probe_intervals_to_reach_completion() {
        // The core new behavior: a wait that would previously have been capped
        // at a guessed budget now runs the full requested duration, verifying
        // liveness along the way instead of assuming death at a fixed point.
        // Every probe here succeeds (rmcp's default ClientHandler::ping always
        // answers `Ok(())`), so this must reach the future's own completion,
        // not time out and not bail early on a phantom probe failure.
        let mcp = create_in_process_mcp_empty()
            .await
            .expect("in-process pair");
        let peer = mcp._server.peer();

        let start = Instant::now();
        let result = wait_with_optional_probe(
            std::time::Duration::from_secs(2),
            Some(peer),
            std::time::Duration::from_millis(40),
            std::time::Duration::from_millis(200),
            async {
                tokio::time::sleep(std::time::Duration::from_millis(180)).await;
                "done"
            },
        )
        .await;

        assert_eq!(
            result,
            Ok("done"),
            "must reach real completion, not bail out on a live connection"
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(180),
            "must not return before the future actually resolved: {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "must not run anywhere near the 2s deadline when the future \
             resolved quickly: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn a_dead_peer_ends_the_wait_early_without_reaching_the_deadline() {
        // The other half of the redesign: detect death, don't guess a fixed
        // deadline for it. Closing the client mid-wait must be caught by the
        // very next probe and end the wait well before the (generously long)
        // total_timeout — proving the mechanism reacts to the connection's
        // actual state instead of always running to a static number.
        let mut mcp = create_in_process_mcp_empty()
            .await
            .expect("in-process pair");
        let peer = mcp._server.peer().clone();

        mcp.client
            .close()
            .await
            .expect("closing the client side must not itself error");

        let start = Instant::now();
        let result = wait_with_optional_probe(
            std::time::Duration::from_secs(30),
            Some(&peer),
            std::time::Duration::from_millis(20),
            std::time::Duration::from_millis(200),
            std::future::pending::<()>(),
        )
        .await;

        assert_eq!(
            result,
            Err(WaitEnded::ClientUnresponsive),
            "a probe against a closed connection must fail, not hang forever — and \
             the failure must be distinguishable from the deadline"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "must bail out on the first failed probe, nowhere near the 30s \
             deadline: {:?}",
            start.elapsed()
        );
    }

    fn probing_caller(peer: rmcp::service::Peer<rmcp::service::RoleServer>) -> AwaitCaller {
        AwaitCaller {
            peer: Some(peer),
            progress_token: None,
            client_type: Some(crate::client_type::McpClientType::ClaudeDesktop),
            push_channel_open: true,
        }
    }

    fn result_text(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect()
    }

    async fn add_running_op(service: &AhmaMcpService, id: &str) {
        let mut op = Operation::new(
            id.to_string(),
            "run_terminal_command".to_string(),
            String::new(),
            None,
        );
        op.state = OperationStatus::InProgress;
        service.operation_monitor.add_operation(op).await;
    }

    /// REGRESSION (dogfooding, 2026-09): `await` with `timeout_seconds: 1500` on
    /// a running operation came back after roughly a minute saying "Timeout
    /// waiting for operation … after 1500s". The wait had ended on a failed
    /// liveness probe (SPEC R2.6.5.3), but the text reported the *requested*
    /// number as if that much time had passed — a claim no reader could
    /// reconcile with the clock. A wait that ends early must say how long it
    /// actually lasted, what was asked for, and why it stopped.
    ///
    /// `start_paused`: the real 20s probe interval is auto-advanced, so the test
    /// costs no wall-clock time; the closed client makes the first probe fail.
    #[tokio::test(start_paused = true)]
    async fn a_probe_ended_wait_reports_the_time_that_passed_not_the_requested_timeout() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        add_running_op(&service, "op_probe_1").await;

        let mut mcp = create_in_process_mcp_empty()
            .await
            .expect("in-process pair");
        let peer = mcp._server.peer().clone();
        mcp.client.close().await.expect("close client");

        let mut args = serde_json::Map::new();
        args.insert("id".to_string(), serde_json::json!("op_probe_1"));
        args.insert("timeout_seconds".to_string(), serde_json::json!(1500));
        let params = CallToolRequestParams::new("await").with_arguments(args);
        let start = Instant::now();
        let result = service
            .handle_await_for_caller(params, probing_caller(peer))
            .await
            .expect("a soft timeout is a result, never an error");
        let waited = start.elapsed().as_secs();
        assert!(
            waited < 1500,
            "the probe must end the wait long before the deadline, waited {waited}s"
        );

        let text = result_text(&result);
        assert!(
            !text.contains("after 1500s"),
            "must never claim the requested 1500s elapsed when it did not: {text}"
        );
        assert!(
            text.contains(&format!("after {waited}s")),
            "must state the time that actually passed ({waited}s): {text}"
        );
        assert!(
            text.contains("requested 1500s"),
            "must state what was asked for, so the gap is visible: {text}"
        );
        assert!(
            text.contains("liveness probe"),
            "must say why the wait ended — a failed liveness probe, not expiry: {text}"
        );
        assert!(
            text.contains("still running") && text.contains("NOT been cancelled"),
            "SPEC R2.5.1: the work survives the ended wait: {text}"
        );
        assert!(
            text.contains("op_probe_1") && text.contains("run_terminal_command"),
            "the operation and its tool must be named: {text}"
        );
    }

    /// The tool-filter path ends the same way and must give the same account.
    #[tokio::test(start_paused = true)]
    async fn a_probe_ended_filter_wait_says_why_it_stopped() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        add_running_op(&service, "op_probe_2").await;

        let mut mcp = create_in_process_mcp_empty()
            .await
            .expect("in-process pair");
        let peer = mcp._server.peer().clone();
        mcp.client.close().await.expect("close client");

        let params = CallToolRequestParams::new("await");
        let result = service
            .handle_await_for_caller(params, probing_caller(peer))
            .await
            .expect("a soft timeout is a result, never an error");

        let text = result_text(&result);
        assert!(
            text.contains("liveness probe"),
            "the reason the wait stopped must be stated: {text}"
        );
        assert!(
            !text.contains("timed out after 540"),
            "the wait did not run to its 540s deadline and must not say so: {text}"
        );
        assert!(text.contains("NOT been cancelled"), "{text}");
    }

    /// The other half of the report: an operation that completes part-way
    /// through a long explicit wait must come back as its result, promptly,
    /// through the probing path — never as a timeout.
    #[tokio::test(start_paused = true)]
    async fn a_completion_during_a_long_probed_wait_returns_the_result() {
        let (service, _tmp) = crate::test_utils::client::setup_test_environment().await;
        add_running_op(&service, "op_probe_3").await;

        let mcp = create_in_process_mcp_empty()
            .await
            .expect("in-process pair");
        let peer = mcp._server.peer().clone();

        let mon = service.operation_monitor.clone();
        let completer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(79)).await;
            mon.update_status(
                "op_probe_3",
                OperationStatus::Completed,
                Some(serde_json::json!({"ok": true})),
            )
            .await;
        });

        let mut args = serde_json::Map::new();
        args.insert("id".to_string(), serde_json::json!("op_probe_3"));
        args.insert("timeout_seconds".to_string(), serde_json::json!(1500));
        let params = CallToolRequestParams::new("await").with_arguments(args);
        let start = Instant::now();
        let result = service
            .handle_await_for_caller(params, probing_caller(peer))
            .await
            .expect("completion is a result");
        completer.await.unwrap();

        let text = result_text(&result);
        assert!(
            text.contains("Completed") && !text.contains("Timeout"),
            "a completion during the wait is the result, not a timeout: {text}"
        );
        let waited = start.elapsed().as_secs();
        assert!(
            (79..100).contains(&waited),
            "must return as soon as the operation completed (~79s), waited {waited}s"
        );
    }
}
