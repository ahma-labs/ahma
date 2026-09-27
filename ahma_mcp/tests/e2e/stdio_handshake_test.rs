//! E2E regression tests for `ahma serve stdio`.
//!
//! These tests run the real binary with `NEXTEST` and `CARGO_MANIFEST_DIR` removed
//! so the production (proxy + background bridge) code path is exercised, not the
//! test-shortcut direct stdio path.  They verify that a standard MCP client can
//! complete an `initialize` → `tools/list` flow and receive tool definitions.
//!
//! Two scenarios:
//! * **roots_client** – client responds to `roots/list` with an empty list (most IDEs).
//! * **no_roots_client** – client never responds to `roots/list` (e.g. Antigravity).
//!   The sandbox must auto-lock from the `--sandbox-scope` fallback.
//!
//! IMPORTANT: do NOT add `env("CARGO_MANIFEST_DIR")` or `env("NEXTEST")` to these
//! processes – the absence of those env vars is what makes the production path run.

use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use ahma_mcp::test_utils::cli::build_binary_cached;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn build_binary() -> PathBuf {
    build_binary_cached("ahma_bin", "ahma")
}

/// Drive an `ahma serve stdio` process through the full MCP handshake and
/// assert that `tools/list` returns at least one tool.
///
/// `respond_to_roots` controls whether the test client answers the server's
/// `roots/list` request.  When `false` the server must fall back to the
/// `--sandbox-scope` it was given and still lock the sandbox in time.
async fn run_stdio_tools_list_scenario(respond_to_roots: bool) {
    // Create the UDS path in the OS temp dir, not the workspace tree: this test
    // passes `--no-sandbox` to the spawned process, so there is no sandbox-scope
    // reason to keep the socket inside the workspace, and a workspace-rooted path
    // (e.g. under a deeply nested git worktree at `.claude/worktrees/agent-<hex>/`)
    // can exceed the OS's `sockaddr_un.sun_path` capacity (~103 bytes on macOS,
    // ~107 on Linux). `std::env::temp_dir()` stays short regardless of workspace
    // nesting depth. See `ahma_common::test_isolation` for the same pattern.
    let rand_id = rand::random::<u32>();
    let socket_path = std::env::temp_dir().join(format!("ahma_test_handshake_{}.sock", rand_id));
    let _ = std::fs::remove_file(&socket_path);
    run_stdio_tools_list_against(respond_to_roots, &socket_path).await;
    let _ = std::fs::remove_file(&socket_path);
}

/// The scenario body, against a chosen shared-endpoint socket path.
async fn run_stdio_tools_list_against(respond_to_roots: bool, socket_path: &std::path::Path) {
    let (mut client, resp) = StdioClient::connect(respond_to_roots, socket_path).await;
    client.shutdown().await;

    let resp = resp.unwrap_or_else(|| {
        panic!(
            "Did not receive tools/list response within timeout (respond_to_roots={})",
            respond_to_roots
        )
    });

    assert!(
        resp.get("error").is_none(),
        "tools/list returned an error (respond_to_roots={}): {:?}",
        respond_to_roots,
        resp
    );

    let tools = resp
        .pointer("/result/tools")
        .and_then(|t| t.as_array())
        .expect("tools/list result should have a 'tools' array");

    assert!(
        !tools.is_empty(),
        "tools/list should return at least one tool (respond_to_roots={})",
        respond_to_roots
    );
}

/// A raw line-oriented MCP client driving one `ahma serve stdio` process.
struct StdioClient {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
    /// The next request id this client has not used yet.
    next_id: u64,
}

impl StdioClient {
    /// Spawn `ahma serve stdio`, complete the MCP handshake and poll
    /// `tools/list` until the sandbox locks. Returns the client and the last
    /// `tools/list` response seen (`None` if none arrived in time).
    async fn connect(
        respond_to_roots: bool,
        socket_path: &std::path::Path,
    ) -> (Self, Option<serde_json::Value>) {
        let binary = build_binary();
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let socket_str = socket_path.to_string_lossy().into_owned();

        // Use a tmp dir as the sandbox scope so the bridge can lock without real
        // roots. Kept for the process's lifetime by leaking it into the OS temp
        // dir's own cleanup: the scope must outlive every call the test makes.
        let scope = tempfile::TempDir::new().expect("tempdir").keep();
        let scope = scope.to_string_lossy().into_owned();

        let mut cmd = tokio::process::Command::new(&binary);
        cmd.current_dir(&workspace)
            .env("RUST_LOG", "warn")
            // Deliberately NOT setting AHMA_SERVER_CHILD, so the production
            // proxy + background hub path runs (this is an E2E test) — on
            // every OS, since the hub's endpoint is a local socket on
            // Windows too (SPEC R-HUB.2).
            .args([
                "--no-sandbox",
                "--unix-socket-path",
                &socket_str,
                "--sandbox-scope",
                &scope,
                "serve",
                "stdio",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        let mut child = cmd.spawn().expect("spawn ahma serve stdio");
        let stdin = child.stdin.take().unwrap();
        let reader = BufReader::new(child.stdout.take().unwrap());
        let mut client = Self {
            child,
            stdin,
            reader,
            next_id: 2,
        };

        // 1. initialize
        client
            .send(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "roots": { "listChanged": true } },
                    "clientInfo": { "name": "test-client", "version": "0.0.1" }
                }
            }))
            .await;

        let timeout = TestTimeouts::get(TimeoutCategory::Handshake);

        // 2. Wait for initialize response (has "id":1 and "result")
        let init_resp = client
            .read_until(timeout, |v| {
                v.get("id").and_then(|i| i.as_u64()) == Some(1) && v.get("result").is_some()
            })
            .await;
        assert!(
            init_resp.is_some(),
            "Did not receive initialize response within {}s (respond_to_roots={})",
            timeout.as_secs(),
            respond_to_roots
        );

        // 3. notifications/initialized
        client
            .send(serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }))
            .await;

        // 4. Optionally respond to roots/list, or just skip it.
        //    The server may send a roots/list request via the SSE channel, but through the
        //    proxy path the request arrives on stdout.  We give it a short window; if we
        //    see it and respond_to_roots=true we answer, otherwise we skip.
        if respond_to_roots {
            // Try to receive roots/list request (it may come within a few seconds).
            let roots_req = client
                .read_until(TestTimeouts::scale_secs(5), |v| {
                    v.get("method").and_then(|m| m.as_str()) == Some("roots/list")
                })
                .await;
            if let Some(req) = roots_req {
                let req_id = req.get("id").cloned().unwrap_or(serde_json::json!(99));
                client
                    .send(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "result": { "roots": [] }
                    }))
                    .await;
            }
        }

        // 5+6. tools/list, retried until the sandbox locks (bridge auto-lock +
        //      subprocess confirmation). The stdio path emits no client-visible
        //      "sandbox locked" notification, so poll the observable instead:
        //      keep issuing tools/list (fresh id per attempt) until a successful
        //      response with a non-empty tools array arrives, under a
        //      SandboxReady deadline.
        let sandbox_deadline = Instant::now() + TestTimeouts::get(TimeoutCategory::SandboxReady);
        let first_id = client.next_id;
        let mut tools_resp: Option<serde_json::Value> = None;
        loop {
            let newest_id = client.next_id;
            client.next_id += 1;
            client
                .send(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": newest_id,
                    "method": "tools/list",
                    "params": {}
                }))
                .await;

            // Per-attempt read window, capped by the overall deadline. Every
            // outstanding request is a tools/list, so accept a response to ANY
            // issued id — matching only the newest id would discard a slow
            // server's reply to the previous attempt and starve the loop under
            // load.
            let attempt_timeout = TestTimeouts::get(TimeoutCategory::ToolCall)
                .min(sandbox_deadline.saturating_duration_since(Instant::now()));
            let resp = client
                .read_until(attempt_timeout, |v| {
                    v.get("method").is_none() // a response, not a server-initiated request
                        && v.get("id")
                            .and_then(|i| i.as_u64())
                            .is_some_and(|i| (first_id..=newest_id).contains(&i))
                })
                .await;

            if let Some(resp) = resp {
                let ready = resp.get("error").is_none()
                    && resp
                        .pointer("/result/tools")
                        .and_then(|t| t.as_array())
                        .is_some_and(|t| !t.is_empty());
                tools_resp = Some(resp);
                if ready {
                    break;
                }
            }
            if Instant::now() >= sandbox_deadline {
                break;
            }
            tokio::time::sleep(TestTimeouts::poll_interval()).await;
        }
        (client, tools_resp)
    }

    async fn send(&mut self, msg: serde_json::Value) {
        let mut line = serde_json::to_string(&msg).unwrap();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .expect("write stdin");
    }

    /// Read lines until a JSON object matching `pred` is found or we time out.
    async fn read_until<F>(&mut self, timeout: Duration, pred: F) -> Option<serde_json::Value>
    where
        F: Fn(&serde_json::Value) -> bool,
    {
        let deadline = Instant::now() + timeout;
        let mut buf = String::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            buf.clear();
            match tokio::time::timeout(remaining, self.reader.read_line(&mut buf)).await {
                Ok(Ok(0)) | Err(_) => return None,
                Ok(Ok(_)) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(buf.trim())
                        && pred(&v)
                    {
                        return Some(v);
                    }
                }
                Ok(Err(_)) => return None,
            }
        }
    }

    async fn shutdown(&mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

#[tokio::test]
async fn test_serve_stdio_tools_list_with_roots_client() {
    run_stdio_tools_list_scenario(true).await;
}

#[tokio::test]
async fn test_serve_stdio_tools_list_no_roots_client() {
    run_stdio_tools_list_scenario(false).await;
}

/// SPEC R-LIFECYCLE.3 (item 3): when the shared backend cannot be started at all —
/// here its socket path lies under a regular file, the way a host sandbox that
/// forbids the detached spawn looks from inside — the frontend serves the
/// session in-process instead of failing.
#[tokio::test]
async fn test_serve_stdio_falls_back_in_process_when_backend_cannot_start() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let not_a_dir = tmp.path().join("f");
    std::fs::write(&not_a_dir, b"").unwrap();
    run_stdio_tools_list_against(true, &not_a_dir.join("mcp.sock")).await;
}

/// REGRESSION: one long `tools/call` must not hold up the session's other
/// requests. The bridge used to send even the response headers of an SSE POST
/// only once the call returned, and the proxy's HTTP client handles one POST
/// at a time — so everything behind a long call (a concurrent request, the
/// bridge's liveness ping) waited for it. Pings went unanswered, the bridge
/// killed the session as unresponsive, and the IDE's server was dead.
///
/// Asserted as an ordering, not a latency: a command that outlasts the
/// request budget holds its `tools/call` open for that whole budget, and a
/// `tools/list` sent after it must still be answered first.
#[tokio::test]
async fn test_serve_stdio_answers_other_requests_during_a_long_call() {
    let rand_id = rand::random::<u32>();
    let socket_path = std::env::temp_dir().join(format!("ahma_test_concurrent_{}.sock", rand_id));
    let _ = std::fs::remove_file(&socket_path);
    let (mut client, tools) = StdioClient::connect(true, &socket_path).await;
    assert!(
        tools.is_some_and(|t| t.get("error").is_none()),
        "handshake did not reach a working tools/list"
    );

    let long_command = if cfg!(windows) {
        "Start-Sleep -Seconds 30"
    } else {
        "sleep 30"
    };
    let long_id = client.next_id;
    let quick_id = long_id + 1;
    client
        .send(serde_json::json!({
            "jsonrpc": "2.0",
            "id": long_id,
            "method": "tools/call",
            "params": {
                "name": "run_terminal_command",
                "arguments": { "command": long_command }
            }
        }))
        .await;
    client
        .send(serde_json::json!({
            "jsonrpc": "2.0",
            "id": quick_id,
            "method": "tools/list",
            "params": {}
        }))
        .await;

    let first = client
        .read_until(TestTimeouts::get(TimeoutCategory::ToolCall), |v| {
            v.get("method").is_none()
                && v.get("id")
                    .and_then(|i| i.as_u64())
                    .is_some_and(|i| i == long_id || i == quick_id)
        })
        .await;
    client.shutdown().await;
    let _ = std::fs::remove_file(&socket_path);

    let first = first.expect("neither request was answered");
    assert_eq!(
        first.get("id").and_then(|i| i.as_u64()),
        Some(quick_id),
        "tools/list was held up behind the long tool call: {first}"
    );
}
