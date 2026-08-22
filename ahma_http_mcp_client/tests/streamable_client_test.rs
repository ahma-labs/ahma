//! In-process tests for the shared MCP Streamable-HTTP client.
//!
//! Each test runs the real client against a minimal in-process axum mock
//! server (no subprocesses), pinning the handshake hard invariant: SSE stream
//! open BEFORE `notifications/initialized`, `roots/list` answered with the
//! same id, and the 409/`-32001` sandbox gate surfaced per retry policy.

use ahma_common::timeouts::TestTimeouts;
use ahma_http_mcp_client::streamable::{
    ConflictRetryPolicy, ConnectOptions, Connector, StreamableHttpMcpClient, ToolCallOutcome,
    parse_tools_list,
};
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// ─── Mock server plumbing ────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct MockState {
    /// True once the GET /mcp SSE handler has accepted the stream.
    sse_connected: Arc<AtomicBool>,
    /// Value of `sse_connected` when `notifications/initialized` first arrived.
    sse_ready_at_initialized: Arc<Mutex<Option<bool>>>,
    /// Recorded roots/list *response* posted back by the client.
    roots_answer: Arc<Mutex<Option<Value>>>,
    /// How many GET /mcp (SSE) requests arrived.
    sse_gets: Arc<AtomicUsize>,
    /// How many `tools/call` POSTs arrived.
    tool_calls: Arc<AtomicUsize>,
    /// How many leading `tools/call`s should answer 409.
    conflicts_before_success: Arc<AtomicUsize>,
    /// Recorded DELETE session ids.
    deleted_sessions: Arc<Mutex<Vec<String>>>,
}

async fn mock_post(State(st): State<MockState>, Json(body): Json<Value>) -> Response {
    let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = body.get("id").cloned().unwrap_or(Value::Null);

    // A client roots/list *response* carries result.roots and no method.
    if method.is_empty() && body.pointer("/result/roots").is_some() {
        *st.roots_answer.lock().expect("lock") = Some(body);
        return StatusCode::ACCEPTED.into_response();
    }

    match method {
        "initialize" => {
            let mut headers = HeaderMap::new();
            headers.insert("mcp-session-id", "mock-session".parse().expect("header"));
            (
                StatusCode::OK,
                headers,
                Json(json!({ "jsonrpc": "2.0", "id": id, "result": {} })),
            )
                .into_response()
        }
        "notifications/initialized" => {
            let connected = st.sse_connected.load(Ordering::SeqCst);
            let mut guard = st.sse_ready_at_initialized.lock().expect("lock");
            if guard.is_none() {
                *guard = Some(connected);
            }
            StatusCode::ACCEPTED.into_response()
        }
        "tools/call" => {
            st.tool_calls.fetch_add(1, Ordering::SeqCst);
            let remaining = st.conflicts_before_success.load(Ordering::SeqCst);
            if remaining > 0 {
                st.conflicts_before_success.fetch_sub(1, Ordering::SeqCst);
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32001, "message": "Sandbox initializing" }
                    })),
                )
                    .into_response();
            }
            Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "content": [{ "type": "text", "text": "tool-ok" }] }
            }))
            .into_response()
        }
        "tools/list" => Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "tools": [
                { "name": "alpha", "description": "d", "inputSchema": { "type": "object" } },
                { "name": "beta" },
                { "description": "nameless — must be skipped" }
            ] }
        }))
        .into_response(),
        _ => Json(json!({ "jsonrpc": "2.0", "id": id, "result": null })).into_response(),
    }
}

/// SSE endpoint: emits a roots/list request, then (after a beat) a
/// sandbox/configured notification and a custom notification, then holds the
/// stream open.
async fn mock_sse(State(st): State<MockState>) -> Response {
    st.sse_gets.fetch_add(1, Ordering::SeqCst);
    // Simulate connection-setup latency to widen the race window a
    // fire-and-forget client would lose.
    tokio::time::sleep(TestTimeouts::scale_millis(100)).await;
    st.sse_connected.store(true, Ordering::SeqCst);

    use futures::StreamExt;
    let roots_req = "data: {\"jsonrpc\":\"2.0\",\"id\":\"r1\",\"method\":\"roots/list\"}\n\n";
    let configured =
        "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/sandbox/configured\"}\n\n";
    let custom = "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/custom\",\"params\":{\"k\":\"v\"}}\n\n";
    let s1 = futures::stream::once(async move {
        Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(roots_req))
    });
    let s2 = futures::stream::once(async move {
        tokio::time::sleep(TestTimeouts::scale_millis(50)).await;
        Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(configured))
    });
    let s3 = futures::stream::once(async move {
        Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(custom))
    });
    let tail = futures::stream::pending::<Result<axum::body::Bytes, std::convert::Infallible>>();
    let stream = s1.chain(s2).chain(s3).chain(tail);
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        axum::body::Body::from_stream(stream),
    )
        .into_response()
}

async fn mock_delete(State(st): State<MockState>, headers: HeaderMap) -> StatusCode {
    if let Some(sid) = headers.get("mcp-session-id").and_then(|v| v.to_str().ok()) {
        st.deleted_sessions
            .lock()
            .expect("lock")
            .push(sid.to_string());
    }
    StatusCode::OK
}

async fn spawn_mock(state: MockState) -> (String, tokio::task::JoinHandle<()>) {
    let router = Router::new()
        .route("/mcp", post(mock_post).get(mock_sse).delete(mock_delete))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let port = listener.local_addr().expect("local_addr").port();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://127.0.0.1:{port}/mcp"), handle)
}

fn connector(mcp_url: &str) -> Connector {
    let post_client = reqwest::Client::builder()
        .timeout(TestTimeouts::scale_secs(10))
        .build()
        .expect("client build");
    // SSE client: no request timeout (long-lived stream).
    let sse_client = reqwest::Client::new();
    Connector {
        mcp_url: mcp_url.to_string(),
        post_client,
        sse_client,
    }
}

fn connect_opts(workspace: &std::path::Path) -> ConnectOptions {
    ConnectOptions {
        client_name: "streamable-test".to_string(),
        client_version: "0.0.1".to_string(),
        roots: vec![workspace.to_path_buf()],
        notifications: None,
        sse_open_timeout: TestTimeouts::scale_secs(5),
        sandbox_lock_timeout: Some(TestTimeouts::scale_secs(10)),
    }
}

// ─── Handshake ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn handshake_opens_sse_before_initialized_and_answers_roots() {
    let state = MockState::default();
    let (mcp_url, server) = spawn_mock(state.clone()).await;
    let tmp = tempfile::tempdir().expect("tempdir");

    let client = StreamableHttpMcpClient::connect(connector(&mcp_url), connect_opts(tmp.path()))
        .await
        .expect("handshake should succeed");
    assert_eq!(client.session_id(), "mock-session");

    // Hard invariant: the SSE return stream was connected at the instant the
    // server received notifications/initialized.
    let recorded = state
        .sse_ready_at_initialized
        .lock()
        .expect("lock")
        .expect("notifications/initialized must have been received");
    assert!(
        recorded,
        "SSE return stream must be connected before notifications/initialized is sent"
    );

    // roots/list answered with the same id and a file:// URI for the root.
    let answer = state
        .roots_answer
        .lock()
        .expect("lock")
        .clone()
        .expect("roots/list must be answered");
    assert_eq!(
        answer["id"],
        json!("r1"),
        "answer must reuse the request id"
    );
    let uri = answer["result"]["roots"][0]["uri"]
        .as_str()
        .expect("root uri");
    assert!(uri.starts_with("file://"), "uri={uri}");

    server.abort();
}

#[tokio::test]
async fn handshake_missing_session_id_is_diagnostic_error() {
    async fn no_header(Json(_b): Json<Value>) -> Response {
        (StatusCode::UNAUTHORIZED, "denied").into_response()
    }
    let router = Router::new().route("/mcp", post(no_header));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let mcp_url = format!("http://127.0.0.1:{port}/mcp");
    let tmp = tempfile::tempdir().expect("tempdir");

    let err = StreamableHttpMcpClient::connect(connector(&mcp_url), connect_opts(tmp.path()))
        .await
        .expect_err("missing session header must fail the handshake");
    let msg = format!("{err:#}");
    assert!(msg.contains("mcp-session-id"), "unexpected error: {msg}");
    assert!(
        msg.contains("401"),
        "error must carry the HTTP status: {msg}"
    );
    assert!(
        msg.contains("denied"),
        "error must carry a body snippet: {msg}"
    );

    server.abort();
}

#[tokio::test]
async fn handshake_sse_failure_aborts() {
    async fn init_only(Json(body): Json<Value>) -> Response {
        let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
        if method == "initialize" {
            let mut headers = HeaderMap::new();
            headers.insert("mcp-session-id", "sse-fail".parse().unwrap());
            (StatusCode::OK, headers, Json(json!({ "result": {} }))).into_response()
        } else {
            StatusCode::OK.into_response()
        }
    }
    let router = Router::new().route(
        "/mcp",
        post(init_only).get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let mcp_url = format!("http://127.0.0.1:{port}/mcp");
    let tmp = tempfile::tempdir().expect("tempdir");

    let err = StreamableHttpMcpClient::connect(connector(&mcp_url), connect_opts(tmp.path()))
        .await
        .expect_err("a failed SSE stream must abort the handshake");
    assert!(
        format!("{err:#}").contains("SSE stream failed to open"),
        "unexpected error: {err:#}"
    );

    server.abort();
}

#[tokio::test]
async fn connect_minimal_skips_sse_and_roots() {
    let state = MockState::default();
    let (mcp_url, server) = spawn_mock(state.clone()).await;

    let client =
        StreamableHttpMcpClient::connect_minimal(connector(&mcp_url), "minimal-test", "0.0.1")
            .await
            .expect("minimal handshake should succeed");
    assert_eq!(client.session_id(), "mock-session");
    assert_eq!(
        state.sse_gets.load(Ordering::SeqCst),
        0,
        "minimal connect must not open an SSE stream"
    );
    assert!(
        state.roots_answer.lock().expect("lock").is_none(),
        "minimal connect must not answer roots/list"
    );

    server.abort();
}

// ─── Notifications ───────────────────────────────────────────────────────────

#[tokio::test]
async fn notifications_are_forwarded_to_channel() {
    let state = MockState::default();
    let (mcp_url, server) = spawn_mock(state.clone()).await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Value>(16);

    let mut opts = connect_opts(tmp.path());
    opts.notifications = Some(tx);
    let _client = StreamableHttpMcpClient::connect(connector(&mcp_url), opts)
        .await
        .expect("handshake should succeed");

    // The stream carries roots/list, sandbox/configured, and a custom
    // notification — all must be forwarded.
    let mut methods = Vec::new();
    let deadline = TestTimeouts::scale_secs(10);
    let _ = tokio::time::timeout(deadline, async {
        while let Some(v) = rx.recv().await {
            if let Some(m) = v.get("method").and_then(Value::as_str) {
                methods.push(m.to_string());
            }
            if methods.iter().any(|m| m == "notifications/custom") {
                break;
            }
        }
    })
    .await;

    assert!(
        methods.iter().any(|m| m == "roots/list"),
        "roots/list must be forwarded too, got: {methods:?}"
    );
    assert!(
        methods
            .iter()
            .any(|m| m == "notifications/sandbox/configured"),
        "sandbox/configured must be forwarded, got: {methods:?}"
    );
    assert!(
        methods.iter().any(|m| m == "notifications/custom"),
        "custom notifications must be forwarded, got: {methods:?}"
    );

    server.abort();
}

// ─── tools/call and the 409 gate ─────────────────────────────────────────────

#[tokio::test]
async fn call_tool_no_retry_surfaces_sandbox_initializing() {
    let state = MockState::default();
    state.conflicts_before_success.store(100, Ordering::SeqCst);
    let (mcp_url, server) = spawn_mock(state.clone()).await;

    let client = StreamableHttpMcpClient::attach(reqwest::Client::new(), &mcp_url, "s1");
    let outcome = client
        .call_tool("status", json!({}), ConflictRetryPolicy::NONE)
        .await
        .expect("call_tool should not error on 409");
    match outcome {
        ToolCallOutcome::SandboxInitializing { body } => {
            assert!(
                body.contains("-32001"),
                "409 body must carry -32001: {body}"
            );
        }
        other => panic!("expected SandboxInitializing, got {other:?}"),
    }
    assert_eq!(
        state.tool_calls.load(Ordering::SeqCst),
        1,
        "policy NONE must not retry"
    );

    server.abort();
}

#[tokio::test]
async fn call_tool_retries_through_transient_409() {
    let state = MockState::default();
    state.conflicts_before_success.store(2, Ordering::SeqCst);
    let (mcp_url, server) = spawn_mock(state.clone()).await;

    let client = StreamableHttpMcpClient::attach(reqwest::Client::new(), &mcp_url, "s2");
    let policy = ConflictRetryPolicy {
        max_retries: 5,
        delay: TestTimeouts::scale_millis(20),
    };
    let outcome = client
        .call_tool("status", json!({}), policy)
        .await
        .expect("call_tool should succeed after retries");
    match outcome {
        ToolCallOutcome::Success(v) => {
            assert_eq!(
                v.pointer("/result/content/0/text").and_then(Value::as_str),
                Some("tool-ok")
            );
        }
        other => panic!("expected Success after retries, got {other:?}"),
    }
    assert_eq!(
        state.tool_calls.load(Ordering::SeqCst),
        3,
        "two 409s then one success"
    );

    server.abort();
}

#[tokio::test]
async fn call_tool_non_409_http_error_is_reported() {
    let router = Router::new().route(
        "/mcp",
        post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let mcp_url = format!("http://127.0.0.1:{port}/mcp");

    let client = StreamableHttpMcpClient::attach(reqwest::Client::new(), &mcp_url, "s3");
    let outcome = client
        .call_tool("x", json!({}), ConflictRetryPolicy::NONE)
        .await
        .expect("HTTP error is an outcome, not an Err");
    match outcome {
        ToolCallOutcome::HttpError { status, body } => {
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(body, "boom");
        }
        other => panic!("expected HttpError, got {other:?}"),
    }

    server.abort();
}

// ─── tools/list ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn tools_list_parses_and_skips_nameless() {
    let state = MockState::default();
    let (mcp_url, server) = spawn_mock(state.clone()).await;

    let client = StreamableHttpMcpClient::attach(reqwest::Client::new(), &mcp_url, "s4");
    let tools = client.tools_list().await.expect("tools/list ok");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].name, "alpha");
    assert_eq!(tools[0].description.as_deref(), Some("d"));
    assert_eq!(tools[1].name, "beta");
    assert!(tools[1].description.is_none());
    // Missing inputSchema falls back to the permissive object schema.
    assert_eq!(tools[1].input_schema["type"], "object");
    assert_eq!(tools[1].input_schema["additionalProperties"], true);

    server.abort();
}

#[test]
fn parse_tools_list_tolerates_odd_shapes() {
    assert!(parse_tools_list(&json!({})).is_empty());
    assert!(parse_tools_list(&json!({ "result": { "tools": "nope" } })).is_empty());
    assert!(parse_tools_list(&json!({ "result": null })).is_empty());
}

#[tokio::test]
async fn tools_list_non_2xx_errors() {
    let router = Router::new().route("/mcp", post(|| async { StatusCode::FORBIDDEN }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let mcp_url = format!("http://127.0.0.1:{port}/mcp");

    let client = StreamableHttpMcpClient::attach(reqwest::Client::new(), &mcp_url, "s5");
    let err = client
        .tools_list()
        .await
        .expect_err("non-2xx tools/list must error");
    assert!(format!("{err:#}").contains("403"), "unexpected: {err:#}");

    server.abort();
}

// ─── Session teardown ────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_session_sends_delete_with_session_header() {
    let state = MockState::default();
    let (mcp_url, server) = spawn_mock(state.clone()).await;

    let client = StreamableHttpMcpClient::attach(reqwest::Client::new(), &mcp_url, "to-delete");
    client.delete_session(TestTimeouts::scale_secs(2)).await;

    assert_eq!(
        state.deleted_sessions.lock().expect("lock").as_slice(),
        &["to-delete".to_string()]
    );

    server.abort();
}
