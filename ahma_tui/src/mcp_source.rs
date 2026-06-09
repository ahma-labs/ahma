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
) -> mpsc::Sender<McpSourceCommand> {
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    tokio::spawn(async move {
        mcp_source_task(connection, tx, cmd_rx).await;
    });
    cmd_tx
}

// ─── Task implementation ──────────────────────────────────────────────────────

async fn mcp_source_task(
    connection: ResolvedConnection,
    tx: mpsc::Sender<SourceEvent>,
    mut cmd_rx: mpsc::Receiver<McpSourceCommand>,
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
    let mut mcp_state: Option<McpSession> = None;

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
                        break;
                    }
                }
            }

            _ = health_tick.tick() => {
                let healthy = check_health(&client, &request_base_url).await;
                if healthy != prev_healthy {
                    prev_healthy = healthy;
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
                    // Reset MCP session on disconnect
                    if !healthy { mcp_state = None; }
                }
            }

            _ = status_tick.tick() => {
                if !prev_healthy { continue; }

                // Ensure we have an MCP session
                if mcp_state.is_none() {
                    match init_mcp_session(&client, &request_base_url).await {
                        Ok(session) => {
                            send(&tx, SourceEvent::SessionId { id: session.id.clone() }).await;
                            // Fetch tools list once after connecting
                            if let Ok(tools) = call_tools_list(&client, &request_base_url, &session).await {
                                send(&tx, SourceEvent::ToolsListUpdated { tools }).await;
                            }
                            mcp_state = Some(session);
                        }
                        Err(e) => {
                            debug!("MCP init failed (will retry): {e:#}");
                        }
                    }
                }

                // Call status to get current operations
                if let Some(ref session) = mcp_state {
                    match call_status(&client, &request_base_url, session).await {
                        Ok(ops) => {
                            send(&tx, SourceEvent::OperationsUpdated { ops }).await;
                        }
                        Err(e) => {
                            debug!("status call failed: {e:#}");
                            mcp_state = None; // force re-init next tick
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

    fn next_req_id(&self) -> u64 {
        self.next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}

/// Perform the minimal MCP handshake required before tool calls:
/// `initialize` → get session ID, `notifications/initialized` → ready.
async fn init_mcp_session(client: &reqwest::Client, base_url: &str) -> Result<McpSession> {
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

    // Step 2: notifications/initialized
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

    // Step 3: respond to roots/list if the server requests it
    // (We send an empty roots list proactively to unblock sandbox init.)
    let roots_resp_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": { "roots": [] }
    });
    let _ = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("mcp-session-id", &session_id)
        .json(&roots_resp_body)
        .send()
        .await;

    Ok(McpSession::new(session_id))
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

async fn call_status(
    client: &reqwest::Client,
    base_url: &str,
    session: &McpSession,
) -> Result<Vec<Operation>> {
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

    if !resp.status().is_success() {
        let s = resp.status();
        warn!("status call returned HTTP {s}");
        return Ok(vec![]);
    }

    let val = resp.json::<Value>().await?;
    let ops = parse_operations(&val);
    Ok(ops)
}

// ─── Parsing helpers ──────────────────────────────────────────────────────────

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

            // Try to parse the text as JSON; fall back to raw text op list
            let op_val: Value = serde_json::from_str(text).unwrap_or(Value::Null);

            let id = op_val
                .get("id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| {
                    // Fallback: use the whole line as id
                    if !text.is_empty() {
                        Some(text.chars().take(12).collect())
                    } else {
                        None
                    }
                })?;

            let tool = op_val
                .get("tool")
                .or_else(|| op_val.get("tool_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            let status_str = op_val
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("Running");

            let status = match status_str.to_lowercase().as_str() {
                "running" | "inprogress" | "in_progress" => OpStatus::Running,
                "pending" => OpStatus::Pending,
                "succeeded" | "success" | "completed" | "done" => OpStatus::Succeeded,
                "failed" | "error" => OpStatus::Failed,
                "cancelled" | "canceled" => OpStatus::Cancelled,
                "waiting" | "waiting_dependency" => OpStatus::Waiting,
                _ => OpStatus::Running,
            };

            let mut op = Operation::new(id, tool, status);
            op.started_at = None; // fallback
            if let Some(cwd) = op_val.get("cwd").and_then(|v| v.as_str()) {
                op.cwd = Some(cwd.to_string());
            }
            if let Some(desc) = op_val.get("description").and_then(|v| v.as_str()) {
                op.description = desc.to_string();
            }

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
                    op.started_at =
                        Some(std::time::Instant::now() - Duration::from_secs(diff_secs));
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
