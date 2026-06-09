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
    let binary = build_binary();
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();

    let rand_id = rand::random::<u32>();
    let socket_path = workspace
        .join("target")
        .join(format!("ahma_test_handshake_{}.sock", rand_id));
    let _ = std::fs::remove_file(&socket_path);
    let socket_str = socket_path.to_string_lossy().into_owned();

    // Find a free TCP port to avoid conflicts.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind free port")
        .local_addr()
        .expect("local_addr")
        .port();

    // Use a tmp dir as the sandbox scope so the bridge can lock without real roots.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let scope = tmp.path().to_string_lossy().into_owned();

    let mut child = tokio::process::Command::new(&binary)
        .current_dir(&workspace)
        .env("RUST_LOG", "warn")
        .env("AHMA_UNIX_SOCKET", &socket_str)
        .env("AHMA_HTTP_PORT", port.to_string())
        // Deliberately DO NOT set CARGO_MANIFEST_DIR or NEXTEST so the
        // production proxy + background bridge path runs.
        .env_remove("NEXTEST")
        .env_remove("CARGO_MANIFEST_DIR")
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
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn ahma serve stdio");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    // Helper: send a JSON-RPC message to the process.
    macro_rules! send {
        ($msg:expr) => {{
            let mut line = serde_json::to_string(&$msg).unwrap();
            line.push('\n');
            stdin.write_all(line.as_bytes()).await.expect("write stdin");
        }};
    }

    // Helper: read lines until a JSON object matching `pred` is found or we time out.
    async fn read_until<F>(
        reader: &mut BufReader<tokio::process::ChildStdout>,
        timeout: Duration,
        pred: F,
    ) -> Option<serde_json::Value>
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
            match tokio::time::timeout(remaining, reader.read_line(&mut buf)).await {
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

    // 1. initialize
    send!(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "roots": { "listChanged": true } },
            "clientInfo": { "name": "test-client", "version": "0.0.1" }
        }
    }));

    let timeout = Duration::from_secs(20);

    // 2. Wait for initialize response (has "id":1 and "result")
    let init_resp = read_until(&mut reader, timeout, |v| {
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
    send!(serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    }));

    // 4. Optionally respond to roots/list, or just skip it.
    //    The server may send a roots/list request via the SSE channel, but through the
    //    proxy path the request arrives on stdout.  We give it a short window; if we
    //    see it and respond_to_roots=true we answer, otherwise we skip.
    if respond_to_roots {
        // Try to receive roots/list request (it may come within a few seconds).
        let roots_req = read_until(&mut reader, Duration::from_secs(5), |v| {
            v.get("method").and_then(|m| m.as_str()) == Some("roots/list")
        })
        .await;
        if let Some(req) = roots_req {
            let req_id = req.get("id").cloned().unwrap_or(serde_json::json!(99));
            send!(serde_json::json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": { "roots": [] }
            }));
        }
    }

    // 5. Give the sandbox time to lock (bridge auto-lock + subprocess confirmation)
    tokio::time::sleep(Duration::from_secs(3)).await;

    // 6. tools/list
    send!(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    }));

    let tools_resp = read_until(&mut reader, Duration::from_secs(15), |v| {
        v.get("id").and_then(|i| i.as_u64()) == Some(2)
    })
    .await;

    let _ = child.kill().await;
    let _ = child.wait().await;
    let _ = std::fs::remove_file(&socket_path);

    let resp = tools_resp.unwrap_or_else(|| {
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

#[tokio::test]
async fn test_serve_stdio_tools_list_with_roots_client() {
    run_stdio_tools_list_scenario(true).await;
}

#[tokio::test]
async fn test_serve_stdio_tools_list_no_roots_client() {
    run_stdio_tools_list_scenario(false).await;
}
