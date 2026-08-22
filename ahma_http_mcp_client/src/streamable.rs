//! Shared MCP Streamable-HTTP client.
//!
//! One implementation of the MCP Streamable-HTTP handshake hard invariant
//! (SPEC / AGENTS.md), shared by every in-workspace consumer that talks to the
//! ahma HTTP bridge (or an external Streamable-HTTP MCP server):
//!
//! 1. `initialize` (no session header) → read the `mcp-session-id` header,
//! 2. open the GET `/mcp` SSE return stream **before** `notifications/initialized`,
//! 3. send `notifications/initialized`,
//! 4. answer the server's `roots/list` request over the SSE stream with the
//!    caller-supplied roots (same JSON-RPC id),
//! 5. only then `tools/call` — and a `tools/call` before the sandbox lock
//!    settles returns HTTP 409 with JSON-RPC `-32001`, which callers poll or
//!    retry through per their [`ConflictRetryPolicy`].
//!
//! What genuinely differs per consumer stays parameterized:
//! `clientInfo.name`/`version` (the server keys `supports_progress` and
//! `request_budget` off the client name — never normalize it), the roots to
//! answer with, notification delivery, and every timeout (callers pass
//! explicit durations; see `ahma_common::timeouts` for the semantic
//! categories — nothing is hardcoded here).

use ahma_common::file_uri::encode_file_uri;
use ahma_common::mcp_methods::{INITIALIZED_METHOD, ROOTS_LIST_METHOD, SANDBOX_CONFIGURED_METHOD};
use ahma_common::mcp_protocol::{MCP_PROTOCOL_VERSION_HEADER, negotiated_protocol_version};
use ahma_common::sse::{event_data_to_json, pop_next_sse_event};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// The HTTP header carrying the MCP session id.
pub const SESSION_ID_HEADER: &str = "mcp-session-id";

/// MCP protocol version this client announces in `initialize`.
///
/// The `initialize` response may negotiate down to an older revision (see
/// [`ahma_common::mcp_protocol`]); the version actually in effect for a
/// session is [`StreamableHttpMcpClient::protocol_version`], not this
/// constant.
pub const PROTOCOL_VERSION: &str = ahma_common::mcp_protocol::REQUESTED_PROTOCOL_VERSION;

/// How to reach the server: the full `/mcp` endpoint URL plus the reqwest
/// clients to use. Callers construct the clients themselves so
/// consumer-specific concerns (per-base-URL caching, Unix-socket binding,
/// request timeouts) stay with the consumer. `sse_client` must have **no
/// request timeout** — the SSE stream is long-lived by design.
#[derive(Clone)]
pub struct Connector {
    /// Full MCP endpoint URL, e.g. `http://127.0.0.1:3000/mcp`.
    pub mcp_url: String,
    /// Client for JSON-RPC POSTs (may carry a request timeout).
    pub post_client: reqwest::Client,
    /// Client for the long-lived GET SSE stream (must not time out).
    pub sse_client: reqwest::Client,
}

/// Parameters for the full handshake ([`StreamableHttpMcpClient::connect`]).
pub struct ConnectOptions {
    /// `clientInfo.name`. The server keys real behaviour off this
    /// (`supports_progress`, `request_budget`) — pass the consumer's own name.
    pub client_name: String,
    /// `clientInfo.version`.
    pub client_version: String,
    /// Roots to answer the server's `roots/list` with. Each becomes one root
    /// with a `file://` URI (via `encode_file_uri`) named after its final path
    /// component (or "workspace").
    pub roots: Vec<PathBuf>,
    /// Every JSON value decoded from the SSE stream is forwarded here (when
    /// set), including the events this client also handles internally
    /// (`roots/list`, `notifications/sandbox/configured`).
    pub notifications: Option<mpsc::Sender<Value>>,
    /// How long to wait for the SSE return stream to open (2xx on the GET)
    /// before aborting the handshake. A handshake that proceeds without the
    /// stream leaves the session unable to receive `roots/list` and every
    /// `tools/call` 409s forever.
    pub sse_open_timeout: Duration,
    /// When set, wait up to this long for `notifications/sandbox/configured`
    /// after `notifications/initialized`. A timeout is **non-fatal** (logged,
    /// then proceed): the SSE listener keeps running and callers retry
    /// `tools/call` through the 409 gate.
    pub sandbox_lock_timeout: Option<Duration>,
}

/// Retry policy for the sandbox gate: HTTP 409 (JSON-RPC `-32001`) on
/// `tools/call` while the session's sandbox lock has not settled yet.
#[derive(Debug, Clone, Copy)]
pub struct ConflictRetryPolicy {
    /// How many times to re-send after a 409 before giving up.
    pub max_retries: u32,
    /// Delay between retries.
    pub delay: Duration,
}

impl ConflictRetryPolicy {
    /// No in-call retries: the first 409 is surfaced immediately as
    /// [`ToolCallOutcome::SandboxInitializing`] (poll-style consumers keep the
    /// session and try again on their own cadence).
    pub const NONE: Self = Self {
        max_retries: 0,
        delay: Duration::ZERO,
    };
}

/// Outcome of a `tools/call` after the [`ConflictRetryPolicy`] was applied.
#[derive(Debug)]
pub enum ToolCallOutcome {
    /// The final response was still HTTP 409 — the sandbox gate is closed.
    /// The session remains valid; retry later.
    SandboxInitializing {
        /// Response body (typically the JSON-RPC `-32001` error object).
        body: String,
    },
    /// A non-2xx response other than the sandbox gate.
    HttpError {
        status: reqwest::StatusCode,
        body: String,
    },
    /// 2xx with the parsed JSON-RPC response body (which may itself carry a
    /// JSON-RPC `error` member — that is the caller's to interpret).
    Success(Value),
}

/// One tool descriptor from `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

/// A connected MCP Streamable-HTTP session.
///
/// Dropping this value does **not** end the session or the SSE listener task:
/// the listener is detached and lives until the server closes the stream (or
/// the process exits), matching how consumers cache bare session ids. Call
/// [`delete_session`](Self::delete_session) to end the session eagerly.
pub struct StreamableHttpMcpClient {
    post_client: reqwest::Client,
    mcp_url: String,
    session_id: String,
    /// The version the server answered in `initialize` (2025-06-18
    /// `MCP-Protocol-Version` negotiation) — echoed on every subsequent
    /// request via [`MCP_PROTOCOL_VERSION_HEADER`].
    protocol_version: String,
    next_id: AtomicU64,
}

impl std::fmt::Debug for StreamableHttpMcpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamableHttpMcpClient")
            .field("mcp_url", &self.mcp_url)
            .field("session_id", &self.session_id)
            .field("protocol_version", &self.protocol_version)
            .finish_non_exhaustive()
    }
}

impl StreamableHttpMcpClient {
    /// Full MCP Streamable-HTTP handshake (steps 1–4 of the hard invariant),
    /// including SSE-before-initialized ordering and `roots/list` answering.
    pub async fn connect(connector: Connector, opts: ConnectOptions) -> Result<Self> {
        let (session_id, protocol_version) = initialize_session(
            &connector.post_client,
            &connector.mcp_url,
            &opts.client_name,
            &opts.client_version,
        )
        .await?;

        // Open the long-lived GET SSE stream BEFORE announcing readiness: the
        // stream is the only channel over which the server can deliver its
        // roots/list request; sending notifications/initialized first leaves
        // the session hanging with nowhere to deliver it.
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
        {
            let connector = connector.clone();
            let session_id = session_id.clone();
            let protocol_version = protocol_version.clone();
            let roots = opts.roots.clone();
            let notifications = opts.notifications.clone();
            tokio::spawn(async move {
                if let Err(e) = run_sse_listener(
                    connector,
                    session_id,
                    protocol_version,
                    roots,
                    notifications,
                    ready_tx,
                    Some(locked_tx),
                )
                .await
                {
                    warn!("MCP SSE listener ended with error: {e:#}");
                }
            });
        }

        // A dropped sender (Err) means the listener failed before the stream
        // opened; a timeout means it never did. Either way abort the handshake
        // so the caller retries rather than proceeding into a half-open
        // session that can never receive roots/list (and thus always 409s).
        match tokio::time::timeout(opts.sse_open_timeout, ready_rx).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                return Err(anyhow!("MCP SSE stream failed to open; aborting handshake"));
            }
            Err(_) => {
                return Err(anyhow!(
                    "timed out waiting for MCP SSE stream to open; aborting handshake"
                ));
            }
        }

        // notifications/initialized — only now that the return stream is live.
        // Failures are deliberately ignored (fire-and-forget), matching every
        // pre-unification consumer.
        let _ = connector
            .post_client
            .post(&connector.mcp_url)
            .header("Content-Type", "application/json")
            .header(SESSION_ID_HEADER, &session_id)
            .header(MCP_PROTOCOL_VERSION_HEADER, &protocol_version)
            .json(&json!({ "jsonrpc": "2.0", "method": INITIALIZED_METHOD }))
            .send()
            .await;

        // Optionally wait for the sandbox-lock confirmation. Non-fatal on
        // timeout: the listener keeps running and tools/call retries on 409.
        if let Some(lock_timeout) = opts.sandbox_lock_timeout {
            match tokio::time::timeout(lock_timeout, locked_rx).await {
                Ok(Ok(())) => {}
                _ => {
                    warn!(
                        session_id = %session_id,
                        "MCP sandbox lock not confirmed within timeout; proceeding (tools/call will retry on 409)"
                    );
                }
            }
        }

        Ok(Self {
            post_client: connector.post_client,
            mcp_url: connector.mcp_url,
            session_id,
            protocol_version,
            next_id: AtomicU64::new(FIRST_REQUEST_ID),
        })
    }

    /// Minimal handshake: `initialize` + `notifications/initialized`, with
    /// **no** SSE stream and **no** roots answering. For MCP servers that do
    /// not gate `tools/call` on `roots/list` (external, non-ahma servers).
    pub async fn connect_minimal(
        connector: Connector,
        client_name: &str,
        client_version: &str,
    ) -> Result<Self> {
        let (session_id, protocol_version) = initialize_session(
            &connector.post_client,
            &connector.mcp_url,
            client_name,
            client_version,
        )
        .await?;

        // Fire-and-forget, matching pre-unification consumers.
        let _ = connector
            .post_client
            .post(&connector.mcp_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header(SESSION_ID_HEADER, &session_id)
            .header(MCP_PROTOCOL_VERSION_HEADER, &protocol_version)
            .json(&json!({ "jsonrpc": "2.0", "method": INITIALIZED_METHOD }))
            .send()
            .await;

        Ok(Self {
            post_client: connector.post_client,
            mcp_url: connector.mcp_url,
            session_id,
            protocol_version,
            next_id: AtomicU64::new(FIRST_REQUEST_ID),
        })
    }

    /// Attach to an already-negotiated session (a cached session id, or one
    /// negotiated by another component such as the TUI's MCP source). No
    /// handshake is performed.
    ///
    /// `protocol_version` is the version that session's own `initialize`
    /// negotiated; pass [`ahma_common::mcp_protocol::DEFAULT_NEGOTIATED_PROTOCOL_VERSION`]
    /// if the caller never captured it.
    pub fn attach(
        post_client: reqwest::Client,
        mcp_url: impl Into<String>,
        session_id: impl Into<String>,
        protocol_version: impl Into<String>,
    ) -> Self {
        Self {
            post_client,
            mcp_url: mcp_url.into(),
            session_id: session_id.into(),
            protocol_version: protocol_version.into(),
            next_id: AtomicU64::new(FIRST_REQUEST_ID),
        }
    }

    /// The negotiated session id.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The protocol version negotiated at `initialize` (or supplied to
    /// [`Self::attach`]), echoed on every subsequent request via
    /// `MCP-Protocol-Version`.
    pub fn protocol_version(&self) -> &str {
        &self.protocol_version
    }

    /// The full `/mcp` endpoint URL.
    pub fn mcp_url(&self) -> &str {
        &self.mcp_url
    }

    /// POST one JSON-RPC request with the session header and a fresh
    /// monotonically increasing request id, returning the raw HTTP response.
    pub async fn post_json_rpc(&self, method: &str, params: Value) -> Result<reqwest::Response> {
        let req_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": method,
            "params": params
        });
        self.post_client
            .post(&self.mcp_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header(SESSION_ID_HEADER, &self.session_id)
            .header(MCP_PROTOCOL_VERSION_HEADER, &self.protocol_version)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("{method} request to {} failed", self.mcp_url))
    }

    /// Invoke `tools/call`, applying `retry` to the HTTP 409 sandbox gate.
    pub async fn call_tool(
        &self,
        tool: &str,
        arguments: Value,
        retry: ConflictRetryPolicy,
    ) -> Result<ToolCallOutcome> {
        let params = json!({ "name": tool, "arguments": arguments });
        let mut attempt: u32 = 0;
        let resp = loop {
            let resp = self.post_json_rpc("tools/call", params.clone()).await?;
            if resp.status() == reqwest::StatusCode::CONFLICT && attempt < retry.max_retries {
                attempt += 1;
                tokio::time::sleep(retry.delay).await;
                continue;
            }
            break resp;
        };

        let status = resp.status();
        if status == reqwest::StatusCode::CONFLICT {
            let body = resp.text().await.unwrap_or_default();
            return Ok(ToolCallOutcome::SandboxInitializing { body });
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Ok(ToolCallOutcome::HttpError { status, body });
        }
        let value = resp
            .json::<Value>()
            .await
            .with_context(|| format!("failed to parse tools/call response for {tool}"))?;
        Ok(ToolCallOutcome::Success(value))
    }

    /// Fetch and parse `tools/list`. Tolerant of a missing/odd `result`
    /// (yields an empty list); errors on non-2xx HTTP status.
    pub async fn tools_list(&self) -> Result<Vec<ToolDescriptor>> {
        let resp = self.post_json_rpc("tools/list", json!({})).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("tools/list failed: HTTP {status}: {body}"));
        }
        let val = resp
            .json::<Value>()
            .await
            .context("failed to parse tools/list response")?;
        Ok(parse_tools_list(&val))
    }

    /// Best-effort `DELETE` of the session so the server frees it immediately
    /// instead of waiting for its idle/SSE-drop grace period.
    pub async fn delete_session(&self, timeout: Duration) {
        let _ = self
            .post_client
            .delete(&self.mcp_url)
            .header(SESSION_ID_HEADER, &self.session_id)
            .header(MCP_PROTOCOL_VERSION_HEADER, &self.protocol_version)
            .timeout(timeout)
            .send()
            .await;
    }
}

/// First request id used after `initialize` (which always uses id 1). Starting
/// above a small gap keeps handshake ids visually distinct in wire logs.
const FIRST_REQUEST_ID: u64 = 10;

/// Parse a `tools/list` JSON-RPC response body into descriptors. Items
/// without a `name` are skipped; a missing `inputSchema` falls back to the
/// permissive object schema.
pub fn parse_tools_list(val: &Value) -> Vec<ToolDescriptor> {
    val.pointer("/result/tools")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let name = t.get("name").and_then(Value::as_str)?;
                    let description = t
                        .get("description")
                        .and_then(Value::as_str)
                        .map(String::from);
                    let input_schema = t.get("inputSchema").cloned().unwrap_or(json!({
                        "type": "object",
                        "additionalProperties": true
                    }));
                    Some(ToolDescriptor {
                        name: name.to_string(),
                        description,
                        input_schema,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Step 1 of the handshake: POST `initialize` (no session header) and extract
/// the `mcp-session-id` response header.
///
/// The error path is deliberately diagnostic: the header is absent for auth
/// failures (401), session limits (429), init-forward errors (500), and for
/// requests that reached something that is not an MCP bridge at all — a bare
/// "missing header" message hides all of those, so surface status + a bounded
/// body snippet.
async fn initialize_session(
    client: &reqwest::Client,
    mcp_url: &str,
    client_name: &str,
    client_version: &str,
) -> Result<(String, String)> {
    let init_body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "roots": { "listChanged": false } },
            "clientInfo": { "name": client_name, "version": client_version }
        }
    });

    let resp = client
        .post(mcp_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&init_body)
        .send()
        .await
        .with_context(|| format!("initialize request to {mcp_url} failed"))?;

    let Some(sid) = resp
        .headers()
        .get(SESSION_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
    else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(500).collect();
        let body_note = if snippet.trim().is_empty() {
            "<empty body>".to_string()
        } else {
            snippet
        };
        return Err(anyhow!(
            "No {SESSION_ID_HEADER} header in initialize response (HTTP {status}). \
             This usually means the request reached something other than an MCP \
             Streamable-HTTP server — check auth, session limits, and that the URL \
             is correct. Response body: {body_note}"
        ));
    };

    // The negotiated version is in the `initialize` result body, not a
    // header — read it before the response is consumed further.
    let init_response = resp.json::<Value>().await.unwrap_or(Value::Null);
    let protocol_version = negotiated_protocol_version(&init_response);

    Ok((sid, protocol_version))
}

/// Long-lived SSE listener for a session: opens the GET stream, signals
/// `ready_tx` once it is established (2xx), answers `roots/list` with the
/// supplied roots, signals `locked_tx` on `notifications/sandbox/configured`,
/// and forwards every decoded event to `notifications`. Runs until the server
/// closes the stream.
async fn run_sse_listener(
    connector: Connector,
    session_id: String,
    protocol_version: String,
    roots: Vec<PathBuf>,
    notifications: Option<mpsc::Sender<Value>>,
    ready_tx: tokio::sync::oneshot::Sender<()>,
    mut locked_tx: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<()> {
    let response = connector
        .sse_client
        .get(&connector.mcp_url)
        .header("Accept", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header(SESSION_ID_HEADER, &session_id)
        .send()
        .await
        .context("SSE GET failed")?;

    if !response.status().is_success() {
        // `ready_tx` is dropped here, which the caller observes as a failed
        // handshake (the SSE return stream never opened).
        return Err(anyhow!("SSE stream failed with HTTP {}", response.status()));
    }

    // The stream is established server-side, so the server has a channel to
    // deliver roots/list: it is now safe to send notifications/initialized.
    let _ = ready_tx.send(());

    let mut resp = response;
    let mut buffer = String::new();
    while let Ok(Some(bytes)) = resp.chunk().await {
        let Ok(chunk_str) = std::str::from_utf8(&bytes) else {
            continue;
        };
        buffer.push_str(chunk_str);
        while let Some(raw_event) = pop_next_sse_event(&mut buffer) {
            let Some(value) = event_data_to_json(&raw_event) else {
                continue;
            };
            handle_sse_event(
                &connector,
                &session_id,
                &protocol_version,
                &roots,
                &notifications,
                &mut locked_tx,
                value,
            )
            .await;
        }
    }
    debug!(session_id = %session_id, "MCP SSE stream closed");
    Ok(())
}

/// Dispatch one decoded SSE event: answer `roots/list`, signal the sandbox
/// lock, and forward the event to the notification channel.
async fn handle_sse_event(
    connector: &Connector,
    session_id: &str,
    protocol_version: &str,
    roots: &[PathBuf],
    notifications: &Option<mpsc::Sender<Value>>,
    locked_tx: &mut Option<tokio::sync::oneshot::Sender<()>>,
    value: Value,
) {
    let method = value.get("method").and_then(Value::as_str);

    if method == Some(SANDBOX_CONFIGURED_METHOD)
        && let Some(tx) = locked_tx.take()
    {
        let _ = tx.send(());
    }

    if method == Some(ROOTS_LIST_METHOD) {
        match value.get("id").cloned() {
            Some(request_id) => {
                respond_to_roots_list(connector, session_id, protocol_version, request_id, roots)
                    .await;
            }
            None => debug!("roots/list request without id; cannot answer"),
        }
    }

    if let Some(tx) = notifications {
        // Ignore channel-closed errors: the consumer may have moved on (e.g.
        // the TUI reset its session) while the stream stays open server-side.
        let _ = tx.send(value).await;
    }
}

/// Answer a server-initiated `roots/list` request by POSTing back a result
/// listing the caller-supplied roots (same JSON-RPC id, per the handshake
/// hard invariant).
async fn respond_to_roots_list(
    connector: &Connector,
    session_id: &str,
    protocol_version: &str,
    request_id: Value,
    roots: &[PathBuf],
) {
    let roots_json: Vec<Value> = roots
        .iter()
        .map(|path| {
            json!({
                "uri": encode_file_uri(path),
                "name": path.file_name().and_then(|n| n.to_str()).unwrap_or("workspace"),
            })
        })
        .collect();
    let roots_response = json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": { "roots": roots_json }
    });
    debug!("answering roots/list: {roots_response:?}");
    let _ = connector
        .post_client
        .post(&connector.mcp_url)
        .header("Content-Type", "application/json")
        .header(SESSION_ID_HEADER, session_id)
        .header(MCP_PROTOCOL_VERSION_HEADER, protocol_version)
        .json(&roots_response)
        .send()
        .await;
}
