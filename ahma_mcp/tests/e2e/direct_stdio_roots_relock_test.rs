//! Direct-stdio sandbox commit is one-shot (SPEC R5.1 / R5.1.1 / R5.2.2).
//!
//! Regression test for the hardening of the *direct-stdio* configuration path
//! (`configure_sandbox_from_roots`) — the path used when an MCP client talks to
//! `ahma serve stdio` WITHOUT the HTTP bridge in front of it.
//!
//! The HTTP bridge already swallows a post-lock `roots/list_changed` as a
//! tolerated no-op (so its subprocess never receives a second one). But a pure
//! stdio client can send `roots/list_changed` repeatedly. Before the fix the
//! server would re-query `roots/list` and re-derive scope every time, which
//! could widen the locked sandbox.
//!
//! After the fix the scope is committed exactly once: a `roots/list_changed`
//! that arrives after the sandbox is committed is ignored WITHOUT re-querying
//! `roots/list`. This test drives a real stdio server and asserts that the
//! second `roots/list_changed` produces NO second `roots/list` request.
//!
//! Runs in `--no-sandbox` (test) mode so it exercises the commit-latch logic
//! across all platforms without depending on kernel enforcement.

use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use ahma_mcp::test_utils;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;

/// RAII guard that kills and reaps the child process on drop.
struct ChildGuard(Option<Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Build a `file://` URI for a path that round-trips on every platform.
fn file_uri(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    if s.starts_with('/') {
        format!("file://{s}") // unix: file:///var/folders/...
    } else {
        format!("file:///{s}") // windows: file:///C:/Users/...
    }
}

/// Event emitted by the stdout reader thread.
enum ServerEvent {
    InitResult,
    /// A server→client `roots/list` request with its JSON-RPC id.
    RootsListRequest(Value),
    SandboxConfigured,
}

fn spawn_reader(
    stdout: std::process::ChildStdout,
    tx: mpsc::Sender<ServerEvent>,
) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut log = String::new();
        for line in reader.lines().map_while(Result::ok) {
            log.push_str(&line);
            log.push('\n');

            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let method = v.get("method").and_then(|m| m.as_str());
            if method == Some("roots/list") {
                if let Some(id) = v.get("id") {
                    let _ = tx.send(ServerEvent::RootsListRequest(id.clone()));
                }
            } else if method == Some("notifications/sandbox/configured") {
                let _ = tx.send(ServerEvent::SandboxConfigured);
            } else if v.get("id").and_then(|i| i.as_i64()) == Some(1) && v.get("result").is_some() {
                let _ = tx.send(ServerEvent::InitResult);
            }
        }
        log
    })
}

/// After the sandbox is committed from the first client roots, a second
/// `roots/list_changed` must NOT cause the server to re-request `roots/list`.
#[test]
fn direct_stdio_second_roots_changed_does_not_requery() {
    let binary = test_utils::cli::build_binary_cached("ahma_bin", "ahma");
    let workspace = tempfile::tempdir().unwrap();
    let tools_dir = workspace.path().join("tools");
    std::fs::create_dir(&tools_dir).unwrap();

    // --defer-sandbox: wait for roots/list_changed before configuring (the
    //   direct-stdio configuration path under test).
    // --no-sandbox: exercise the commit-latch logic without kernel enforcement
    //   so the test is platform-agnostic.
    // No --sandbox-scope: scope must be DERIVED from client roots, otherwise the
    //   roots/list request is skipped entirely (SPEC R5.5).
    let child = Command::new(&binary)
        .args(["serve", "stdio"])
        .arg("--no-sandbox")
        .arg("--defer-sandbox")
        .arg("--skip-probes")
        .arg("--tools-dir")
        .arg(&tools_dir)
        .current_dir(workspace.path())
        .env("AHMA_SERVER_CHILD", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("Failed to spawn ahma serve stdio");

    let mut guard = ChildGuard(Some(child));
    let child_ref = guard.0.as_mut().unwrap();
    let mut stdin = child_ref.stdin.take().expect("stdin");
    let stdout = child_ref.stdout.take().expect("stdout");

    let (tx, rx) = mpsc::channel::<ServerEvent>();
    let reader = spawn_reader(stdout, tx);

    let handshake = TestTimeouts::get(TimeoutCategory::Handshake);

    // 1. initialize
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{"roots":{"listChanged":true}},"clientInfo":{"name":"direct-stdio-test","version":"1.0"}}}"#;
    stdin.write_all(init.as_bytes()).unwrap();
    stdin.write_all(b"\n").unwrap();

    // 2. notifications/initialized
    let initialized = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    stdin.write_all(initialized.as_bytes()).unwrap();
    stdin.write_all(b"\n").unwrap();

    // Helper: wait for a specific event, draining others, until timeout.
    let wait_for = |rx: &mpsc::Receiver<ServerEvent>,
                    pred: &dyn Fn(&ServerEvent) -> bool,
                    timeout: std::time::Duration|
     -> Option<ServerEvent> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match rx.recv_timeout(remaining) {
                Ok(ev) if pred(&ev) => return Some(ev),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    };

    assert!(
        wait_for(&rx, &|e| matches!(e, ServerEvent::InitResult), handshake).is_some(),
        "did not receive initialize result"
    );

    // 3. First roots/list_changed → server should request roots/list.
    let roots_changed = r#"{"jsonrpc":"2.0","method":"notifications/roots/list_changed"}"#;
    stdin.write_all(roots_changed.as_bytes()).unwrap();
    stdin.write_all(b"\n").unwrap();

    let first_req = wait_for(
        &rx,
        &|e| matches!(e, ServerEvent::RootsListRequest(_)),
        handshake,
    )
    .expect("server must request roots/list after first roots/list_changed");
    let ServerEvent::RootsListRequest(req_id) = first_req else {
        unreachable!()
    };

    // 4. Respond with the workspace root → server commits the sandbox.
    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "result": { "roots": [ { "uri": file_uri(workspace.path()), "name": "workspace" } ] }
    });
    stdin
        .write_all(serde_json::to_string(&response).unwrap().as_bytes())
        .unwrap();
    stdin.write_all(b"\n").unwrap();

    assert!(
        wait_for(
            &rx,
            &|e| matches!(e, ServerEvent::SandboxConfigured),
            handshake
        )
        .is_some(),
        "server must emit notifications/sandbox/configured after first roots response"
    );

    // 5. Second roots/list_changed AFTER commit must be a no-op: NO new
    //    roots/list request. This is the direct-stdio hardening under test.
    stdin.write_all(roots_changed.as_bytes()).unwrap();
    stdin.write_all(b"\n").unwrap();

    let second_req = wait_for(
        &rx,
        &|e| matches!(e, ServerEvent::RootsListRequest(_)),
        TestTimeouts::scale_secs(2),
    );
    assert!(
        second_req.is_none(),
        "post-commit roots/list_changed must NOT trigger a second roots/list request \
         (scope is committed once and never re-derived; SPEC R5.2.2)"
    );

    drop(stdin);
    let mut child = guard.0.take().unwrap();
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
}
