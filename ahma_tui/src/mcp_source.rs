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
use crate::state::{AiActivityEntry, LogEntry, LogLevel, OpStatus, Operation};

// ─── Events emitted by this task ─────────────────────────────────────────────

/// Events produced by the MCP source task and consumed by the app event loop.
#[derive(Debug, Clone)]
pub enum SourceEvent {
    HealthChanged {
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
}

// ─── Entry point ─────────────────────────────────────────────────────────────

/// Spawn the background task.  The task runs until the channel is closed or
/// the server stays unreachable for an extended period.
pub fn spawn_mcp_source(connection: ResolvedConnection, tx: mpsc::Sender<SourceEvent>) {
    tokio::spawn(async move {
        mcp_source_task(connection, tx).await;
    });
}

// ─── Task implementation ──────────────────────────────────────────────────────

async fn mcp_source_task(connection: ResolvedConnection, tx: mpsc::Sender<SourceEvent>) {
    let base_url = extract_http_base_url(&connection);
    debug!("mcp_source: base_url={base_url}");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client build failed");

    let mut health_tick = tokio::time::interval(Duration::from_secs(2));
    health_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut status_tick = tokio::time::interval(Duration::from_secs(3));
    status_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut prev_healthy = false;
    let mut mcp_state: Option<McpSession> = None;

    loop {
        tokio::select! {
            biased;

            _ = health_tick.tick() => {
                let healthy = check_health(&client, &base_url).await;
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
                    match init_mcp_session(&client, &base_url).await {
                        Ok(session) => {
                            send(&tx, SourceEvent::SessionId { id: session.id.clone() }).await;
                            // Fetch tools list once after connecting
                            if let Ok(tools) = call_tools_list(&client, &base_url, &session).await {
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
                    match call_status(&client, &base_url, session).await {
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
            op.started_at = None; // elapsed is tracked server-side
            if let Some(cwd) = op_val.get("cwd").and_then(|v| v.as_str()) {
                op.cwd = Some(cwd.to_string());
            }
            if let Some(desc) = op_val.get("description").and_then(|v| v.as_str()) {
                op.description = desc.to_string();
            }
            if let Some(st_str) = op_val.get("start_time").and_then(|v| v.as_str())
                && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(st_str)
            {
                op.started_time = dt.with_timezone(&chrono::Local);
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
        ResolvedTransport::UnixSocket(_) => {
            // Unix socket transport: the HTTP bridge is typically at localhost:3000
            std::env::var("AHMA_HTTP_URL").unwrap_or_else(|_| "http://localhost:3000".to_string())
        }
    }
}

async fn send(tx: &mpsc::Sender<SourceEvent>, event: SourceEvent) {
    let _ = tx.send(event).await; // ignore channel-closed errors
}
