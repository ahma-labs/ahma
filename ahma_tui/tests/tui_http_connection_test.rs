//! Integration tests — TUI working with `ahma serve http` and `ahma serve stdio`.
//!
//! `ahma serve http` exposes the bridge directly on a TCP port.
//! `ahma serve stdio` starts the same HTTP bridge, which internally spawns a
//! fresh stdio-mode `ahma_mcp` subprocess per client session.  From the TUI's
//! perspective both modes look identical: an HTTP server at a known base URL.
//!
//! These tests verify:
//! * The TUI health probe succeeds against a running bridge.
//! * `resolve_connection` resolves an explicit HTTP URL correctly.
//! * `resolve_connection` returns a helpful error for unreachable URLs.
//! * `spawn_mcp_source` emits `HealthChanged` and `ToolsListUpdated` events
//!   when talking to an MCP-compatible server.

mod common;

use std::time::Duration;

use ahma_common::timeouts::TestTimeouts;
use ahma_tui::connection::{ResolvedConnection, ResolvedTransport, probe_candidate};
use ahma_tui::mcp_source::{SourceEvent, spawn_mcp_source};
use axum::Json;
use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};
use tokio::sync::mpsc;

// ─── Minimal mock MCP server ─────────────────────────────────────────────────

async fn mock_health() -> StatusCode {
    StatusCode::OK
}

/// Minimal GET /mcp SSE endpoint. The TUI now opens this stream as part of the
/// handshake and waits for a 2xx before sending `notifications/initialized`, so
/// the mock must answer it. An empty event-stream body is sufficient for the
/// readiness signal.
async fn mock_mcp_sse() -> Response {
    (StatusCode::OK, [("content-type", "text/event-stream")], "").into_response()
}

async fn mock_mcp(Json(body): Json<Value>) -> Response {
    let method = body
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    let id = body.get("id").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            let mut headers = HeaderMap::new();
            headers.insert(
                "mcp-session-id",
                "test-session-abc".parse().expect("valid header value"),
            );
            (
                StatusCode::OK,
                headers,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "serverInfo": {"name": "mock-mcp", "version": "0.0.1"}
                    }
                })),
            )
                .into_response()
        }
        "tools/list" => (
            StatusCode::OK,
            Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [{
                        "name": "mock_tool",
                        "description": "A mock tool used by TUI integration tests",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }
            })),
        )
            .into_response(),
        _ => (
            StatusCode::OK,
            Json(json!({"jsonrpc": "2.0", "id": id, "result": null})),
        )
            .into_response(),
    }
}

/// Spin up a minimal in-process MCP-compatible HTTP server.
///
/// Returns `(base_url, JoinHandle)`. Dropping the handle aborts the server.
async fn start_mock_mcp_server() -> (String, tokio::task::JoinHandle<()>) {
    let router = Router::new()
        .route("/health", get(mock_health))
        .route("/mcp", post(mock_mcp).get(mock_mcp_sse));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock MCP server");
    let port = listener.local_addr().expect("local_addr").port();
    let base_url = format!("http://127.0.0.1:{port}");

    let handle = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("mock MCP server error");
    });

    tokio::time::sleep(TestTimeouts::short_delay()).await;
    (base_url, handle)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// `probe_candidate` returns `true` when an HTTP bridge is reachable.
///
/// Represents: TUI successfully detects an `ahma serve http` server.
#[tokio::test]
async fn http_health_probe_succeeds() {
    let bridge = common::start_bridge_tcp(false).await;
    let candidate = ResolvedConnection {
        display_url: bridge.base_url.clone(),
        transport: ResolvedTransport::Http(bridge.base_url.clone()),
    };
    assert!(
        probe_candidate(&candidate).await,
        "probe_candidate should return true when the bridge is healthy"
    );
}

/// `resolve_connection(Some(url))` resolves an explicit HTTP URL successfully.
#[tokio::test]
async fn resolve_explicit_url_returns_http_transport() {
    let bridge = common::start_bridge_tcp(false).await;
    let result = ahma_tui::connection::resolve_connection(Some(&bridge.base_url))
        .await
        .expect("resolve_connection should succeed for a healthy bridge");
    assert!(
        matches!(
            result.transport,
            ResolvedTransport::Http(_) | ResolvedTransport::Http3(_)
        ),
        "expected Http or Http3 transport, got: {:?}",
        result.transport
    );
}

/// `resolve_connection` returns a helpful error when the URL is unreachable.
#[tokio::test]
async fn resolve_explicit_url_unreachable_returns_helpful_error() {
    // Port 1 is reserved and not bindable in user space; always unreachable.
    let result = ahma_tui::connection::resolve_connection(Some("http://127.0.0.1:1")).await;
    assert!(result.is_err(), "expected Err for unreachable server");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("unreachable") || msg.contains("ahma serve"),
        "error message should guide the user, got: {msg}"
    );
}

/// `spawn_mcp_source` emits `HealthChanged { healthy: true }` once the server
/// responds to GET /health.
///
/// Represents: TUI status bar transitions from "disconnected" to "connected".
#[tokio::test]
async fn mcp_source_emits_health_changed() {
    let (base_url, _server) = start_mock_mcp_server().await;
    let (tx, mut rx) = mpsc::channel(32);
    let connection = ResolvedConnection {
        display_url: base_url.clone(),
        transport: ResolvedTransport::Http(base_url),
    };
    let _cmd_tx = spawn_mcp_source(connection, tx, None);
    let healthy = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(ev) = rx.recv().await {
            if let SourceEvent::HealthChanged { healthy } = ev {
                return healthy;
            }
        }
        false
    })
    .await
    .expect("timed out waiting for HealthChanged event");
    assert!(healthy, "mcp_source should report the server as healthy");
}

// ─── Handshake ordering regression test ──────────────────────────────────────

#[derive(Clone)]
struct HandshakeState {
    /// Set true once the GET /mcp SSE handler has accepted the stream.
    sse_connected: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Records the value of `sse_connected` at the instant the server first
    /// received `notifications/initialized`. `None` until that POST arrives.
    sse_ready_at_initialized: std::sync::Arc<parking_lot::Mutex<Option<bool>>>,
}

async fn handshake_sse(
    axum::extract::State(state): axum::extract::State<HandshakeState>,
) -> Response {
    // Simulate connection-setup latency. This widens the window in which the
    // old fire-and-forget client would (incorrectly) send notifications/initialized
    // before the SSE stream was live.
    tokio::time::sleep(Duration::from_millis(150)).await;
    state
        .sse_connected
        .store(true, std::sync::atomic::Ordering::SeqCst);
    (StatusCode::OK, [("content-type", "text/event-stream")], "").into_response()
}

async fn handshake_mcp(
    axum::extract::State(state): axum::extract::State<HandshakeState>,
    Json(body): Json<Value>,
) -> Response {
    let method = body
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    let id = body.get("id").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            let mut headers = HeaderMap::new();
            headers.insert(
                "mcp-session-id",
                "handshake-session".parse().expect("valid header value"),
            );
            (
                StatusCode::OK,
                headers,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "serverInfo": {"name": "handshake-mock", "version": "0.0.1"}
                    }
                })),
            )
                .into_response()
        }
        "notifications/initialized" => {
            let connected = state
                .sse_connected
                .load(std::sync::atomic::Ordering::SeqCst);
            let mut guard = state.sse_ready_at_initialized.lock();
            if guard.is_none() {
                *guard = Some(connected);
            }
            (StatusCode::ACCEPTED, Json(json!({}))).into_response()
        }
        _ => (
            StatusCode::OK,
            Json(json!({"jsonrpc": "2.0", "id": id, "result": null})),
        )
            .into_response(),
    }
}

fn new_handshake_state() -> HandshakeState {
    HandshakeState {
        sse_connected: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        sse_ready_at_initialized: std::sync::Arc::new(parking_lot::Mutex::new(None)),
    }
}

fn handshake_router(state: HandshakeState) -> Router {
    Router::new()
        .route("/health", get(mock_health))
        .route("/mcp", post(handshake_mcp).get(handshake_sse))
        .with_state(state)
}

/// Drive the full TUI handshake against `connection` and assert that the SSE
/// return stream was connected at the instant the server received
/// `notifications/initialized`. This is the transport-agnostic core of the
/// handshake-race regression: `init_mcp_session`/`run_sse_listener` are shared
/// by every transport, so each variant must satisfy the same ordering.
async fn assert_sse_open_before_initialized(connection: ResolvedConnection, state: HandshakeState) {
    let (tx, _rx) = mpsc::channel(32);
    let _cmd_tx = spawn_mcp_source(connection, tx, None);

    let recorded = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(v) = *state.sse_ready_at_initialized.lock() {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("timed out waiting for notifications/initialized");

    assert!(
        recorded,
        "SSE return stream must be connected before notifications/initialized is sent"
    );
}

/// Regression test for the TUI MCP handshake race over plain HTTP.
///
/// The TUI must open the GET /mcp SSE return stream BEFORE sending
/// `notifications/initialized`; otherwise the bridge has nowhere to deliver its
/// roots/list request and the session hangs with no response (the original
/// "ran ls, got nothing" bug).
#[tokio::test]
async fn handshake_opens_sse_before_initialized_http() {
    let state = new_handshake_state();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind handshake mock server");
    let port = listener.local_addr().expect("local_addr").port();
    let base_url = format!("http://127.0.0.1:{port}");
    let router = handshake_router(state.clone());
    let _server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("handshake mock server error");
    });
    tokio::time::sleep(TestTimeouts::short_delay()).await;

    let connection = ResolvedConnection {
        display_url: base_url.clone(),
        transport: ResolvedTransport::Http(base_url),
    };
    assert_sse_open_before_initialized(connection, state).await;
}

/// Same handshake-ordering invariant over the `ResolvedTransport::Http3` path.
///
/// In `mcp_source`, `extract_http_base_url` treats `Http3(url)` identically to
/// `Http(url)` and builds the same reqwest client, so the MCP traffic flows over
/// the same code regardless of whether the connection was QUIC-upgraded. This
/// test therefore points an `Http3` transport at a plain-HTTP mock (no real QUIC
/// stack is needed) to prove the `Http3` variant still opens SSE first.
#[tokio::test]
async fn handshake_opens_sse_before_initialized_http3() {
    let state = new_handshake_state();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind handshake mock server");
    let port = listener.local_addr().expect("local_addr").port();
    let base_url = format!("http://127.0.0.1:{port}");
    let router = handshake_router(state.clone());
    let _server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("handshake mock server error");
    });
    tokio::time::sleep(TestTimeouts::short_delay()).await;

    let connection = ResolvedConnection {
        display_url: base_url.clone(),
        transport: ResolvedTransport::Http3(base_url),
    };
    assert_sse_open_before_initialized(connection, state).await;
}

/// Same handshake-ordering invariant over the Unix-socket transport.
///
/// `ahma serve unix` is the default local transport, so the race must be proven
/// gone here too. The mock MCP server is served over a Unix domain socket and
/// the TUI connects via `ResolvedTransport::UnixSocket`.
#[cfg(unix)]
#[tokio::test]
async fn handshake_opens_sse_before_initialized_unix() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let socket_path = tmp.path().join("ahma_tui_handshake_test.sock");

    let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind unix socket");
    let state = new_handshake_state();
    let router = handshake_router(state.clone());
    let _server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("handshake mock server error");
    });
    tokio::time::sleep(TestTimeouts::short_delay()).await;

    let socket_str = socket_path.to_string_lossy().into_owned();
    let connection = ResolvedConnection {
        display_url: format!("unix://{socket_str}"),
        transport: ResolvedTransport::UnixSocket(socket_str),
    };
    assert_sse_open_before_initialized(connection, state).await;
}

/// `spawn_mcp_source` emits `ToolsListUpdated` with the tool names returned by
/// the server's `tools/list` response.
///
/// Represents: TUI command palette populates with tools exposed by the MCP server.
#[tokio::test]
async fn mcp_source_emits_tools_list() {
    let (base_url, _server) = start_mock_mcp_server().await;
    let (tx, mut rx) = mpsc::channel(32);
    let connection = ResolvedConnection {
        display_url: base_url.clone(),
        transport: ResolvedTransport::Http(base_url),
    };
    let _cmd_tx = spawn_mcp_source(connection, tx, None);
    let tools = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(ev) = rx.recv().await {
            if let SourceEvent::ToolsListUpdated { tools } = ev {
                return tools;
            }
        }
        vec![]
    })
    .await
    .expect("timed out waiting for ToolsListUpdated event");
    assert!(
        tools.iter().any(|t| t.name == "mock_tool"),
        "expected 'mock_tool' in tools list, got: {tools:?}"
    );
}

// ─── Transient 409 during sandbox handshake regression test ──────────────────

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Mock MCP server whose `status` tool call returns `409 Conflict` for the
/// first poll (sandbox still locking on the bridge) and `200 OK` thereafter.
///
/// This reproduces the real bridge behaviour where a `status` poll can race
/// ahead of the subprocess finishing its sandbox lock. The TUI must treat that
/// 409 as transient and keep the session, not tear it down.
async fn flaky_status_mcp(
    axum::extract::State(status_calls): axum::extract::State<Arc<AtomicUsize>>,
    Json(body): Json<Value>,
) -> Response {
    let method = body
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    let id = body.get("id").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            let mut headers = HeaderMap::new();
            headers.insert(
                "mcp-session-id",
                "flaky-status-session".parse().expect("valid header value"),
            );
            (
                StatusCode::OK,
                headers,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "serverInfo": {"name": "flaky-status-mock", "version": "0.0.1"}
                    }
                })),
            )
                .into_response()
        }
        "tools/call" => {
            let tool = body
                .pointer("/params/name")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if tool == "status" {
                // First status poll races the sandbox lock and 409s; the bridge
                // finishes locking immediately after, so every later poll is OK.
                let n = status_calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    return (
                        StatusCode::CONFLICT,
                        "sandbox still initializing — complete MCP handshake (roots/list) before tools/call",
                    )
                        .into_response();
                }
            }
            (
                StatusCode::OK,
                Json(json!({"jsonrpc": "2.0", "id": id, "result": {"content": []}})),
            )
                .into_response()
        }
        _ => (
            StatusCode::OK,
            Json(json!({"jsonrpc": "2.0", "id": id, "result": null})),
        )
            .into_response(),
    }
}

/// Regression test: a transient `409` from the `status` poll must NOT tear down
/// the MCP session.
///
/// Before the fix, a 409 (sandbox still locking) was treated as fatal: the TUI
/// dropped the session, then immediately re-initialized and polled again,
/// racing the same handshake and 409-ing forever. The session id flapped to
/// empty on every reset and no tool call (e.g. `pwd`) ever ran — the
/// "tool calls don't work in ahma tui" bug.
///
/// The fixed behaviour: the session is kept across the 409 and the next poll
/// succeeds. We assert that `OperationsUpdated` is eventually emitted and that
/// no session-reset (`SessionId { id: "" }`) was observed in the meantime.
#[tokio::test]
async fn transient_409_status_poll_does_not_reset_session() {
    let status_calls = Arc::new(AtomicUsize::new(0));
    let router = Router::new()
        .route("/health", get(mock_health))
        .route("/mcp", post(flaky_status_mcp).get(mock_mcp_sse))
        .with_state(status_calls.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind flaky-status mock server");
    let port = listener.local_addr().expect("local_addr").port();
    let base_url = format!("http://127.0.0.1:{port}");
    let _server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("flaky-status mock server error");
    });
    tokio::time::sleep(TestTimeouts::short_delay()).await;

    let (tx, mut rx) = mpsc::channel(64);
    let connection = ResolvedConnection {
        display_url: base_url.clone(),
        transport: ResolvedTransport::Http(base_url),
    };
    let _cmd_tx = spawn_mcp_source(connection, tx, None);

    // The status tick is 3s, so the first poll (409) and the recovering second
    // poll (200) land within ~6s. Allow generous headroom.
    let got_operations = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(ev) = rx.recv().await {
            match ev {
                // A reset would clear the session id — this must never happen
                // for a merely-transient 409.
                SourceEvent::SessionId { id } if id.is_empty() => {
                    panic!(
                        "session was reset (SessionId cleared) on a transient 409 — \
                         the handshake-race reset loop has regressed"
                    );
                }
                SourceEvent::OperationsUpdated { .. } => return true,
                _ => {}
            }
        }
        false
    })
    .await
    .expect("timed out waiting for OperationsUpdated after a transient 409");

    assert!(
        got_operations,
        "TUI should recover from a transient 409 and emit operations"
    );
    assert!(
        status_calls.load(Ordering::SeqCst) >= 2,
        "expected the status poll to be retried on the same session after the 409"
    );
}
