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
    sse_ready_at_initialized: std::sync::Arc<std::sync::Mutex<Option<bool>>>,
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
            let mut guard = state.sse_ready_at_initialized.lock().expect("lock");
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

/// Regression test for the TUI MCP handshake race.
///
/// The TUI must open the GET /mcp SSE return stream BEFORE sending
/// `notifications/initialized`; otherwise the bridge has nowhere to deliver its
/// roots/list request and the session hangs with no response (the original
/// "ran ls, got nothing" bug). This test asserts that, at the moment the server
/// receives `notifications/initialized`, the SSE stream was already connected.
#[tokio::test]
async fn handshake_opens_sse_before_initialized() {
    let state = HandshakeState {
        sse_connected: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        sse_ready_at_initialized: std::sync::Arc::new(std::sync::Mutex::new(None)),
    };

    let router = Router::new()
        .route("/health", get(mock_health))
        .route("/mcp", post(handshake_mcp).get(handshake_sse))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind handshake mock server");
    let port = listener.local_addr().expect("local_addr").port();
    let base_url = format!("http://127.0.0.1:{port}");
    let _server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("handshake mock server error");
    });
    tokio::time::sleep(TestTimeouts::short_delay()).await;

    let (tx, _rx) = mpsc::channel(32);
    let connection = ResolvedConnection {
        display_url: base_url.clone(),
        transport: ResolvedTransport::Http(base_url),
    };
    let _cmd_tx = spawn_mcp_source(connection, tx, None);

    // Wait until the server records the SSE state seen when it first received
    // notifications/initialized.
    let recorded = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(v) = *state.sse_ready_at_initialized.lock().expect("lock") {
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
