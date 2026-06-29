//! Async background task that feeds live data from the ahma server into the TUI.
//!
//! The task polls `/health` for connection state and (when the server is reachable)
//! performs a simplified MCP handshake so it can call `tools/list` and `status`.
//! All results are forwarded to the app event loop via an `mpsc` channel.

use std::time::Duration;

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
        status: String,
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
    AgentDone,
    AgentError {
        error: String,
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
    let mut mcp_state: Option<McpSession> = None;

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
                            && let Ok(files) = call_logs_list(&client, &request_base_url, session).await
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
                            let mcp_url = format!("{request_base_url}/mcp");
                            let _ = client
                                .delete(&mcp_url)
                                .header("mcp-session-id", session.id())
                                .timeout(Duration::from_secs(2))
                                .send()
                                .await;
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
                        send(&tx, SourceEvent::SandboxStatus { status: "UNKNOWN".to_string() }).await;
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
                            send(&tx, SourceEvent::SessionId { id: session.id.clone() }).await;
                            // Fetch tools list once after connecting
                            if let Ok(tools) = call_tools_list(&client, &request_base_url, &session).await {
                                send(&tx, SourceEvent::ToolsListUpdated { tools }).await;
                            }
                            mcp_state = Some(session);
                        }
                        Err(e) => {
                            send(&tx, SourceEvent::SandboxStatus { status: "FAILED".to_string() }).await;
                            init_fail_count = init_fail_count.saturating_add(1);
                            // Exponential backoff capped at 60 s: 1 → 3 → 8 → 20 → 60
                            let backoff_secs: u64 = match init_fail_count {
                                1 => 1,
                                2 => 3,
                                3 => 8,
                                4 => 20,
                                _ => 60,
                            };
                            debug!("MCP init failed (attempt {init_fail_count}, retry in {backoff_secs}s): {e:#}");
                            next_init_attempt = tokio::time::Instant::now()
                                + tokio::time::Duration::from_secs(backoff_secs);
                        }
                    }
                }

                // Call status to get current operations
                if let Some(ref session) = mcp_state {
                    match call_status(&client, &request_base_url, session).await {
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
                            send(&tx, SourceEvent::SandboxStatus { status: "INITIALIZING".to_string() }).await;
                        }
                        Err(e) => {
                            if prev_status_ok {
                                tracing::warn!(error = %e, "TUI status poll failed; resetting MCP session");
                                prev_status_ok = false;
                            } else {
                                debug!("status call failed (resetting session): {e:#}");
                            }
                            mcp_state = None;
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
                    let files = match call_logs_list(&client, &request_base_url, session).await {
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
                                &client,
                                &request_base_url,
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

struct McpSession {
    id: String,
    next_id: std::sync::atomic::AtomicU64,
}

impl McpSession {
    fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            next_id: std::sync::atomic::AtomicU64::new(10),
        }
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn next_req_id(&self) -> u64 {
        self.next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}

/// Perform the minimal MCP handshake required before tool calls:
/// `initialize` → get session ID, `notifications/initialized` → ready, and start long-lived GET /mcp SSE stream.
async fn init_mcp_session(
    client: &reqwest::Client,
    sse_client: &reqwest::Client,
    base_url: &str,
    workspace_path: Option<std::path::PathBuf>,
    tx: mpsc::Sender<SourceEvent>,
) -> Result<McpSession> {
    send(
        &tx,
        SourceEvent::SandboxStatus {
            status: "INITIALIZING".to_string(),
        },
    )
    .await;
    let url = format!("{base_url}/mcp");

    // Step 1: initialize
    let init_body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "roots": { "listChanged": false } },
            "clientInfo": { "name": "ahma-tui", "version": env!("CARGO_PKG_VERSION") }
        }
    });

    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&init_body)
        .send()
        .await?;

    let session_id = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("no mcp-session-id in initialize response"))?;

    // Step 1.5: Open the long-lived GET /mcp SSE stream BEFORE announcing readiness.
    //
    // The MCP Streamable-HTTP handshake requires the SSE return stream to be open
    // before `notifications/initialized`; otherwise the server has nowhere to
    // deliver its roots/list request and the session hangs with no response
    // (observed as repeated "Broadcasting to 0 SSE subscribers" on the bridge).
    // The listener signals `ready_rx` once the GET returns 2xx.
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let sse_client_clone = sse_client.clone();
    let client_clone = client.clone();
    let sse_url_clone = url.clone();
    let session_id_clone = session_id.clone();
    let workspace_path_clone = workspace_path.clone();
    let tx_clone = tx.clone();

    tokio::spawn(async move {
        if let Err(e) = run_sse_listener(
            sse_client_clone,
            client_clone,
            sse_url_clone,
            session_id_clone,
            workspace_path_clone,
            tx_clone,
            Some(ready_tx),
        )
        .await
        {
            warn!("TUI MCP SSE listener terminated with error: {e:#}");
        }
    });

    // Wait for the SSE stream to be established. A dropped sender (Err) means the
    // listener failed before the stream opened; a timeout means it never did.
    // Either way we abort the handshake so the caller retries with backoff rather
    // than proceeding into a half-open session that can never receive responses.
    match tokio::time::timeout(Duration::from_secs(5), ready_rx).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            return Err(anyhow::anyhow!(
                "MCP SSE stream failed to open; aborting handshake"
            ));
        }
        Err(_) => {
            return Err(anyhow::anyhow!(
                "timed out waiting for MCP SSE stream to open; aborting handshake"
            ));
        }
    }

    // Step 2: notifications/initialized (only now that the SSE return stream is live).
    let notif_body = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let _ = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("mcp-session-id", &session_id)
        .json(&notif_body)
        .send()
        .await;

    // The server's roots/list request is answered over the SSE stream by
    // `handle_sse_event`, which replies with the actual workspace scope. We
    // deliberately do NOT proactively POST an empty roots list here: doing so
    // races the real reply and can lock the sandbox with zero scopes (every
    // subsequent tool call then 409s).

    Ok(McpSession::new(session_id))
}

/// Helper function to encode a filesystem path as a file:// URI path component.
fn encode_file_uri(path: &std::path::Path) -> String {
    let mut path_str = path.to_string_lossy().into_owned();

    // Strip Windows extended-length prefix (\\?\) if present.
    if path_str.starts_with(r"\\?\") {
        path_str = path_str[4..].to_string();
    }

    // Normalise path separators to forward slashes.
    path_str = path_str.replace('\\', "/");

    let mut out = String::with_capacity(path_str.len() + 10);
    out.push_str("file://");

    #[cfg(target_os = "windows")]
    {
        let is_drive = path_str.len() >= 2
            && path_str.as_bytes()[0].is_ascii_alphabetic()
            && path_str.as_bytes()[1] == b':';
        if is_drive {
            out.push('/');
        }
    }

    out.push_str(&path_str);
    out
}

fn first_sse_event_boundary(buffer: &str) -> Option<(usize, usize)> {
    let lf = buffer.find("\n\n").map(|idx| (idx, 2));
    let crlf = buffer.find("\r\n\r\n").map(|idx| (idx, 4));

    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn pop_next_sse_event(buffer: &mut String) -> Option<String> {
    let (idx, delimiter_len) = first_sse_event_boundary(buffer)?;
    let raw_event = buffer[..idx].to_string();
    *buffer = buffer[idx + delimiter_len..].to_string();
    Some(raw_event)
}

fn event_data_to_json(raw_event: &str) -> Option<Value> {
    let data: Vec<&str> = raw_event
        .lines()
        .filter_map(|line| line.trim_end_matches('\r').strip_prefix("data:"))
        .map(str::trim)
        .collect();

    if data.is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(&data.join("\n")).ok()
}

async fn handle_sse_event(
    client: &reqwest::Client,
    mcp_url: &str,
    value: &Value,
    session_id: &str,
    workspace_path: Option<&std::path::Path>,
    tx: &mpsc::Sender<SourceEvent>,
) -> Result<()> {
    let method = value.get("method").and_then(|m| m.as_str());

    if method == Some("notifications/sandbox/failed") {
        let error = value
            .get("params")
            .and_then(|p| p.get("error"))
            .and_then(|e| e.as_str())
            .unwrap_or("unknown");
        warn!("Sandbox configuration failed: {}", error);
        send(
            tx,
            SourceEvent::SandboxStatus {
                status: "FAILED".to_string(),
            },
        )
        .await;
    }

    if method == Some("notifications/sandbox/configured") {
        debug!("Sandbox configured successfully!");
        send(
            tx,
            SourceEvent::SandboxStatus {
                status: "LOCKED".to_string(),
            },
        )
        .await;
    }

    if method == Some("roots/list") {
        let request_id = value
            .get("id")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("roots/list must include id"))?;

        // Determine workspace path to send
        let actual_path = workspace_path.map(|p| p.to_path_buf()).unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        });

        let roots_json = vec![json!({
            "uri": encode_file_uri(&actual_path),
            "name": actual_path.file_name().and_then(|n| n.to_str()).unwrap_or("workspace")
        })];

        let roots_response = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {
                "roots": roots_json
            }
        });

        debug!("Sending roots response: {:?}", roots_response);
        let _ = client
            .post(mcp_url)
            .header("Content-Type", "application/json")
            .header("mcp-session-id", session_id)
            .json(&roots_response)
            .send()
            .await;
    }

    Ok(())
}

async fn run_sse_listener(
    sse_client: reqwest::Client,
    client: reqwest::Client,
    sse_url: String,
    session_id: String,
    workspace_path: Option<std::path::PathBuf>,
    tx: mpsc::Sender<SourceEvent>,
    ready_tx: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<()> {
    let response = sse_client
        .get(&sse_url)
        .header("Accept", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("mcp-session-id", &session_id)
        .send()
        .await?;

    if !response.status().is_success() {
        // `ready_tx` is dropped here, which the caller observes as a failed
        // handshake (the SSE return stream never opened).
        return Err(anyhow::anyhow!(
            "SSE stream failed with HTTP {}",
            response.status()
        ));
    }

    // The GET /mcp SSE stream is now established server-side, so the server has
    // a channel to deliver roots/list and notifications. Signal the caller that
    // it is safe to send `notifications/initialized` (see `init_mcp_session`).
    if let Some(ready_tx) = ready_tx {
        let _ = ready_tx.send(());
    }

    let mut resp = response;
    let mut buffer = String::new();
    while let Ok(Some(bytes)) = resp.chunk().await {
        if let Ok(chunk_str) = std::str::from_utf8(&bytes) {
            buffer.push_str(chunk_str);
            while let Some(raw_event) = pop_next_sse_event(&mut buffer) {
                if let Some(json_val) = event_data_to_json(&raw_event)
                    && let Err(e) = handle_sse_event(
                        &client,
                        &sse_url,
                        &json_val,
                        &session_id,
                        workspace_path.as_deref(),
                        &tx,
                    )
                    .await
                {
                    debug!("Error handling SSE event: {:?}", e);
                }
            }
        }
    }

    Ok(())
}

// ─── Tool calls ───────────────────────────────────────────────────────────────

async fn call_tools_list(
    client: &reqwest::Client,
    base_url: &str,
    session: &McpSession,
) -> Result<Vec<crate::mcp_connections::ToolInfo>> {
    let url = format!("{base_url}/mcp");
    let req_id = session.next_req_id();
    let body = json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "method": "tools/list",
        "params": {}
    });

    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("mcp-session-id", &session.id)
        .json(&body)
        .send()
        .await?
        .json::<Value>()
        .await?;

    let tools = resp
        .pointer("/result/tools")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let name = t.get("name").and_then(|n| n.as_str())?;
                    let description = t
                        .get("description")
                        .and_then(|d| d.as_str())
                        .map(String::from);
                    let input_schema = t.get("inputSchema").cloned().unwrap_or(serde_json::json!({
                        "type": "object",
                        "additionalProperties": true
                    }));
                    Some(crate::mcp_connections::ToolInfo {
                        name: name.to_string(),
                        description,
                        input_schema,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(tools)
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

async fn call_status(
    client: &reqwest::Client,
    base_url: &str,
    session: &McpSession,
) -> Result<StatusPoll> {
    let url = format!("{base_url}/mcp");
    let req_id = session.next_req_id();
    let body = json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "method": "tools/call",
        "params": { "name": "status", "arguments": {} }
    });

    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("mcp-session-id", &session.id)
        .json(&body)
        .send()
        .await?;

    if resp.status() == reqwest::StatusCode::CONFLICT {
        // Transient: sandbox still locking. Keep the session and retry.
        return Ok(StatusPoll::Initializing);
    }

    if !resp.status().is_success() {
        let s = resp.status();
        return Err(status_call_error(&url, s));
    }

    let val = resp.json::<Value>().await?;
    let ops = parse_operations(&val);
    Ok(StatusPoll::Ready(ops))
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

fn parse_op_times(op_val: &Value, op: &mut Operation) {
    let mut started_dt = None;
    if let Some(st_str) = op_val.get("start_time").and_then(|v| v.as_str())
        && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(st_str)
    {
        let s_dt = dt.with_timezone(&chrono::Local);
        started_dt = Some(s_dt);
        op.started_time = s_dt;

        let now_local = chrono::Local::now();
        if now_local >= s_dt {
            let diff = now_local.signed_duration_since(s_dt);
            let diff_secs = diff.num_seconds().max(0) as u64;
            op.started_at = Some(std::time::Instant::now() - Duration::from_secs(diff_secs));
        }
    }

    if let Some(et_str) = op_val.get("end_time").and_then(|v| v.as_str())
        && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(et_str)
    {
        let e_dt = dt.with_timezone(&chrono::Local);
        if let Some(s_dt) = started_dt
            && e_dt >= s_dt
        {
            let duration = e_dt.signed_duration_since(s_dt);
            let duration_ms = duration.num_milliseconds().max(0) as u64;
            op.duration_ms = Some(duration_ms);
            if let Some(start_inst) = op.started_at {
                op.completed_at = Some(start_inst + Duration::from_millis(duration_ms));
            }
        }
    }
}

fn parse_operations(val: &Value) -> Vec<Operation> {
    // The `status` tool returns content items; each is a JSON object
    // describing one operation.  We tolerate many shapes gracefully.
    let content = match val.pointer("/result/content") {
        Some(Value::Array(arr)) => arr.clone(),
        _ => return vec![],
    };

    content
        .iter()
        .filter_map(|item| {
            // Try JSON-embedded text first
            let text = item
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or_default();

            // Try to parse the text as JSON; if it is not a JSON object, skip it.
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

            let status = parse_op_status(&op_val);

            let mut op = Operation::new(id, tool, status);
            op.started_at = None; // fallback
            if let Some(cwd) = op_val.get("cwd").and_then(|v| v.as_str()) {
                op.cwd = Some(cwd.to_string());
            }
            if let Some(desc) = op_val.get("description").and_then(|v| v.as_str()) {
                op.description = desc.to_string();
            }

            parse_op_times(&op_val, &mut op);

            if let Some(arr) = op_val.get("stdout_tail").and_then(|v| v.as_array()) {
                op.stdout_tail = arr
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
            }
            if let Some(arr) = op_val.get("alerts").and_then(|v| v.as_array()) {
                op.alerts = arr
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
            }

            Some(op)
        })
        .collect()
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
            std::env::var("AHMA_HTTP_URL").unwrap_or_else(|_| format!("unix://{}", path))
        }
    }
}

async fn send(tx: &mpsc::Sender<SourceEvent>, event: SourceEvent) {
    let _ = tx.send(event).await; // ignore channel-closed errors
}

async fn call_logs_list(
    client: &reqwest::Client,
    base_url: &str,
    session: &McpSession,
) -> Result<Vec<LogFileInfo>> {
    let url = format!("{base_url}/mcp");
    let req_id = session.next_req_id();
    let body = json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "method": "tools/call",
        "params": {
            "name": "logs_list",
            "arguments": {}
        }
    });

    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("mcp-session-id", &session.id)
        .json(&body)
        .send()
        .await?
        .json::<Value>()
        .await?;

    if let Some(err) = resp.get("error") {
        let err_msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        return Err(anyhow::anyhow!("logs_list error: {err_msg}"));
    }

    let content_text = resp
        .pointer("/result/content/0/text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Invalid response format for logs_list"))?;

    let files: Vec<LogFileInfo> = serde_json::from_str(content_text)?;
    Ok(files)
}

async fn call_logs_read(
    client: &reqwest::Client,
    base_url: &str,
    session: &McpSession,
    file_name: &str,
    offset: usize,
    limit: usize,
) -> Result<String> {
    let url = format!("{base_url}/mcp");
    let req_id = session.next_req_id();
    let body = json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "method": "tools/call",
        "params": {
            "name": "logs_read",
            "arguments": {
                "file": file_name,
                "offset": offset,
                "limit": limit
            }
        }
    });

    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("mcp-session-id", &session.id)
        .json(&body)
        .send()
        .await?
        .json::<Value>()
        .await?;

    if let Some(err) = resp.get("error") {
        let err_msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        return Err(anyhow::anyhow!("logs_read error: {err_msg}"));
    }

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

    // ── pop_next_sse_event ────────────────────────────────────────────────────

    #[test]
    fn test_pop_next_sse_event_lf_delimiter() {
        let mut buf = "event: ping\ndata: {}\n\nmore".to_string();
        let event = pop_next_sse_event(&mut buf);
        assert_eq!(event.as_deref(), Some("event: ping\ndata: {}"));
        assert_eq!(buf, "more");
    }

    #[test]
    fn test_pop_next_sse_event_crlf_delimiter() {
        let mut buf = "data: hello\r\n\r\nremainder".to_string();
        let event = pop_next_sse_event(&mut buf);
        assert_eq!(event.as_deref(), Some("data: hello"));
        assert_eq!(buf, "remainder");
    }

    #[test]
    fn test_pop_next_sse_event_no_delimiter_returns_none() {
        let mut buf = "data: incomplete".to_string();
        assert!(pop_next_sse_event(&mut buf).is_none());
        assert_eq!(buf, "data: incomplete");
    }

    #[test]
    fn test_pop_next_sse_event_empty_buf() {
        let mut buf = String::new();
        assert!(pop_next_sse_event(&mut buf).is_none());
    }

    #[test]
    fn test_pop_next_sse_event_multiple_events() {
        let mut buf = "data: 1\n\ndata: 2\n\n".to_string();
        assert_eq!(pop_next_sse_event(&mut buf).as_deref(), Some("data: 1"));
        assert_eq!(pop_next_sse_event(&mut buf).as_deref(), Some("data: 2"));
        assert!(pop_next_sse_event(&mut buf).is_none());
    }

    // ── event_data_to_json ────────────────────────────────────────────────────

    #[test]
    fn test_event_data_to_json_single_data_line() {
        let raw = "data: {\"method\":\"roots/list\",\"id\":1}";
        let v = event_data_to_json(raw).expect("should parse");
        assert_eq!(v["method"].as_str(), Some("roots/list"));
    }

    #[test]
    fn test_event_data_to_json_no_data_prefix_returns_none() {
        let raw = "event: ping\n: comment";
        assert!(event_data_to_json(raw).is_none());
    }

    #[test]
    fn test_event_data_to_json_invalid_json_returns_none() {
        let raw = "data: not-valid-json";
        assert!(event_data_to_json(raw).is_none());
    }

    #[test]
    fn test_event_data_to_json_strips_data_prefix() {
        let raw = "data: {\"ok\":true}";
        let v = event_data_to_json(raw).expect("should parse");
        assert_eq!(v["ok"].as_bool(), Some(true));
    }

    // ── encode_file_uri ───────────────────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn test_encode_file_uri_unix_absolute() {
        let path = std::path::Path::new("/home/user/project");
        let uri = encode_file_uri(path);
        assert_eq!(uri, "file:///home/user/project");
    }

    #[cfg(unix)]
    #[test]
    fn test_encode_file_uri_unix_nested() {
        let path = std::path::Path::new("/tmp/foo/bar baz");
        let uri = encode_file_uri(path);
        assert_eq!(uri, "file:///tmp/foo/bar baz");
    }

    #[test]
    fn test_status_call_error_includes_actionable_hint() {
        let err = super::status_call_error("http://localhost/mcp", reqwest::StatusCode::CONFLICT);
        let msg = format!("{err:#}");
        assert!(msg.contains("409"));
        assert!(msg.contains("sandbox"));
    }

    // ── coverage batch: SSE/parse helpers + axum-mocked HTTP calls ─────────────
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use tokio::sync::oneshot;

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
            .timeout(Duration::from_secs(5))
            .build()
            .expect("client build")
    }

    #[derive(Clone)]
    struct RecState {
        tx: tokio::sync::mpsc::UnboundedSender<Value>,
    }

    async fn record_handler(State(st): State<RecState>, Json(body): Json<Value>) -> StatusCode {
        let _ = st.tx.send(body);
        StatusCode::OK
    }

    #[test]
    fn mcp_boundary_lf_only() {
        assert_eq!(first_sse_event_boundary("a\n\nb"), Some((1, 2)));
    }

    #[test]
    fn mcp_boundary_crlf_only() {
        assert_eq!(first_sse_event_boundary("a\r\n\r\nb"), Some((1, 4)));
    }

    #[test]
    fn mcp_boundary_lf_before_crlf() {
        assert_eq!(first_sse_event_boundary("a\n\nb\r\n\r\n"), Some((1, 2)));
    }

    #[test]
    fn mcp_boundary_crlf_before_lf() {
        assert_eq!(first_sse_event_boundary("ab\r\n\r\nc\n\nd"), Some((2, 4)));
    }

    #[test]
    fn mcp_boundary_none() {
        assert_eq!(first_sse_event_boundary("abc"), None);
    }

    #[test]
    fn mcp_event_data_multiline_join() {
        // Two `data:` lines are concatenated with '\n' before JSON parsing.
        let raw = "data: {\"x\":\ndata: 5}";
        let v = event_data_to_json(raw).expect("multiline data joins into valid json");
        assert_eq!(v["x"].as_i64(), Some(5));
    }

    #[test]
    fn mcp_event_data_empty_returns_none() {
        assert!(event_data_to_json("").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn mcp_encode_file_uri_relative() {
        let uri = encode_file_uri(std::path::Path::new("foo/bar"));
        assert_eq!(uri, "file://foo/bar");
    }

    #[test]
    fn mcp_session_id_and_req_id_sequence() {
        let s = McpSession::new("sess-xyz");
        assert_eq!(s.id(), "sess-xyz");
        assert_eq!(s.next_req_id(), 10);
        assert_eq!(s.next_req_id(), 11);
        assert_eq!(s.next_req_id(), 12);
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

    #[cfg(unix)]
    #[test]
    fn mcp_extract_http_base_url_unix_socket_default() {
        if std::env::var("AHMA_HTTP_URL").is_ok() {
            return;
        }
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
        let session = McpSession::new("s");
        let tools = call_tools_list(&test_client(), &base, &session)
            .await
            .expect("tools list ok");
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
        let session = McpSession::new("s");
        let tools = call_tools_list(&test_client(), &base, &session)
            .await
            .expect("ok");
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
        let session = McpSession::new("s");
        let poll = call_status(&test_client(), &base, &session)
            .await
            .expect("status ok");
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
        let session = McpSession::new("s");
        let poll = call_status(&test_client(), &base, &session)
            .await
            .expect("conflict maps to Initializing");
        assert!(matches!(poll, StatusPoll::Initializing));
        handle.abort();
    }

    #[tokio::test]
    async fn mcp_call_status_forbidden_is_error() {
        let router = Router::new().route("/mcp", post(|| async { StatusCode::FORBIDDEN }));
        let (base, handle) = spawn_test_server(router).await;
        let session = McpSession::new("s");
        let err = match call_status(&test_client(), &base, &session).await {
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
        let session = McpSession::new("s");
        let files = call_logs_list(&test_client(), &base, &session)
            .await
            .expect("logs_list ok");
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
        let session = McpSession::new("s");
        let err = call_logs_list(&test_client(), &base, &session)
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
        let session = McpSession::new("s");
        let err = call_logs_list(&test_client(), &base, &session)
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
        let session = McpSession::new("s");
        let text = call_logs_read(&test_client(), &base, &session, "a.log", 0, 100)
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
        let session = McpSession::new("s");
        let err = call_logs_read(&test_client(), &base, &session, "a.log", 0, 100)
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
        let session = McpSession::new("s");
        let err = call_logs_read(&test_client(), &base, &session, "a.log", 0, 100)
            .await
            .expect_err("missing content must error");
        assert!(format!("{err:#}").contains("Invalid response format"));
        handle.abort();
    }

    #[tokio::test]
    async fn mcp_handle_sse_event_sandbox_failed() {
        let (tx, mut rx) = mpsc::channel::<SourceEvent>(8);
        let client = test_client();
        let value = json!({
            "method": "notifications/sandbox/failed",
            "params": { "error": "scope locked" }
        });
        handle_sse_event(&client, "http://127.0.0.1:9/mcp", &value, "sid", None, &tx)
            .await
            .expect("ok");
        match rx.try_recv().expect("event emitted") {
            SourceEvent::SandboxStatus { status } => assert_eq!(status, "FAILED"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mcp_handle_sse_event_sandbox_configured() {
        let (tx, mut rx) = mpsc::channel::<SourceEvent>(8);
        let client = test_client();
        let value = json!({ "method": "notifications/sandbox/configured" });
        handle_sse_event(&client, "http://127.0.0.1:9/mcp", &value, "sid", None, &tx)
            .await
            .expect("ok");
        match rx.try_recv().expect("event emitted") {
            SourceEvent::SandboxStatus { status } => assert_eq!(status, "LOCKED"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mcp_handle_sse_event_other_method_noop() {
        let (tx, mut rx) = mpsc::channel::<SourceEvent>(8);
        let client = test_client();
        let value = json!({ "method": "notifications/progress" });
        handle_sse_event(&client, "http://127.0.0.1:9/mcp", &value, "sid", None, &tx)
            .await
            .expect("ok");
        assert!(rx.try_recv().is_err(), "no event should be emitted");
    }

    #[tokio::test]
    async fn mcp_handle_sse_event_roots_list_missing_id_errors() {
        let (tx, _rx) = mpsc::channel::<SourceEvent>(8);
        let client = test_client();
        let value = json!({ "method": "roots/list" });
        let err = handle_sse_event(&client, "http://127.0.0.1:9/mcp", &value, "sid", None, &tx)
            .await
            .expect_err("missing id must error");
        assert!(format!("{err:#}").contains("must include id"));
    }

    #[tokio::test]
    async fn mcp_handle_sse_event_roots_list_posts_reply() {
        let (rec_tx, mut rec_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let state = RecState { tx: rec_tx };
        let router = Router::new()
            .route("/mcp", post(record_handler))
            .with_state(state);
        let (base, handle) = spawn_test_server(router).await;
        let mcp_url = format!("{base}/mcp");

        let (tx, _rx) = mpsc::channel::<SourceEvent>(8);
        let tmp = tempfile::TempDir::new().unwrap();
        let value = json!({ "jsonrpc": "2.0", "id": 7, "method": "roots/list", "params": {} });

        handle_sse_event(
            &test_client(),
            &mcp_url,
            &value,
            "sid-1",
            Some(tmp.path()),
            &tx,
        )
        .await
        .expect("roots/list handled");

        let recorded = rec_rx.try_recv().expect("roots reply posted");
        assert_eq!(recorded["id"], json!(7));
        assert!(recorded["result"]["roots"].is_array());
        let uri = recorded["result"]["roots"][0]["uri"].as_str().unwrap();
        assert!(uri.starts_with("file://"), "uri={uri}");
        handle.abort();
    }

    #[tokio::test]
    async fn mcp_run_sse_listener_processes_event_and_signals_ready() {
        async fn sse_roots_body() -> Response {
            let body =
                "data: {\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"roots/list\",\"params\":{}}\n\n";
            (
                StatusCode::OK,
                [("content-type", "text/event-stream")],
                body,
            )
                .into_response()
        }
        let (rec_tx, mut rec_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let state = RecState { tx: rec_tx };
        let router = Router::new()
            .route("/mcp", get(sse_roots_body).post(record_handler))
            .with_state(state);
        let (base, handle) = spawn_test_server(router).await;
        let sse_url = format!("{base}/mcp");

        let (tx, _rx) = mpsc::channel::<SourceEvent>(8);
        let (ready_tx, ready_rx) = oneshot::channel();
        let tmp = tempfile::TempDir::new().unwrap();

        run_sse_listener(
            test_client(),
            test_client(),
            sse_url,
            "sid-2".to_string(),
            Some(tmp.path().to_path_buf()),
            tx,
            Some(ready_tx),
        )
        .await
        .expect("listener completes ok");

        assert!(ready_rx.await.is_ok(), "ready signal fired");
        let recorded = rec_rx.try_recv().expect("roots reply posted by listener");
        assert_eq!(recorded["id"], json!(7));
        handle.abort();
    }

    #[tokio::test]
    async fn mcp_run_sse_listener_non_2xx_errors() {
        let router =
            Router::new().route("/mcp", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }));
        let (base, handle) = spawn_test_server(router).await;
        let sse_url = format!("{base}/mcp");
        let (tx, _rx) = mpsc::channel::<SourceEvent>(8);

        let err = run_sse_listener(
            test_client(),
            test_client(),
            sse_url,
            "sid-3".to_string(),
            None,
            tx,
            None,
        )
        .await
        .expect_err("non-2xx SSE must error");
        assert!(format!("{err:#}").contains("SSE stream failed"));
        handle.abort();
    }

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
        assert_eq!(session.id(), "session-ok");
        match rx.try_recv().expect("status event") {
            SourceEvent::SandboxStatus { status } => assert_eq!(status, "INITIALIZING"),
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
        assert!(format!("{err:#}").contains("no mcp-session-id"));
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
