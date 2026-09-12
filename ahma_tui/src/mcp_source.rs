//! Async background task that feeds live data from the ahma server into the TUI.
//!
//! The task polls `/health` for connection state and (when the server is reachable)
//! performs a simplified MCP handshake so it can call `tools/list` and `status`.
//! All results are forwarded to the app event loop via an `mpsc` channel.

use std::time::Duration;

use ahma_common::mcp_methods::{SANDBOX_CONFIGURED_METHOD, SANDBOX_FAILED_METHOD};
use ahma_http_mcp_client::streamable::{
    ConflictRetryPolicy, ConnectOptions, Connector, StreamableHttpMcpClient, ToolCallOutcome,
};
use anyhow::Result;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::connection::{ResolvedConnection, ResolvedTransport};
use crate::state::{AiActivityEntry, LogEntry, LogFileInfo, LogLevel, OpStatus, Operation};

// ─── Events emitted by this task ─────────────────────────────────────────────

/// Events produced by the MCP source task and consumed by the app event loop.
#[derive(Debug, Clone)]
pub enum SourceEvent {
    HealthChanged {
        healthy: bool,
    },
    DaemonHealthChanged {
        healthy: bool,
    },
    OperationsUpdated {
        ops: Vec<Operation>,
    },
    /// A single live output line from a running operation, streamed as the
    /// child process produces it (pushed via the daemon hub).
    OperationOutput {
        instance_id: Option<String>,
        op_id: String,
        line: String,
        is_stderr: bool,
    },
    AiActivity(AiActivityEntry),
    LogLine(LogEntry),
    ToolsListUpdated {
        tools: Vec<crate::mcp_connections::ToolInfo>,
    },
    SandboxStatus {
        status: crate::state::SandboxAuthority,
    },
    /// The complete scope + provenance carried by `notifications/sandbox/configured`
    /// (SPEC R5.4: every writable root, read root, tmp, enforcement, and source).
    SandboxScope {
        scope: crate::state::SandboxScopeInfo,
    },
    /// `notifications/sandbox/failed` with its reason, so the failure outlives
    /// a log line and can be shown with remediation (SPEC R7.3: fail loudly).
    SandboxFailed {
        error: String,
    },
    SessionId {
        id: String,
    },
    LogFilesUpdated {
        files: Vec<LogFileInfo>,
    },
    LogLinesUpdated {
        file: String,
        content: String,
        append: bool,
    },
    InstancesUpdated {
        instances: Vec<ahma_common::daemon_hub::InstanceInfo>,
    },
    ChatToken {
        token: String,
    },
    ChatThinking {
        token: String,
    },
    ApprovalRequested {
        id: String,
        tool: String,
        args: String,
    },
    /// An auto-detected sandbox scope violation: raise the "grant access?" modal.
    ScopeGrantRequested {
        request: ahma_common::scope_grant::ScopeGrantRequest,
    },
    /// Dismiss the scope-grant modal for `decision_id` (a twin answered, or the
    /// instance withdrew it).
    ScopeGrantDismiss {
        decision_id: String,
    },
    /// An unknown domain under a `deny` web policy: raise the "allow egress?" modal.
    WebApprovalRequested {
        request: ahma_common::web_approval::WebApprovalRequest,
    },
    /// Dismiss the web-approval modal for `decision_id` (a twin answered, or the
    /// instance withdrew it).
    WebApprovalDismiss {
        decision_id: String,
    },
    AgentDone,
    AgentError {
        error: String,
    },
    /// Token usage for the latest model turn, forwarded over the daemon hub so
    /// the status-bar counter updates on the hub path (not just in-process).
    Usage {
        prompt_tokens: u32,
        completion_tokens: u32,
        total_tokens: u32,
    },
    /// A tool call the agent started — drives the live "which tool is running"
    /// display when the agent runs through the daemon hub.
    ToolCallStarted {
        id: String,
        name: String,
        args: String,
    },
    /// A tool call result, forwarded over the daemon hub.
    ToolCallFinished {
        id: String,
        result: String,
        failed: bool,
    },
    /// The model's response was cut short (length limit, provider
    /// truncation), forwarded over the daemon hub so the hub path can render
    /// it as a system note the same way the in-process path's
    /// `BridgeEvent::Truncated` already does.
    Truncated {
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub enum McpSourceCommand {
    SetActiveFile(Option<String>),
    RefreshLogs,
    SetDaemonHealthy(bool),
}

// ─── Entry point ─────────────────────────────────────────────────────────────

/// Spawn the background task.  The task runs until the channel is closed or
/// the server stays unreachable for an extended period.
pub fn spawn_mcp_source(
    connection: ResolvedConnection,
    tx: mpsc::Sender<SourceEvent>,
    workspace_path: Option<std::path::PathBuf>,
) -> mpsc::Sender<McpSourceCommand> {
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    tokio::spawn(async move {
        mcp_source_task(connection, tx, cmd_rx, workspace_path).await;
    });
    cmd_tx
}

// ─── Task implementation ──────────────────────────────────────────────────────

async fn mcp_source_task(
    connection: ResolvedConnection,
    tx: mpsc::Sender<SourceEvent>,
    mut cmd_rx: mpsc::Receiver<McpSourceCommand>,
    workspace_path: Option<std::path::PathBuf>,
) {
    let base_url = extract_http_base_url(&connection);
    debug!("mcp_source: base_url={base_url}");

    let socket_path = base_url.strip_prefix("unix://");
    let builder = reqwest::Client::builder().timeout(Duration::from_secs(5));
    #[cfg(unix)]
    let builder = if let Some(path) = socket_path {
        builder.unix_socket(path)
    } else {
        builder
    };
    let client = builder.build().expect("reqwest client build failed");

    let sse_builder = reqwest::Client::builder();
    #[cfg(unix)]
    let sse_builder = if let Some(path) = socket_path {
        sse_builder.unix_socket(path)
    } else {
        sse_builder
    };
    let sse_client = sse_builder
        .build()
        .expect("reqwest sse client build failed");

    let request_base_url = if socket_path.is_some() {
        "http://localhost".to_string()
    } else {
        base_url.clone()
    };

    let mut health_tick = tokio::time::interval(Duration::from_secs(2));
    health_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut status_tick = tokio::time::interval(Duration::from_secs(3));
    status_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut logs_tick = tokio::time::interval(Duration::from_secs(2));
    logs_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut prev_healthy = false;
    let mut prev_status_ok = true;
    let mut mcp_state: Option<StreamableHttpMcpClient> = None;

    // Backoff state for MCP session init failures.  After each consecutive
    // failure the wait grows (1 s → 3 s → 8 s → 20 s → 60 s cap) to avoid
    // flooding the server with 409/403 during startup.
    let mut init_fail_count: u32 = 0;
    let mut next_init_attempt = tokio::time::Instant::now();

    let mut active_log_file: Option<String> = None;
    let mut last_read_file: Option<String> = None;
    let mut last_read_offset: usize = 0;
    let mut last_file_size: u64 = 0;

    loop {
        tokio::select! {
            biased;

            maybe_cmd = cmd_rx.recv() => {
                match maybe_cmd {
                    Some(McpSourceCommand::SetActiveFile(file)) => {
                        if file != active_log_file {
                            active_log_file = file;
                            last_read_file = None;
                            last_read_offset = 0;
                            last_file_size = 0;
                        }
                    }
                    Some(McpSourceCommand::RefreshLogs) => {
                        if prev_healthy
                            && let Some(ref session) = mcp_state
                            && let Ok(files) = call_logs_list(session).await
                        {
                            send(&tx, SourceEvent::LogFilesUpdated { files }).await;
                        }
                    }
                    Some(McpSourceCommand::SetDaemonHealthy(healthy)) => {
                        let interval_secs = if healthy { 10 } else { 3 };
                        status_tick = tokio::time::interval(Duration::from_secs(interval_secs));
                        status_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    }
                    None => {
                        // The TUI is shutting down.  Delete the MCP session so the bridge
                        // decrements active_sessions immediately rather than waiting for the
                        // 5-second SSE-drop grace period, allowing the auto-spawned bridge to
                        // idle-exit promptly once no client is connected.
                        if let Some(ref session) = mcp_state {
                            session.delete_session(Duration::from_secs(2)).await;
                        }
                        break;
                    }
                }
            }

            _ = health_tick.tick() => {
                let healthy = check_health(&client, &request_base_url).await;
                if healthy != prev_healthy {
                    prev_healthy = healthy;
                    if healthy {
                        tracing::info!(
                            url = %base_url,
                            transport = %connection.transport_label(),
                            "TUI connected to bridge"
                        );
                    } else {
                        tracing::warn!(url = %base_url, "TUI bridge unreachable");
                    }
                    send(&tx, SourceEvent::HealthChanged { healthy }).await;
                    let msg = if healthy {
                        format!("Connected to {} ({})", base_url, connection.transport_label())
                    } else {
                        format!("Server unreachable: {base_url}")
                    };
                    send(&tx, SourceEvent::LogLine(LogEntry {
                        timestamp: chrono::Local::now(),
                        level: if healthy { LogLevel::Info } else { LogLevel::Warn },
                        message: msg,
                    })).await;
                    // Reset MCP session on disconnect; reset backoff on reconnect.
                    if !healthy {
                        mcp_state = None;
                        prev_status_ok = true;
                        send(&tx, SourceEvent::SandboxStatus { status: crate::state::SandboxAuthority::Unknown }).await;
                    } else {
                        // Fresh connection — allow immediate init attempt.
                        init_fail_count = 0;
                        next_init_attempt = tokio::time::Instant::now();
                    }
                }
            }

            _ = status_tick.tick() => {
                if !prev_healthy { continue; }

                // Ensure we have an MCP session, with exponential backoff after failures.
                if mcp_state.is_none() {
                    if tokio::time::Instant::now() < next_init_attempt {
                        // Still in backoff window — skip this tick.
                        continue;
                    }
                    match init_mcp_session(&client, &sse_client, &request_base_url, workspace_path.clone(), tx.clone()).await {
                        Ok(session) => {
                            init_fail_count = 0;
                            send(&tx, SourceEvent::SessionId { id: session.session_id().to_string() }).await;
                            // Fetch tools list once after connecting
                            if let Ok(tools) = call_tools_list(&session).await {
                                send(&tx, SourceEvent::ToolsListUpdated { tools }).await;
                            }
                            mcp_state = Some(session);
                        }
                        Err(e) => {
                            send(&tx, SourceEvent::SandboxStatus { status: crate::state::SandboxAuthority::Failed }).await;
                            init_fail_count = init_fail_count.saturating_add(1);
                            let backoff = backoff_secs(init_fail_count);
                            debug!("MCP init failed (attempt {init_fail_count}, retry in {backoff}s): {e:#}");
                            next_init_attempt = tokio::time::Instant::now()
                                + tokio::time::Duration::from_secs(backoff);
                        }
                    }
                }

                // Call status to get current operations
                if let Some(ref session) = mcp_state {
                    match call_status(&request_base_url, session).await {
                        Ok(StatusPoll::Ready(ops)) => {
                            prev_status_ok = true;
                            send(&tx, SourceEvent::OperationsUpdated { ops }).await;
                        }
                        Ok(StatusPoll::Initializing) => {
                            // Sandbox handshake still completing on the bridge.
                            // The session is valid — keep it and retry next tick.
                            // Tearing it down here would spawn a fresh session
                            // that races the same handshake and 409s again,
                            // looping forever so no tool call ever runs.
                            send(&tx, SourceEvent::SandboxStatus { status: crate::state::SandboxAuthority::Initializing }).await;
                        }
                        Err(e) => {
                            if prev_status_ok {
                                tracing::warn!(error = %e, "TUI status poll failed; resetting MCP session");
                                prev_status_ok = false;
                            } else {
                                debug!("status call failed (resetting session): {e:#}");
                            }
                            // Best-effort: tell the bridge to free this session now rather
                            // than leaving it to expire via the idle timeout. Without this,
                            // a transient status-poll hiccup abandons a session client-side
                            // while it stays alive server-side, and repeated hiccups can pile
                            // up enough orphaned sessions to blow through the server's
                            // concurrent-session cap.
                            session.delete_session(Duration::from_secs(2)).await;
                            mcp_state = None;
                            // Same backoff as init failures — a run of status hiccups
                            // must not re-init every 3-10s and keep growing the pile.
                            init_fail_count = init_fail_count.saturating_add(1);
                            next_init_attempt = tokio::time::Instant::now()
                                + tokio::time::Duration::from_secs(backoff_secs(init_fail_count));
                            // Clear the session id in the UI so it doesn't use the dead id for tool calls.
                            send(&tx, SourceEvent::SessionId { id: String::new() }).await;
                        }
                    }
                }
            }

            _ = logs_tick.tick() => {
                if !prev_healthy { continue; }
                if let Some(ref session) = mcp_state {
                    // 1. Poll logs list
                    let files = match call_logs_list(session).await {
                        Ok(f) => {
                            send(&tx, SourceEvent::LogFilesUpdated { files: f.clone() }).await;
                            f
                        }
                        Err(e) => {
                            debug!("logs_list call failed: {e:#}");
                            vec![]
                        }
                    };

                    if let Some(ref active_file) = active_log_file
                        && let Some(info) = files.iter().find(|f| &f.name == active_file)
                        && info.is_approved
                    {
                        let mut file_size_changed = false;
                        if last_read_file.as_ref() != Some(active_file) {
                            last_read_file = Some(active_file.clone());
                            last_read_offset = 0;
                            last_file_size = info.size_bytes;
                            file_size_changed = true;
                        } else if info.size_bytes < last_file_size {
                            last_read_offset = 0;
                            last_file_size = info.size_bytes;
                            file_size_changed = true;
                        } else if info.size_bytes > last_file_size {
                            last_file_size = info.size_bytes;
                            file_size_changed = true;
                        }

                        if file_size_changed || last_read_offset == 0 {
                            let limit = if last_read_offset == 0 { 500 } else { 100 };
                            match call_logs_read(
                                session,
                                active_file,
                                last_read_offset,
                                limit,
                            ).await {
                                Ok(content) => {
                                    let lines_count = if content.is_empty() { 0 } else { content.split('\n').count() };
                                    let append = last_read_offset > 0;
                                    last_read_offset += lines_count;
                                    send(&tx, SourceEvent::LogLinesUpdated {
                                        file: active_file.clone(),
                                        content,
                                        append,
                                    }).await;
                                }
                                Err(e) => {
                                    debug!("logs_read call failed for {active_file}: {e:#}");
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

// ─── MCP session ──────────────────────────────────────────────────────────────

/// Perform the full MCP Streamable-HTTP handshake via the shared client:
/// `initialize` → open the long-lived GET /mcp SSE stream BEFORE
/// `notifications/initialized` → answer the server's `roots/list` with the
/// workspace scope. The shared client answers `roots/list` itself; we
/// deliberately do NOT proactively POST an empty roots list — doing so races
/// the real reply and can lock the sandbox with zero scopes (every subsequent
/// tool call then 409s).
async fn init_mcp_session(
    client: &reqwest::Client,
    sse_client: &reqwest::Client,
    base_url: &str,
    workspace_path: Option<std::path::PathBuf>,
    tx: mpsc::Sender<SourceEvent>,
) -> Result<StreamableHttpMcpClient> {
    send(
        &tx,
        SourceEvent::SandboxStatus {
            status: crate::state::SandboxAuthority::Initializing,
        },
    )
    .await;
    let url = format!("{base_url}/mcp");

    // Server-pushed notifications (sandbox scope/failure above all) are
    // translated into SourceEvents by a dedicated task; the shared client's
    // SSE listener forwards every decoded event onto this channel.
    let (notif_tx, mut notif_rx) = mpsc::channel::<Value>(64);
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(value) = notif_rx.recv().await {
                handle_notification(&value, &tx).await;
            }
        });
    }

    // Determine the workspace root to answer roots/list with.
    let actual_path = workspace_path.unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    });

    let connector = Connector {
        mcp_url: url,
        post_client: client.clone(),
        sse_client: sse_client.clone(),
    };
    let opts = ConnectOptions {
        // clientInfo.name is load-bearing: the server keys `supports_progress`
        // and `request_budget` off it.
        client_name: "ahma-tui".to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        roots: vec![actual_path],
        notifications: Some(notif_tx),
        // Pre-unification value, preserved: 5 s for the SSE stream to open.
        sse_open_timeout: Duration::from_secs(5),
        // The TUI does not wait for the sandbox lock here (preserved): the
        // status poll treats the 409 gate as `StatusPoll::Initializing` and
        // keeps the session until the lock settles.
        sandbox_lock_timeout: None,
    };
    StreamableHttpMcpClient::connect(connector, opts).await
}

/// Build a `SandboxScopeInfo` from a `notifications/sandbox/configured`
/// `params` payload.
///
/// The server nests the full ScopeView under `params.scope` (SPEC R5.4: "the
/// configured notification carries the complete scope and its provenance").
/// Older emitters put `active`/`host`/`active_disclosure` at the top level of
/// params, so fall back there field-by-field.
fn parse_sandbox_scope(params: Option<&Value>) -> crate::state::SandboxScopeInfo {
    let scope_obj = params.and_then(|p| p.get("scope"));
    let field = |key: &str| -> Option<&Value> {
        scope_obj
            .and_then(|s| s.get(key))
            .or_else(|| params.and_then(|p| p.get(key)))
    };
    let str_field = |key: &str| field(key).and_then(Value::as_str).map(str::to_string);
    let paths_field = |key: &str| -> Vec<String> {
        field(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };

    crate::state::SandboxScopeInfo {
        write: paths_field("write"),
        read: paths_field("read"),
        tmp: field("tmp").and_then(Value::as_bool).unwrap_or(false),
        // A notification with no scope payload comes from a server that
        // only emits it after enforcing — assume enforced there rather
        // than rendering a false "DISABLED".
        enforced: field("enforced").and_then(Value::as_bool).unwrap_or(true),
        source: str_field("source"),
        active: str_field("active"),
        host: str_field("host"),
        disclosure: str_field("active_disclosure"),
        platform_note: str_field("platform_note"),
    }
}

/// Translate one server-pushed notification into `SourceEvent`s. `roots/list`
/// is answered by the shared client's SSE listener before it reaches here, so
/// this only reacts to the sandbox lifecycle notifications.
async fn handle_notification(value: &Value, tx: &mpsc::Sender<SourceEvent>) {
    let method = value.get("method").and_then(|m| m.as_str());

    if method == Some(SANDBOX_FAILED_METHOD) {
        let error = value
            .get("params")
            .and_then(|p| p.get("error"))
            .and_then(|e| e.as_str())
            .unwrap_or("unknown");
        warn!("Sandbox configuration failed: {}", error);
        send(
            tx,
            SourceEvent::SandboxFailed {
                error: error.to_string(),
            },
        )
        .await;
    }

    if method == Some(SANDBOX_CONFIGURED_METHOD) {
        debug!("Sandbox configured successfully!");
        let scope = parse_sandbox_scope(value.get("params"));

        // Classify the active-sandbox token once, here at the boundary. The
        // label is derived from this at the render edge — previously the label
        // *was* the event, and every consumer re-derived the classification from
        // its text.
        let status = crate::state::SandboxAuthority::from_token(
            scope.active.as_deref(),
            scope.host.as_deref(),
        );
        let not_sole_authority = !status.is_sole_authority();
        let disclosure_text = scope.disclosure.clone();
        send(tx, SourceEvent::SandboxScope { scope }).await;
        send(tx, SourceEvent::SandboxStatus { status }).await;

        // When ahma is not the sole authority, surface the loud, actionable
        // disclosure once in the log pane so the remediation is visible in-TUI —
        // not just in the server's stderr the user may never see. (The `/scope`
        // window keeps it visible persistently after this line scrolls away.)
        if not_sole_authority && let Some(text) = disclosure_text {
            send(
                tx,
                SourceEvent::LogLine(LogEntry {
                    timestamp: chrono::Local::now(),
                    level: LogLevel::Warn,
                    message: text,
                }),
            )
            .await;
        }
    }

    // `roots/list` is intentionally not handled here: the shared client's SSE
    // listener answers it with the workspace scope before forwarding the event.
}

// ─── Tool calls ───────────────────────────────────────────────────────────────

/// Invoke an MCP tool via `tools/call` (no 409 retries — the TUI polls on its
/// own cadence), decode the JSON-RPC response, and turn a JSON-RPC `error`
/// object (or a transport-level failure) into an `Err` tagged with the tool
/// name.
async fn call_tool_json(
    session: &StreamableHttpMcpClient,
    tool: &str,
    args: Value,
) -> Result<Value> {
    let resp = match session
        .call_tool(tool, args, ConflictRetryPolicy::NONE)
        .await?
    {
        ToolCallOutcome::Success(resp) => resp,
        ToolCallOutcome::SandboxInitializing { body } => {
            return Err(anyhow::anyhow!(
                "{tool} error: sandbox initializing: {body}"
            ));
        }
        ToolCallOutcome::HttpError { status, body } => {
            return Err(anyhow::anyhow!("{tool} failed: HTTP {status}: {body}"));
        }
    };

    if let Some(err) = resp.get("error") {
        let err_msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        return Err(anyhow::anyhow!("{tool} error: {err_msg}"));
    }

    Ok(resp)
}

/// Exponential backoff after consecutive MCP session failures, capped at 60 s:
/// 1 → 3 → 8 → 20 → 60.
fn backoff_secs(fail_count: u32) -> u64 {
    match fail_count {
        1 => 1,
        2 => 3,
        3 => 8,
        4 => 20,
        _ => 60,
    }
}

async fn call_tools_list(
    session: &StreamableHttpMcpClient,
) -> Result<Vec<crate::mcp_connections::ToolInfo>> {
    let tools = session.tools_list().await?;
    Ok(tools
        .into_iter()
        .map(|t| crate::mcp_connections::ToolInfo {
            name: t.name,
            description: t.description,
            input_schema: t.input_schema,
        })
        .collect())
}

fn status_call_error(url: &str, status: reqwest::StatusCode) -> anyhow::Error {
    let hint = match status.as_u16() {
        409 => "sandbox still initializing — complete MCP handshake (roots/list) before tools/call",
        403 => "forbidden — check bearer token, session id, or bridge auth settings",
        401 => "unauthorized — bearer token missing or invalid",
        404 => "session not found — MCP session may have expired; reconnect",
        _ => "unexpected HTTP status from status tool",
    };
    anyhow::anyhow!("status call to {url} returned HTTP {status} ({hint})")
}

/// Outcome of a `status` poll.
///
/// A `409 Conflict` from the bridge means the sandbox handshake is still
/// completing (the subprocess has not finished locking its scope yet). The MCP
/// session is valid and will become usable within milliseconds, so the caller
/// must keep it and retry on the next tick rather than tearing it down — doing
/// otherwise spawns a fresh session that races the same handshake and 409s
/// again, an infinite reset loop in which no tool call ever succeeds.
enum StatusPoll {
    Ready(Vec<Operation>),
    Initializing,
}

async fn call_status(base_url: &str, session: &StreamableHttpMcpClient) -> Result<StatusPoll> {
    // No in-call 409 retries (preserved): a CONFLICT means the sandbox is
    // still locking — keep the session and retry on the next status tick.
    match session
        .call_tool("status", json!({}), ConflictRetryPolicy::NONE)
        .await?
    {
        ToolCallOutcome::SandboxInitializing { .. } => Ok(StatusPoll::Initializing),
        ToolCallOutcome::HttpError { status, .. } => {
            Err(status_call_error(&format!("{base_url}/mcp"), status))
        }
        ToolCallOutcome::Success(val) => Ok(StatusPoll::Ready(parse_operations(&val))),
    }
}

// ─── Parsing helpers ──────────────────────────────────────────────────────────

fn parse_op_status(op_val: &Value) -> OpStatus {
    let status_str = op_val
        .get("status")
        .or_else(|| op_val.get("state"))
        .and_then(|v| v.as_str())
        .unwrap_or("Running");

    match status_str.to_lowercase().as_str() {
        "running" | "inprogress" | "in_progress" => OpStatus::Running,
        "pending" => OpStatus::Pending,
        "succeeded" | "success" | "completed" | "done" => OpStatus::Succeeded,
        "failed" | "error" => OpStatus::Failed,
        "cancelled" | "canceled" => OpStatus::Cancelled,
        "waiting" | "waiting_dependency" => OpStatus::Waiting,
        _ => OpStatus::Running,
    }
}

/// Parse an RFC-3339 timestamp field off `op_val`, converted to local time.
fn parse_rfc3339_field(op_val: &Value, key: &str) -> Option<chrono::DateTime<chrono::Local>> {
    let raw = op_val.get(key).and_then(|v| v.as_str())?;
    let dt = chrono::DateTime::parse_from_rfc3339(raw).ok()?;
    Some(dt.with_timezone(&chrono::Local))
}

fn parse_op_times(op_val: &Value, op: &mut Operation) {
    let Some(s_dt) = parse_rfc3339_field(op_val, "start_time") else {
        return;
    };
    op.started_time = s_dt;

    let now_local = chrono::Local::now();
    if now_local >= s_dt {
        let diff_secs = now_local.signed_duration_since(s_dt).num_seconds().max(0) as u64;
        op.started_at = Some(std::time::Instant::now() - Duration::from_secs(diff_secs));
    }

    let Some(e_dt) = parse_rfc3339_field(op_val, "end_time") else {
        return;
    };
    if e_dt < s_dt {
        return;
    }

    let duration_ms = e_dt.signed_duration_since(s_dt).num_milliseconds().max(0) as u64;
    op.duration_ms = Some(duration_ms);
    if let Some(start_inst) = op.started_at {
        op.completed_at = Some(start_inst + Duration::from_millis(duration_ms));
    }
}

/// Collect a JSON array field's string elements, skipping non-strings. `None`
/// when the field is absent or is not an array, so the caller can keep the
/// existing default rather than overwrite it with an empty list.
fn string_array_field<C: FromIterator<String>>(op_val: &Value, key: &str) -> Option<C> {
    let arr = op_val.get(key)?.as_array()?;
    Some(
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
    )
}

/// Parse one `status`-tool content item into an [`Operation`]. `None` when the
/// item's embedded text is not a JSON object or carries no `id` — the only two
/// required things. Every other field is optional and falls back to the
/// `Operation::new` default, so an older or newer server shape still yields a
/// usable row.
fn parse_operation(item: &Value) -> Option<Operation> {
    // The operation is JSON embedded in the item's `text` field.
    let text = item
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or_default();
    let op_val: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    if !op_val.is_object() {
        return None;
    }

    let id = op_val
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)?;
    let tool = op_val
        .get("tool")
        .or_else(|| op_val.get("tool_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let mut op = Operation::new(id, tool, parse_op_status(&op_val));
    op.started_at = None; // fallback until parse_op_times fills it in
    if let Some(cwd) = op_val.get("cwd").and_then(|v| v.as_str()) {
        op.cwd = Some(cwd.to_string());
    }
    if let Some(desc) = op_val.get("description").and_then(|v| v.as_str()) {
        op.description = desc.to_string();
    }
    if let Some(parent) = op_val.get("parent_id").and_then(|v| v.as_str()) {
        op.parent_id = Some(parent.to_string());
    }

    parse_op_times(&op_val, &mut op);

    if let Some(lines) = string_array_field(&op_val, "stdout_tail") {
        op.stdout_tail = lines;
    }
    if let Some(alerts) = string_array_field(&op_val, "alerts") {
        op.alerts = alerts;
    }

    Some(op)
}

fn parse_operations(val: &Value) -> Vec<Operation> {
    // The `status` tool returns content items; each is a JSON object
    // describing one operation.  We tolerate many shapes gracefully.
    let Some(Value::Array(content)) = val.pointer("/result/content") else {
        return vec![];
    };
    content.iter().filter_map(parse_operation).collect()
}

// ─── Utility helpers ──────────────────────────────────────────────────────────

async fn check_health(client: &reqwest::Client, base_url: &str) -> bool {
    let url = format!("{base_url}/health");
    match client.get(&url).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(e) => {
            debug!("health check failed: {e}");
            false
        }
    }
}

fn extract_http_base_url(connection: &ResolvedConnection) -> String {
    match &connection.transport {
        ResolvedTransport::Http(url) | ResolvedTransport::Http3(url) => url.clone(),
        #[cfg(unix)]
        ResolvedTransport::UnixSocket(path) => {
            // Retired (R-CFG1.2); use `ahma tui --connect <URL>` instead.
            ahma_common::config::warn_retired_env("AHMA_HTTP_URL");
            format!("unix://{}", path)
        }
    }
}

async fn send(tx: &mpsc::Sender<SourceEvent>, event: SourceEvent) {
    let _ = tx.send(event).await; // ignore channel-closed errors
}

async fn call_logs_list(session: &StreamableHttpMcpClient) -> Result<Vec<LogFileInfo>> {
    let resp = call_tool_json(session, "logs_list", json!({})).await?;

    let content_text = resp
        .pointer("/result/content/0/text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Invalid response format for logs_list"))?;

    let files: Vec<LogFileInfo> = serde_json::from_str(content_text)?;
    Ok(files)
}

async fn call_logs_read(
    session: &StreamableHttpMcpClient,
    file_name: &str,
    offset: usize,
    limit: usize,
) -> Result<String> {
    let args = json!({
        "file": file_name,
        "offset": offset,
        "limit": limit
    });
    let resp = call_tool_json(session, "logs_read", args).await?;

    let text = resp
        .pointer("/result/content/0/text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Invalid response format for logs_read"))?;

    Ok(text.to_string())
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ahma_common::timeouts::TestTimeouts;

    // The SSE framing helpers and encode_file_uri moved to `ahma_common`
    // (`sse` / `file_uri` modules) — their unit tests live there now.

    #[test]
    fn test_status_call_error_includes_actionable_hint() {
        let err = super::status_call_error("http://localhost/mcp", reqwest::StatusCode::CONFLICT);
        let msg = format!("{err:#}");
        assert!(msg.contains("409"));
        assert!(msg.contains("sandbox"));
    }

    // ── coverage batch: SSE/parse helpers + axum-mocked HTTP calls ─────────────
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::{get, post};
    use axum::{Json, Router};

    async fn spawn_test_server(router: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let port = listener.local_addr().expect("local_addr").port();
        let base = format!("http://127.0.0.1:{port}");
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        (base, handle)
    }

    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(TestTimeouts::scale_secs(5))
            .build()
            .expect("client build")
    }

    /// A shared-client session attached to `base` with a fixed test id —
    /// the handshake itself is covered by `ahma_http_mcp_client`'s tests.
    fn test_session(base: &str) -> StreamableHttpMcpClient {
        StreamableHttpMcpClient::attach(
            test_client(),
            format!("{base}/mcp"),
            "s",
            ahma_common::mcp_protocol::DEFAULT_NEGOTIATED_PROTOCOL_VERSION,
        )
    }

    #[test]
    fn mcp_status_call_error_403_401_404_other() {
        let m403 = format!(
            "{:#}",
            status_call_error("u", reqwest::StatusCode::FORBIDDEN)
        );
        assert!(m403.contains("forbidden"), "{m403}");
        let m401 = format!(
            "{:#}",
            status_call_error("u", reqwest::StatusCode::UNAUTHORIZED)
        );
        assert!(m401.contains("unauthorized"), "{m401}");
        let m404 = format!(
            "{:#}",
            status_call_error("u", reqwest::StatusCode::NOT_FOUND)
        );
        assert!(m404.contains("session not found"), "{m404}");
        let m500 = format!(
            "{:#}",
            status_call_error("u", reqwest::StatusCode::INTERNAL_SERVER_ERROR)
        );
        assert!(m500.contains("unexpected"), "{m500}");
    }

    #[test]
    fn mcp_extract_http_base_url_http_and_http3() {
        let http = ResolvedConnection {
            display_url: "d".to_string(),
            transport: ResolvedTransport::Http("http://h:1234".to_string()),
        };
        assert_eq!(extract_http_base_url(&http), "http://h:1234");

        let http3 = ResolvedConnection {
            display_url: "d".to_string(),
            transport: ResolvedTransport::Http3("http://h:5678".to_string()),
        };
        assert_eq!(extract_http_base_url(&http3), "http://h:5678");
    }

    /// A Unix-socket connection reports a `unix://` base URL. `AHMA_HTTP_URL`
    /// no longer redirects it (retired, R-CFG1.2) — so unlike before, this does
    /// not need to bail out when the variable happens to be set.
    #[cfg(unix)]
    #[test]
    fn mcp_extract_http_base_url_unix_socket_default() {
        let conn = ResolvedConnection {
            display_url: "d".to_string(),
            transport: ResolvedTransport::UnixSocket("/run/ahma.sock".to_string()),
        };
        assert_eq!(extract_http_base_url(&conn), "unix:///run/ahma.sock");
    }

    #[test]
    fn mcp_parse_op_status_all_variants() {
        let cases = [
            ("running", OpStatus::Running),
            ("inprogress", OpStatus::Running),
            ("in_progress", OpStatus::Running),
            ("pending", OpStatus::Pending),
            ("succeeded", OpStatus::Succeeded),
            ("success", OpStatus::Succeeded),
            ("completed", OpStatus::Succeeded),
            ("done", OpStatus::Succeeded),
            ("failed", OpStatus::Failed),
            ("error", OpStatus::Failed),
            ("cancelled", OpStatus::Cancelled),
            ("canceled", OpStatus::Cancelled),
            ("waiting", OpStatus::Waiting),
            ("waiting_dependency", OpStatus::Waiting),
            ("SUCCEEDED", OpStatus::Succeeded),
            ("weird-unknown", OpStatus::Running),
        ];
        for (s, expected) in cases {
            let v = json!({ "status": s });
            assert_eq!(parse_op_status(&v), expected, "status={s}");
        }
    }

    #[test]
    fn mcp_parse_op_status_state_fallback_and_missing() {
        assert_eq!(
            parse_op_status(&json!({ "state": "failed" })),
            OpStatus::Failed
        );
        assert_eq!(parse_op_status(&json!({})), OpStatus::Running);
    }

    #[test]
    fn mcp_parse_op_times_start_and_end() {
        let start = chrono::Local::now() - chrono::Duration::seconds(10);
        let end = start + chrono::Duration::milliseconds(5000);
        let v = json!({
            "start_time": start.to_rfc3339(),
            "end_time": end.to_rfc3339(),
        });
        let mut op = Operation::new("id", "tool", OpStatus::Succeeded);
        parse_op_times(&v, &mut op);
        assert_eq!(op.duration_ms, Some(5000));
        assert!(op.started_at.is_some());
        assert!(op.completed_at.is_some());
    }

    #[test]
    fn mcp_parse_op_times_start_only() {
        let start = chrono::Local::now() - chrono::Duration::seconds(3);
        let v = json!({ "start_time": start.to_rfc3339() });
        let mut op = Operation::new("id", "tool", OpStatus::Running);
        parse_op_times(&v, &mut op);
        assert!(op.started_at.is_some());
        assert!(op.duration_ms.is_none());
    }

    #[test]
    fn mcp_parse_op_times_none() {
        let mut op = Operation::new("id", "tool", OpStatus::Running);
        parse_op_times(&json!({}), &mut op);
        assert!(op.duration_ms.is_none());
    }

    #[test]
    fn mcp_parse_op_times_end_before_start_no_duration() {
        let start = chrono::Local::now() - chrono::Duration::seconds(3);
        let end = start - chrono::Duration::seconds(2);
        let v = json!({
            "start_time": start.to_rfc3339(),
            "end_time": end.to_rfc3339(),
        });
        let mut op = Operation::new("id", "tool", OpStatus::Running);
        parse_op_times(&v, &mut op);
        assert!(op.duration_ms.is_none());
    }

    #[test]
    fn mcp_parse_operations_empty_and_non_array() {
        assert!(parse_operations(&json!({})).is_empty());
        assert!(parse_operations(&json!({ "result": { "content": "nope" } })).is_empty());
    }

    #[test]
    fn mcp_parse_operations_full_op() {
        let op_text = json!({
            "id": "op-1",
            "tool": "cargo_build",
            "status": "running",
            "cwd": "/work/x",
            "description": "build it",
            "stdout_tail": ["line1", "line2"],
            "alerts": ["alert-a"],
        })
        .to_string();
        let val = json!({
            "result": { "content": [ { "type": "text", "text": op_text } ] }
        });
        let ops = parse_operations(&val);
        assert_eq!(ops.len(), 1);
        let op = &ops[0];
        assert_eq!(op.id, "op-1");
        assert_eq!(op.tool_name, "cargo_build");
        assert_eq!(op.status, OpStatus::Running);
        assert_eq!(op.cwd.as_deref(), Some("/work/x"));
        assert_eq!(op.description, "build it");
        assert!(op.stdout_tail.iter().any(|s| s == "line1"));
        assert!(op.alerts.iter().any(|s| s == "alert-a"));
    }

    #[test]
    fn mcp_parse_operations_tool_name_fallback_and_default() {
        let aliased =
            json!({ "id": "a", "tool_name": "npm_install", "status": "pending" }).to_string();
        let unknown = json!({ "id": "b", "status": "done" }).to_string();
        let val = json!({
            "result": { "content": [
                { "text": aliased },
                { "text": unknown },
            ] }
        });
        let ops = parse_operations(&val);
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].tool_name, "npm_install");
        assert_eq!(ops[0].status, OpStatus::Pending);
        assert_eq!(ops[1].tool_name, "unknown");
        assert_eq!(ops[1].status, OpStatus::Succeeded);
    }

    #[test]
    fn mcp_parse_operations_skips_malformed_items() {
        let valid = json!({ "id": "ok", "tool": "t", "status": "running" }).to_string();
        let val = json!({
            "result": { "content": [
                { "text": "not-json-at-all" },
                { "text": "[1,2,3]" },
                { "text": "{\"tool\":\"x\"}" },
                { "text": valid },
            ] }
        });
        let ops = parse_operations(&val);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].id, "ok");
    }

    #[tokio::test]
    async fn mcp_check_health_ok() {
        let router = Router::new().route("/health", get(|| async { StatusCode::OK }));
        let (base, handle) = spawn_test_server(router).await;
        let client = test_client();
        assert!(check_health(&client, &base).await);
        handle.abort();
    }

    #[tokio::test]
    async fn mcp_check_health_non_2xx_is_false() {
        let router = Router::new().route(
            "/health",
            get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
        );
        let (base, handle) = spawn_test_server(router).await;
        let client = test_client();
        assert!(!check_health(&client, &base).await);
        handle.abort();
    }

    #[tokio::test]
    async fn mcp_check_health_unreachable_is_false() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let base = format!("http://127.0.0.1:{port}");
        let client = test_client();
        assert!(!check_health(&client, &base).await);
    }

    #[tokio::test]
    async fn mcp_call_tools_list_parses_and_filters() {
        async fn handler(Json(_b): Json<Value>) -> Response {
            Json(json!({
                "jsonrpc": "2.0", "id": 1,
                "result": { "tools": [
                    { "name": "alpha", "description": "d", "inputSchema": {"type":"object"} },
                    { "name": "beta" },
                    { "description": "no name here" }
                ] }
            }))
            .into_response()
        }
        let router = Router::new().route("/mcp", post(handler));
        let (base, handle) = spawn_test_server(router).await;
        let session = test_session(&base);
        let tools = call_tools_list(&session).await.expect("tools list ok");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "alpha");
        assert_eq!(tools[0].description.as_deref(), Some("d"));
        assert_eq!(tools[1].name, "beta");
        assert!(tools[1].description.is_none());
        assert_eq!(tools[1].input_schema["type"], "object");
        handle.abort();
    }

    #[tokio::test]
    async fn mcp_call_tools_list_missing_result_is_empty() {
        async fn handler(Json(_b): Json<Value>) -> Response {
            Json(json!({ "jsonrpc": "2.0", "id": 1, "result": null })).into_response()
        }
        let router = Router::new().route("/mcp", post(handler));
        let (base, handle) = spawn_test_server(router).await;
        let session = test_session(&base);
        let tools = call_tools_list(&session).await.expect("ok");
        assert!(tools.is_empty());
        handle.abort();
    }

    #[tokio::test]
    async fn mcp_call_status_ready() {
        async fn handler(Json(_b): Json<Value>) -> Response {
            let op = json!({ "id": "op9", "tool": "t", "status": "running" }).to_string();
            Json(json!({
                "jsonrpc": "2.0", "id": 1,
                "result": { "content": [ { "type": "text", "text": op } ] }
            }))
            .into_response()
        }
        let router = Router::new().route("/mcp", post(handler));
        let (base, handle) = spawn_test_server(router).await;
        let session = test_session(&base);
        let poll = call_status(&base, &session).await.expect("status ok");
        match poll {
            StatusPoll::Ready(ops) => {
                assert_eq!(ops.len(), 1);
                assert_eq!(ops[0].id, "op9");
            }
            StatusPoll::Initializing => panic!("expected Ready"),
        }
        handle.abort();
    }

    #[tokio::test]
    async fn mcp_call_status_conflict_is_initializing() {
        let router = Router::new().route("/mcp", post(|| async { StatusCode::CONFLICT }));
        let (base, handle) = spawn_test_server(router).await;
        let session = test_session(&base);
        let poll = call_status(&base, &session)
            .await
            .expect("conflict maps to Initializing");
        assert!(matches!(poll, StatusPoll::Initializing));
        handle.abort();
    }

    #[tokio::test]
    async fn mcp_call_status_forbidden_is_error() {
        let router = Router::new().route("/mcp", post(|| async { StatusCode::FORBIDDEN }));
        let (base, handle) = spawn_test_server(router).await;
        let session = test_session(&base);
        let err = match call_status(&base, &session).await {
            Ok(_) => panic!("403 must error"),
            Err(e) => e,
        };
        assert!(format!("{err:#}").contains("forbidden"));
        handle.abort();
    }

    #[tokio::test]
    async fn mcp_call_logs_list_success() {
        async fn handler(Json(_b): Json<Value>) -> Response {
            let files = json!([{
                "name": "a.log", "path": "/x/a.log", "size_bytes": 10,
                "modified": null, "is_symlink": false,
                "symlink_target": null, "is_approved": true
            }])
            .to_string();
            Json(json!({
                "jsonrpc": "2.0", "id": 1,
                "result": { "content": [ { "type": "text", "text": files } ] }
            }))
            .into_response()
        }
        let router = Router::new().route("/mcp", post(handler));
        let (base, handle) = spawn_test_server(router).await;
        let session = test_session(&base);
        let files = call_logs_list(&session).await.expect("logs_list ok");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "a.log");
        assert!(files[0].is_approved);
        handle.abort();
    }

    #[tokio::test]
    async fn mcp_call_logs_list_error_field() {
        async fn handler(Json(_b): Json<Value>) -> Response {
            Json(json!({ "jsonrpc": "2.0", "id": 1, "error": { "message": "boom" } }))
                .into_response()
        }
        let router = Router::new().route("/mcp", post(handler));
        let (base, handle) = spawn_test_server(router).await;
        let session = test_session(&base);
        let err = call_logs_list(&session)
            .await
            .expect_err("error field must surface");
        assert!(format!("{err:#}").contains("boom"));
        handle.abort();
    }

    #[tokio::test]
    async fn mcp_call_logs_list_missing_content() {
        async fn handler(Json(_b): Json<Value>) -> Response {
            Json(json!({ "jsonrpc": "2.0", "id": 1, "result": {} })).into_response()
        }
        let router = Router::new().route("/mcp", post(handler));
        let (base, handle) = spawn_test_server(router).await;
        let session = test_session(&base);
        let err = call_logs_list(&session)
            .await
            .expect_err("missing content must error");
        assert!(format!("{err:#}").contains("Invalid response format"));
        handle.abort();
    }

    #[tokio::test]
    async fn mcp_call_logs_read_success() {
        async fn handler(Json(_b): Json<Value>) -> Response {
            Json(json!({
                "jsonrpc": "2.0", "id": 1,
                "result": { "content": [ { "type": "text", "text": "line1\nline2" } ] }
            }))
            .into_response()
        }
        let router = Router::new().route("/mcp", post(handler));
        let (base, handle) = spawn_test_server(router).await;
        let session = test_session(&base);
        let text = call_logs_read(&session, "a.log", 0, 100)
            .await
            .expect("logs_read ok");
        assert_eq!(text, "line1\nline2");
        handle.abort();
    }

    #[tokio::test]
    async fn mcp_call_logs_read_error_field() {
        async fn handler(Json(_b): Json<Value>) -> Response {
            Json(json!({ "jsonrpc": "2.0", "id": 1, "error": { "message": "nope" } }))
                .into_response()
        }
        let router = Router::new().route("/mcp", post(handler));
        let (base, handle) = spawn_test_server(router).await;
        let session = test_session(&base);
        let err = call_logs_read(&session, "a.log", 0, 100)
            .await
            .expect_err("error field must surface");
        assert!(format!("{err:#}").contains("nope"));
        handle.abort();
    }

    #[tokio::test]
    async fn mcp_call_logs_read_missing_content() {
        async fn handler(Json(_b): Json<Value>) -> Response {
            Json(json!({ "jsonrpc": "2.0", "id": 1, "result": {} })).into_response()
        }
        let router = Router::new().route("/mcp", post(handler));
        let (base, handle) = spawn_test_server(router).await;
        let session = test_session(&base);
        let err = call_logs_read(&session, "a.log", 0, 100)
            .await
            .expect_err("missing content must error");
        assert!(format!("{err:#}").contains("Invalid response format"));
        handle.abort();
    }

    #[tokio::test]
    async fn mcp_handle_sse_event_sandbox_failed() {
        let (tx, mut rx) = mpsc::channel::<SourceEvent>(8);
        let value = json!({
            "method": "notifications/sandbox/failed",
            "params": { "error": "scope locked" }
        });
        handle_notification(&value, &tx).await;
        match rx.try_recv().expect("event emitted") {
            SourceEvent::SandboxFailed { error } => assert_eq!(error, "scope locked"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mcp_handle_sse_event_sandbox_configured() {
        let (tx, mut rx) = mpsc::channel::<SourceEvent>(8);
        let value = json!({ "method": "notifications/sandbox/configured" });
        handle_notification(&value, &tx).await;
        // First event carries the (empty) scope; second the compact status.
        match rx.try_recv().expect("scope event emitted") {
            SourceEvent::SandboxScope { scope } => {
                assert!(scope.write.is_empty());
                assert!(scope.enforced, "bare notification assumes enforced");
            }
            other => panic!("unexpected: {other:?}"),
        }
        match rx.try_recv().expect("status event emitted") {
            SourceEvent::SandboxStatus { status } => {
                assert_eq!(status, crate::state::SandboxAuthority::Locked)
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// The real wire shape (SPEC R5.4): the full ScopeView nested under
    /// `params.scope` — write/read roots, tmp, enforcement, provenance, and the
    /// active-sandbox posture. This is what `Sandbox::scope_json` emits and what
    /// the bridge forwards verbatim; the top-level-params shape covered by the
    /// `nested_in_host` test below is the legacy fallback.
    #[tokio::test]
    async fn mcp_handle_sse_event_sandbox_configured_nested_scope_payload() {
        let (tx, mut rx) = mpsc::channel::<SourceEvent>(8);
        let value = json!({
            "method": "notifications/sandbox/configured",
            "params": {
                "scope": {
                    "enforced": true,
                    "write": ["/home/user/proj", "/home/user/cache"],
                    "read": ["/opt/toolchain"],
                    "tmp": true,
                    "source": "roots/list",
                    "active": "ahma",
                    "active_disclosure": "Sandbox: ahma kernel sandbox is ENFORCING"
                }
            }
        });
        handle_notification(&value, &tx).await;
        match rx.try_recv().expect("scope event emitted") {
            SourceEvent::SandboxScope { scope } => {
                assert_eq!(scope.write, vec!["/home/user/proj", "/home/user/cache"]);
                assert_eq!(scope.read, vec!["/opt/toolchain"]);
                assert!(scope.tmp);
                assert!(scope.enforced);
                assert_eq!(scope.source.as_deref(), Some("roots/list"));
                assert_eq!(scope.active.as_deref(), Some("ahma"));
            }
            other => panic!("unexpected: {other:?}"),
        }
        match rx.try_recv().expect("status event emitted") {
            SourceEvent::SandboxStatus { status } => {
                assert_eq!(status, crate::state::SandboxAuthority::Locked)
            }
            other => panic!("unexpected: {other:?}"),
        }
        // Sole authority => no disclosure log line.
        assert!(rx.try_recv().is_err(), "no extra events expected");
    }

    /// `disabled` nested under `params.scope` must reach the chip as
    /// UNSANDBOXED — this exact path was dead before the nested-shape parsing
    /// landed (the TUI read only top-level params, which the server never sent,
    /// so every configured notification rendered as [LOCKED]).
    #[tokio::test]
    async fn mcp_handle_sse_event_sandbox_configured_nested_disabled() {
        let (tx, mut rx) = mpsc::channel::<SourceEvent>(8);
        let value = json!({
            "method": "notifications/sandbox/configured",
            "params": {
                "scope": {
                    "enforced": false,
                    "write": ["/home/user/proj"],
                    "read": [],
                    "tmp": false,
                    "source": "explicit",
                    "active": "disabled",
                    "active_disclosure": "Sandbox: DISABLED — no kernel confinement is active."
                }
            }
        });
        handle_notification(&value, &tx).await;
        match rx.try_recv().expect("scope event emitted") {
            SourceEvent::SandboxScope { scope } => {
                assert!(!scope.enforced);
                assert_eq!(scope.active.as_deref(), Some("disabled"));
            }
            other => panic!("unexpected: {other:?}"),
        }
        match rx.try_recv().expect("status event emitted") {
            SourceEvent::SandboxStatus { status } => {
                assert_eq!(status, crate::state::SandboxAuthority::Unsandboxed)
            }
            other => panic!("unexpected: {other:?}"),
        }
        match rx.try_recv().expect("disclosure log line emitted") {
            SourceEvent::LogLine(entry) => {
                assert_eq!(entry.level, LogLevel::Warn);
                assert!(entry.message.contains("DISABLED"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mcp_handle_sse_event_sandbox_configured_nested_in_host() {
        let (tx, mut rx) = mpsc::channel::<SourceEvent>(8);
        let value = json!({
            "method": "notifications/sandbox/configured",
            "params": {
                "active": "ahma_nested_in_host",
                "host": "Claude Code",
                "active_disclosure": "Sandbox: ahma kernel sandbox is ENFORCING, but ... INTERSECTION ... run ahma as a configured MCP server ..."
            }
        });
        handle_notification(&value, &tx).await;
        // First event: the parsed scope (legacy top-level shape still honored).
        match rx.try_recv().expect("scope event emitted") {
            SourceEvent::SandboxScope { scope } => {
                assert_eq!(scope.active.as_deref(), Some("ahma_nested_in_host"));
                assert_eq!(scope.host.as_deref(), Some("Claude Code"));
            }
            other => panic!("unexpected: {other:?}"),
        }
        // Second event: the compact status-bar label naming the host.
        match rx.try_recv().expect("status event emitted") {
            SourceEvent::SandboxStatus { status } => assert_eq!(
                status,
                crate::state::SandboxAuthority::Nested(Some("Claude Code".to_string()))
            ),
            other => panic!("unexpected: {other:?}"),
        }
        // Third event: the loud remediation surfaced as a warning log line.
        match rx.try_recv().expect("log line emitted") {
            SourceEvent::LogLine(entry) => {
                assert_eq!(entry.level, LogLevel::Warn);
                assert!(
                    entry.message.contains("INTERSECTION"),
                    "log must carry the disclosure: {}",
                    entry.message
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mcp_handle_sse_event_other_method_noop() {
        let (tx, mut rx) = mpsc::channel::<SourceEvent>(8);
        let value = json!({ "method": "notifications/progress" });
        handle_notification(&value, &tx).await;
        assert!(rx.try_recv().is_err(), "no event should be emitted");
    }

    // The roots/list answering and SSE-listener mechanics moved to the shared
    // client (`ahma_http_mcp_client::streamable`) — their tests live there now.

    #[tokio::test]
    async fn mcp_init_session_success() {
        async fn post_handler(Json(body): Json<Value>) -> Response {
            let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
            if method == "initialize" {
                let mut headers = HeaderMap::new();
                headers.insert("mcp-session-id", "session-ok".parse().unwrap());
                (
                    StatusCode::OK,
                    headers,
                    Json(json!({ "jsonrpc": "2.0", "id": 1, "result": {} })),
                )
                    .into_response()
            } else {
                (
                    StatusCode::OK,
                    Json(json!({ "jsonrpc": "2.0", "result": null })),
                )
                    .into_response()
            }
        }
        async fn sse_handler() -> Response {
            (StatusCode::OK, [("content-type", "text/event-stream")], "").into_response()
        }
        let router = Router::new().route("/mcp", post(post_handler).get(sse_handler));
        let (base, handle) = spawn_test_server(router).await;
        let (tx, mut rx) = mpsc::channel::<SourceEvent>(16);

        let session = init_mcp_session(&test_client(), &reqwest::Client::new(), &base, None, tx)
            .await
            .expect("handshake succeeds");
        assert_eq!(session.session_id(), "session-ok");
        match rx.try_recv().expect("status event") {
            SourceEvent::SandboxStatus { status } => {
                assert_eq!(status, crate::state::SandboxAuthority::Initializing)
            }
            other => panic!("unexpected first event: {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test]
    async fn mcp_init_session_missing_session_id_errors() {
        async fn post_handler(Json(_b): Json<Value>) -> Response {
            Json(json!({ "jsonrpc": "2.0", "id": 1, "result": {} })).into_response()
        }
        let router = Router::new().route("/mcp", post(post_handler));
        let (base, handle) = spawn_test_server(router).await;
        let (tx, _rx) = mpsc::channel::<SourceEvent>(16);

        let err = match init_mcp_session(&test_client(), &reqwest::Client::new(), &base, None, tx)
            .await
        {
            Ok(_) => panic!("missing session id must error"),
            Err(e) => e,
        };
        assert!(format!("{err:#}").contains("mcp-session-id"));
        handle.abort();
    }

    #[tokio::test]
    async fn mcp_init_session_sse_failure_aborts_handshake() {
        async fn post_handler(Json(body): Json<Value>) -> Response {
            let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
            if method == "initialize" {
                let mut headers = HeaderMap::new();
                headers.insert("mcp-session-id", "session-x".parse().unwrap());
                (StatusCode::OK, headers, Json(json!({ "result": {} }))).into_response()
            } else {
                (StatusCode::OK, Json(json!({ "result": null }))).into_response()
            }
        }
        let router = Router::new().route(
            "/mcp",
            post(post_handler).get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
        );
        let (base, handle) = spawn_test_server(router).await;
        let (tx, _rx) = mpsc::channel::<SourceEvent>(16);

        let err = match init_mcp_session(&test_client(), &reqwest::Client::new(), &base, None, tx)
            .await
        {
            Ok(_) => panic!("SSE failure must abort handshake"),
            Err(e) => e,
        };
        assert!(format!("{err:#}").contains("SSE stream failed to open"));
        handle.abort();
    }
}
