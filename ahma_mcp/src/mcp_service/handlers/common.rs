use crate::operation_monitor::Operation;
use rmcp::model::{CallToolResult, ContentBlock, ErrorData as McpError};
use serde_json::{Map, Value};

struct CommandOutput {
    stdout: String,
    stderr: String,
    exit_code: i64,
}

/// Parses a comma-separated string value from JSON args into a list of trimmed, non-empty strings.
pub fn parse_comma_separated_filter(args: &Map<String, Value>, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| {
            s.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Serializes operations to Content text entries, logging errors.
pub fn serialize_operations_to_content(operations: &[Operation]) -> Vec<ContentBlock> {
    operations
        .iter()
        .filter_map(|op| match serde_json::to_string_pretty(op) {
            Ok(s) => Some(ContentBlock::text(s)),
            Err(e) => {
                tracing::error!("Serialization error: {}", e);
                None
            }
        })
        .collect()
}

/// Checks whether an operation matches the given tool name prefixes and optional operation ID.
pub fn operation_matches_filters(
    op: &Operation,
    tool_filters: &[String],
    id: Option<&str>,
) -> bool {
    let matches_filter =
        tool_filters.is_empty() || tool_filters.iter().any(|tn| op.tool_name.starts_with(tn));
    let matches_id = id.is_none_or(|id| op.id == id);
    matches_filter && matches_id
}

pub fn parse_tool_filters(args: &Map<String, Value>) -> Vec<String> {
    parse_comma_separated_filter(args, "tools")
}

pub fn parse_id(args: &Map<String, Value>) -> Option<String> {
    args.get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Returns a successful MCP result with a single text content block.
pub fn text_result(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text.into())])
}

/// Appends a note to a result's trailing text block, or adds one if the result
/// carries no text. Used to disclose things the caller should know about the
/// call itself (e.g. ignored arguments) without disturbing the payload.
pub fn append_note(mut result: CallToolResult, note: &str) -> CallToolResult {
    if let Some(ContentBlock::Text(last)) = result.content.last_mut() {
        last.text.push_str(note);
        return result;
    }
    result.content.push(ContentBlock::text(note.trim_start()));
    result
}

/// Builds an internal MCP error with no extra data payload.
pub fn mcp_internal(message: impl Into<String>) -> McpError {
    McpError::internal_error(message.into(), None)
}

/// Builds the MCP error for a failed synchronous tool execution.
///
/// See `denial_aware_error` for the `sandbox_denial` payload.
pub fn execution_error(e: &anyhow::Error) -> McpError {
    denial_aware_error("Synchronous execution failed", e)
}

/// Builds the MCP error for an async operation that never started.
///
/// Same treatment as [`execution_error`], and it matters more here: async is the
/// *default* execution path for `run_terminal_command`, so this is the error an
/// agent normally receives. It used to be built with [`mcp_internal`], i.e. with
/// `data: None` — a scope violation reached the agent as bare prose with no
/// remediation, and (observed on the wire in an Antigravity session) nothing it
/// could act on. Only the rarely-taken sync path carried the actionable payload.
pub fn async_execution_error(e: &anyhow::Error) -> McpError {
    denial_aware_error("Async execution failed", e)
}

/// Builds an MCP error that upgrades a sandbox denial into machine-readable
/// signal.
///
/// When the failure is an out-of-sandbox-scope path access, the error's `data`
/// field carries a `sandbox_denial` payload
/// (`{kind, path, access, reason, current_scopes, remediation}`) so an AI client
/// can reason about — and act on — the blocked path instead of parsing the
/// message text. The `path`/`access` shape mirrors the `ScopeGrantRequest` the
/// TUI already receives, so both surfaces describe a denial the same way.
/// Non-sandbox failures get a plain internal error (no `data`).
///
/// `context` prefixes the message so the caller can still tell which execution
/// path failed; the payload is identical either way, because a denial is a
/// denial regardless of how the command was going to run.
fn denial_aware_error(context: &str, e: &anyhow::Error) -> McpError {
    use crate::sandbox::SandboxError;

    // An outside service's failure leads with which one, in plain words
    // (SPEC R-HTTP.3); anything else keeps its whole cause chain, which a bare
    // `{e}` would cut to the outermost context.
    let message = match ahma_common::http_retry::find_service_error(e) {
        Some(_) => ahma_common::http_retry::user_message(e),
        None => format!("{context}: {e:#}"),
    };
    tracing::error!("{context}: {e:#}");

    if let Some(SandboxError::PathOutsideSandbox { path, scopes }) =
        e.downcast_ref::<SandboxError>()
    {
        let data = serde_json::json!({
            "kind": "sandbox_denial",
            "path": path.to_string_lossy(),
            // A working directory is used for both reads and writes.
            "access": "write",
            "reason": "path_outside_sandbox",
            "current_scopes": scopes
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            "remediation": format!(
                "'{path}' is outside the sandbox scope. To allow it, grant the path \
                 (`ahma sandbox grant {path}`) and restart to apply.",
                path = path.display()
            ),
        });
        return McpError::internal_error(message, Some(data));
    }

    // A runtime kernel denial (the command ran but the kernel blocked an
    // out-of-scope write/read it referenced in stderr). Same machine-readable
    // shape as PathOutsideSandbox so the agent can drive the grant -> restart ->
    // retry loop, but the remediation points at the `sandbox_grant`/`restart`
    // MCP tools and the `details` already carry the original command output.
    if let Some(SandboxError::RuntimeDenial {
        path,
        access,
        scopes,
        ..
    }) = e.downcast_ref::<SandboxError>()
    {
        let data = serde_json::json!({
            "kind": "sandbox_denial",
            "path": path.to_string_lossy(),
            "access": if access.is_write() { "write" } else { "read" },
            "reason": "runtime_kernel_denial",
            "current_scopes": scopes
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            "remediation": crate::sandbox::grant_channel::runtime_denial_remediation(path, *access),
        });
        return McpError::internal_error(message, Some(data));
    }

    McpError::internal_error(message, None)
}

/// Builds an invalid-params MCP error with no extra data payload.
pub fn mcp_invalid_params(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

/// Reads an optional string argument from JSON args.
pub fn opt_str(args: &Map<String, Value>, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

/// Reads a required string argument from JSON args.
pub fn require_str(
    args: &Map<String, Value>,
    key: &str,
    error_message: &str,
) -> Result<String, McpError> {
    opt_str(args, key).ok_or_else(|| mcp_invalid_params(error_message))
}

/// How long this `tools/call` should wait for its operation before handing back
/// an operation id (SPEC R2.6.1).
///
/// Two signals, no magic number:
///
/// * **Is anything else running?** If not, the model has nothing to overlap
///   with and its next move would be `await` anyway, so waiting is free and
///   buys an inline result. If it is already fanning out, hand the id back fast
///   so the next command starts now.
/// * **What will the client tolerate?** The wait holds one MCP request open, so
///   it can never approach the client's single-request budget (R2.6.5). Pass
///   the caller's already-resolved effective budget (built-in guess, or the
///   `tools.request_budget_override_secs` override when set) — this function
///   does not re-derive it from a client type.
pub async fn inline_window(
    monitor: &crate::operation_monitor::OperationMonitor,
    op_id: &str,
    budget: std::time::Duration,
) -> std::time::Duration {
    use crate::constants::{INLINE_WINDOW_BUSY_SECS, INLINE_WINDOW_IDLE_SECS};
    use std::time::Duration;

    let busy = monitor.active_count_excluding(op_id).await > 0;
    let wanted = Duration::from_secs(if busy {
        INLINE_WINDOW_BUSY_SECS
    } else {
        INLINE_WINDOW_IDLE_SECS
    });
    // Half the budget, never more: the response still has to travel back, and a
    // window that consumes the client's whole tolerance leaves no margin.
    wanted.min(budget / 2)
}

/// Attempts to wait for an async operation to complete within the inline window.
/// If the operation finishes in time, returns a `CallToolResult` with the output
/// inline. Otherwise returns `None` to signal normal async behavior.
///
/// This reduces context chatter for fast commands by eliminating the need for an
/// extra `await` round-trip.
///
/// `budget` is the caller's already-resolved effective single-request budget
/// (see [`inline_window`]).
pub async fn try_automatic_async_completion(
    monitor: &crate::operation_monitor::OperationMonitor,
    op_id: &str,
    budget: std::time::Duration,
) -> Option<rmcp::model::CallToolResult> {
    let window = inline_window(monitor, op_id, budget).await;
    wait_for_completion(monitor, op_id, window).await
}

/// Wait up to `window` for operation `op_id` to finish, returning its result
/// in the inline format (identity line first, SPEC R2.6.2), or `None` if it is
/// still running when the window ends. The operation is not affected either
/// way: it keeps running and `await` can collect it.
pub async fn wait_for_completion(
    monitor: &crate::operation_monitor::OperationMonitor,
    op_id: &str,
    window: std::time::Duration,
) -> Option<rmcp::model::CallToolResult> {
    // First check if already completed (race: task finished before we got here)
    if let Some(op) = monitor.check_completion_history_pub(op_id).await {
        return Some(format_completed_operation(&op));
    }

    // Get a completion watch receiver for this operation.
    let mut rx = match monitor.get_completion_receiver_or_terminal_pub(op_id).await {
        Err(terminal_op) => return Some(format_completed_operation(&terminal_op)),
        Ok(None) => {
            // Not in active ops; may have just completed — check history once.
            return monitor
                .check_completion_history_pub(op_id)
                .await
                .map(|op| format_completed_operation(&op));
        }
        Ok(Some(rx)) => rx,
    };

    // Wait out the inline window.
    // The watch channel stores its current value, so if the operation finished
    // between the receiver creation and this await, `wait_for` returns immediately.
    //
    // Note: `watch::Ref` wraps an `RwLockReadGuard` which is not `Send`, so we extract
    // a plain `bool` and drop the guard before any subsequent `await`.
    let timed_out = tokio::time::timeout(window, rx.wait_for(|done| *done))
        .await
        .is_err();
    if timed_out {
        // Window elapsed — fall back to normal async behavior.
        tracing::debug!(
            "Inline window ({:?}) elapsed for {}, returning async ID",
            window,
            op_id
        );
        None
    } else {
        // Operation completed — guaranteed to be in history now.
        monitor
            .check_completion_history_pub(op_id)
            .await
            .map(|op| format_completed_operation(&op))
    }
}

/// Formats a completed operation into a `CallToolResult`.
///
/// **Always leads with the identity line** (SPEC R2.6.2, R24.7). A silent
/// success used to return an empty string: `cargo fmt --check` on clean code
/// produces no stdout, no stderr and exit 0, so the model received
/// `{"text": ""}` and could not tell "passed" from "the tool is broken". The
/// exit code and duration existed — they went only to a progress notification,
/// which is best-effort and which the caller may not even receive. An operation
/// that finished inline must say so in the result itself.
fn format_completed_operation(op: &Operation) -> rmcp::model::CallToolResult {
    text_result(render_completed_operation(op))
}

/// The text of [`format_completed_operation`]: identity line, output, then the
/// workspace-queue notes (SPEC R2.7) — how long it waited for its turn, and
/// which files changed while it ran.
pub(crate) fn render_completed_operation(op: &Operation) -> String {
    use crate::operation_monitor::OperationStatus;

    let body = match op.state {
        OperationStatus::Cancelled | OperationStatus::TimedOut => op
            .result
            .as_ref()
            .and_then(|v| v.get("reason"))
            .and_then(|v| v.as_str())
            .unwrap_or("Operation was cancelled or timed out")
            .to_string(),
        _ => extract_output_from_result(&op.result),
    };

    let body = if body.trim().is_empty() {
        "(no output)"
    } else {
        body.trim_end()
    };
    let mut text = format!("{}\n{}", identity_line(op), body);
    for note in concurrency_notes(op) {
        text.push_str("\n\n");
        text.push_str(&note);
    }
    text
}

/// What the workspace write queue has to say about a finished operation
/// (SPEC R2.7.1, R2.7.6).
pub(crate) fn concurrency_notes(op: &Operation) -> Vec<String> {
    let mut notes = Vec::new();
    if let Some(ms) = op.queue_wait_ms {
        notes.push(format!(
            "Waited {} for the workspace before starting (an earlier command was still writing it).",
            crate::adapter::workspace_queue::format_duration_secs(ms.div_ceil(1000))
        ));
    }
    if let Some(changed) = op.result.as_ref().and_then(|r| r.get("changed_during_run")) {
        notes.push(changed_during_run_note(changed));
    }
    notes
}

/// Render the `changed_during_run` result field (SPEC R2.7.6).
fn changed_during_run_note(changed: &Value) -> String {
    let files: Vec<&str> = changed
        .get("files")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let more = changed.get("more").and_then(Value::as_u64).unwrap_or(0);
    let read_only = changed.get("lane").and_then(Value::as_str) == Some("read_only");
    let who = if read_only {
        "by another writer — this command could not write"
    } else {
        "by this command or another writer, such as your editor"
    };
    let mut list = files.join(", ");
    if more > 0 {
        list.push_str(&format!(" (+{more} more)"));
    }
    if changed.get("incomplete").and_then(Value::as_bool) == Some(true) {
        list.push_str(" (the scan stopped early; there may be others)");
    }
    format!(
        "Files changed while this ran ({who}): {list}. If a change was not made by this \
         command, its output may not reflect it — rerun it if that matters."
    )
}

/// The one-line identity for a finished operation, rendered by the shared
/// renderer every other surface uses (SPEC R24.7) so chat, monitor rows and
/// tool results name the same operation the same way.
fn identity_line(op: &Operation) -> String {
    use ahma_common::op_identity::{OpIdentity, OpOutcome};

    let duration_ms = op
        .end_time
        .and_then(|end| end.duration_since(op.start_time).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let exit_code = op
        .result
        .as_ref()
        .and_then(|v| v.get("exit_code"))
        .and_then(serde_json::Value::as_i64);

    OpIdentity {
        title: op.title.as_deref().unwrap_or(&op.tool_name),
        cwd: op.cwd.as_deref(),
        origin: None,
        outcome: OpOutcome::Finished {
            exit_code,
            status: format!("{:?}", op.state),
            duration_ms,
        },
    }
    .render(false)
}

fn format_structured_output(stdout_str: &str, stderr_str: &str, exit_code: i64) -> String {
    if exit_code != 0 {
        return format!(
            "Exit code: {}\nStdout:\n{}\nStderr:\n{}",
            exit_code, stdout_str, stderr_str
        );
    }

    match (stdout_str.is_empty(), stderr_str.is_empty()) {
        (true, false) => stderr_str.to_string(),
        (false, false) => format!("{}\n{}", stdout_str, stderr_str),
        _ => stdout_str.to_string(),
    }
}

fn command_output_from_value(value: &Value) -> Option<CommandOutput> {
    Some(CommandOutput {
        stdout: value
            .get("stdout")?
            .as_str()
            .unwrap_or_default()
            .to_string(),
        stderr: value
            .get("stderr")?
            .as_str()
            .unwrap_or_default()
            .to_string(),
        exit_code: value
            .get("exit_code")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1),
    })
}

fn format_result_fallback(value: &Value) -> String {
    value
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| serde_json::to_string_pretty(value).unwrap_or_default())
}

/// Extracts human-readable output from an operation result JSON value.
fn extract_output_from_result(result: &Option<Value>) -> String {
    let Some(val) = result else {
        return String::new();
    };

    if let Some(output) = command_output_from_value(val) {
        return format_structured_output(&output.stdout, &output.stderr, output.exit_code);
    }

    format_result_fallback(val)
}

/// What `await`, `status` and `cancel` say about an operation id this process
/// does not know (SPEC R-LIFECYCLE.4).
///
/// A bare "not found" leaves an agent guessing whether its build failed, is
/// still running somewhere, or never started. The id's generation tag
/// ([`crate::utils::operation::generation`]) tells us which of those is true:
/// an id from another generation predates a restart or an update (or belongs
/// to another session's process), and the work it named most likely finished
/// before that — which the agent should verify rather than assume.
pub fn unknown_operation_message(id: &str) -> String {
    use crate::utils::operation::{generation, id_generation};
    let current = generation();
    let looks_like_op_id = id
        .strip_prefix("op_")
        .and_then(|rest| rest.split('_').next())
        .is_some_and(|first| !first.is_empty() && first.chars().all(|c| c.is_ascii_digit()));
    match id_generation(id) {
        Some(g) if g == current => format!(
            "Operation '{id}' not found: this ahma process issued it, but it is no longer \
             kept (only the most recent {} completed operations are). It finished; check its \
             effect before relying on it.",
            crate::operation_monitor::MAX_COMPLETION_HISTORY
        ),
        Some(_) => format!(
            "Operation '{id}' not found: it was issued by an earlier ahma process, and ahma \
             has restarted or was updated since (or it belongs to another session). The \
             operation most likely finished before that, but its result was not kept here — \
             check its effect before relying on it, or run it again. Any output file it wrote \
             is still on disk."
        ),
        None if looks_like_op_id => format!(
            "Operation '{id}' not found: it was issued by an older ahma, which has since \
             restarted or was updated. The operation most likely finished before that, but \
             its result was not kept here — check its effect before relying on it, or run it \
             again."
        ),
        None => format!(
            "Operation '{id}' not found: this is not an ahma operation id. Ids look like \
             `op_{current}_12_cargo_build`; `status` lists the ones this session knows."
        ),
    }
}

/// What the hub recorded that bears on an operation id this process does not
/// know: why and when ahma last restarted, and how that operation ended.
#[derive(Debug, Default)]
pub struct RestartContext {
    /// The hub's `last-exit.json`.
    pub last_exit: Option<ahma_common::hub_history::LastExit>,
    /// What the hub's history says about this id.
    pub outcome: Option<ahma_common::hub_history::RecordedOutcome>,
}

/// Read the [`RestartContext`] for `id`: the last exit recorded beside the
/// hub this process reports to, and the hub's history. Either may be
/// missing, which only makes the answer less specific.
pub async fn restart_context(id: &str) -> RestartContext {
    let socket = ahma_common::hub::default_socket_path();
    let last_exit = ahma_common::hub_history::read_last_exit(
        &ahma_common::hub_history::last_exit_path_for(&socket),
    )
    .await;
    let outcome = match ahma_common::hub_history::history_path() {
        Some(path) => ahma_common::hub_history::recorded_outcome(&path, id).await,
        None => None,
    };
    RestartContext { last_exit, outcome }
}

/// "45 s", "12 min", "3 h", "2 days": how long ago, to the unit a reader
/// needs.
fn ago(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0..=89 => format!("{secs} s"),
        90..=5399 => format!("{} min", secs / 60),
        5400..=172_799 => format!("{} h", secs / 3600),
        _ => format!("{} days", secs / 86_400),
    }
}

/// [`unknown_operation_message`], told what the hub recorded (SPEC
/// R-LIFECYCLE.4): *how* the operation ended when the history kept it, and
/// when, why and from which version ahma last restarted.
///
/// Only an id from another process gains anything: one this process issued
/// and evicted, or a string that is no operation id, reads as before.
pub fn unknown_operation_message_with(id: &str, context: &RestartContext, now_ms: u64) -> String {
    use crate::utils::operation::{generation, id_generation};
    use ahma_common::hub_history::RecordedOutcome;
    let from_another_process = match id_generation(id) {
        Some(g) => g != generation(),
        None => id
            .strip_prefix("op_")
            .and_then(|rest| rest.split('_').next())
            .is_some_and(|first| !first.is_empty() && first.chars().all(|c| c.is_ascii_digit())),
    };
    if !from_another_process {
        return unknown_operation_message(id);
    }

    let interrupted_by_exit = context
        .last_exit
        .as_ref()
        .is_some_and(|exit| exit.interrupted_ops.iter().any(|op| op == id));
    let mut message = match &context.outcome {
        Some(RecordedOutcome::Finished {
            status,
            exit_code,
            summary,
            ended_epoch_ms,
            interrupted,
        }) => {
            let mut how = format!("{status:?}");
            if let Some(code) = exit_code {
                how.push_str(&format!(", exit {code}"));
            }
            if *interrupted {
                how.push_str(", interrupted");
            }
            if let Some(ended) = ended_epoch_ms {
                how.push_str(&format!(", {} ago", ago(now_ms.saturating_sub(*ended))));
            }
            if let Some(summary) = summary.as_deref().filter(|s| !s.is_empty()) {
                how.push_str(&format!(": {summary}"));
            }
            format!(
                "Operation '{id}' not found: it was issued by an earlier ahma process, and its \
                 result was not kept here. ahma's history recorded how it ended — {how}. Any \
                 output file it wrote is still on disk."
            )
        }
        Some(RecordedOutcome::NeverFinished) => format!(
            "Operation '{id}' not found: it was issued by an earlier ahma process and was still \
             running when that process went, so it did not finish. Check what it left behind, \
             or run it again."
        ),
        None if interrupted_by_exit => format!(
            "Operation '{id}' not found: it was issued by an earlier ahma process and was still \
             running when ahma restarted, which interrupted it. Check what it left behind, or \
             run it again."
        ),
        None => unknown_operation_message(id),
    };
    if let Some(exit) = &context.last_exit {
        message.push_str(&format!(
            " ahma last restarted {} ago ({}), from v{}+{} to v{}+{}.",
            ago(now_ms.saturating_sub(exit.at_epoch_ms)),
            exit.reason.phrase(),
            exit.version,
            exit.build_id,
            env!("CARGO_PKG_VERSION"),
            ahma_common::BUILD_ID,
        ));
    }
    message
}

/// What `await`, `status` and `cancel` say about an id this process does not
/// know, with everything the hub recorded about it (SPEC R-LIFECYCLE.4).
pub async fn unknown_operation_report(id: &str) -> String {
    let context = restart_context(id).await;
    unknown_operation_message_with(id, &context, ahma_common::keepalive::current_timestamp_ms())
}

#[cfg(test)]
#[path = "common_tests.rs"]
mod tests;
