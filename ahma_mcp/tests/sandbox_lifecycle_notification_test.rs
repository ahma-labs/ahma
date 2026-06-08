use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use ahma_mcp::test_utils;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::thread;

/// RAII guard that kills and reaps the child process on drop.
/// Prevents leaking zombie processes when assertions fail mid-test.
struct ChildGuard(Option<Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn_mcp_server(binary: &Path, temp_dir: &Path, tools_dir: &Path) -> Child {
    // AHMA_DISABLE_SANDBOX=1: this test verifies lifecycle notification emission,
    // not sandbox enforcement. Disabling avoids Landlock/seatbelt interactions and
    // makes the test identical across all CI platforms.
    // AHMA_SKIP_PROBES=1: no tools need availability probing; skip the startup delay.
    Command::new(binary)
        .args(["serve", "stdio"])
        .current_dir(temp_dir)
        .env("AHMA_SANDBOX_SCOPE", temp_dir)
        .env("AHMA_TOOLS_DIR", tools_dir)
        .env("AHMA_DISABLE_SANDBOX", "1")
        .env("AHMA_SKIP_PROBES", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn ahma_mcp")
}

fn read_stdout_for_notifications(
    stdout: ChildStdout,
    init_ok_tx: mpsc::Sender<()>,
    tools_ok_tx: mpsc::Sender<()>,
) -> (String, bool) {
    let reader = BufReader::new(stdout);
    let mut output_log = String::new();
    let mut seen_terminated = false;
    let mut init_acked = false;
    let mut tools_acked = false;

    for line in reader.lines() {
        let line = line.expect("Failed to read line");
        output_log.push_str(&line);
        output_log.push('\n');

        if !init_acked && line.contains("\"id\":1") && line.contains("\"result\"") {
            let _ = init_ok_tx.send(());
            init_acked = true;
        }
        if !tools_acked && line.contains("\"id\":2") && line.contains("\"result\"") {
            let _ = tools_ok_tx.send(());
            tools_acked = true;
        }
        if line.contains("notifications/sandbox/terminated") {
            seen_terminated = true;
        }
    }
    (output_log, seen_terminated)
}

fn perform_mcp_handshake(
    stdin: &mut dyn Write,
    init_ok_rx: &mpsc::Receiver<()>,
    tools_ok_rx: &mpsc::Receiver<()>,
) -> (bool, bool) {
    let timeout = TestTimeouts::get(TimeoutCategory::Handshake);

    let init_req = r#"{"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test", "version": "1.0"} }}"#;
    stdin.write_all(init_req.as_bytes()).unwrap();
    stdin.write_all(b"\n").unwrap();

    let init_ok = init_ok_rx.recv_timeout(timeout).is_ok();
    if !init_ok {
        return (false, false);
    }

    let initialized_notif = r#"{"jsonrpc": "2.0", "method": "notifications/initialized"}"#;
    stdin.write_all(initialized_notif.as_bytes()).unwrap();
    stdin.write_all(b"\n").unwrap();

    let tools_req = r#"{"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}"#;
    stdin.write_all(tools_req.as_bytes()).unwrap();
    stdin.write_all(b"\n").unwrap();

    let tools_ok = tools_ok_rx.recv_timeout(timeout).is_ok();
    (true, tools_ok)
}

#[test]
fn test_sandbox_lifecycle_notifications() {
    let binary = test_utils::cli::build_binary_cached("ahma_bin", "ahma");
    let temp_dir = tempfile::tempdir().unwrap();
    let tools_dir = temp_dir.path().join("tools");
    std::fs::create_dir(&tools_dir).unwrap();

    let child = spawn_mcp_server(&binary, temp_dir.path(), &tools_dir);
    let mut guard = ChildGuard(Some(child));
    let child_ref = guard.0.as_mut().unwrap();

    let mut stdin = child_ref.stdin.take().expect("Failed to open stdin");
    let stdout = child_ref.stdout.take().expect("Failed to open stdout");
    let stderr = child_ref.stderr.take().expect("Failed to open stderr");

    let stderr_handle = thread::spawn(move || {
        let reader = BufReader::new(stderr);
        let mut err_log = String::new();
        for line in reader.lines().map_while(Result::ok) {
            err_log.push_str(&line);
            err_log.push('\n');
        }
        err_log
    });

    let (init_ok_tx, init_ok_rx) = mpsc::channel::<()>();
    let (tools_ok_tx, tools_ok_rx) = mpsc::channel::<()>();
    let handle =
        thread::spawn(move || read_stdout_for_notifications(stdout, init_ok_tx, tools_ok_tx));

    let (init_ok, tools_ok) = perform_mcp_handshake(&mut stdin, &init_ok_rx, &tools_ok_rx);

    if !init_ok || !tools_ok {
        if let Some(child) = guard.0.as_mut() {
            let _ = child.kill();
        }
        let stderr_log = stderr_handle.join().unwrap_or_default();
        let (stdout_log, _) = handle.join().unwrap_or_default();
        let which = if !init_ok { "initialize" } else { "tools/list" };
        panic!(
            "Timed out waiting for {} response from server.\n\nSTDOUT LOG:\n{}\n\nSTDERR LOG:\n{}",
            which, stdout_log, stderr_log
        );
    }

    drop(stdin);

    let mut child = guard.0.take().unwrap();
    let _ = child.wait().expect("Failed to wait on child");

    let (log, seen) = handle.join().expect("Thread panicked");
    let err_log = stderr_handle.join().unwrap_or_default();

    if !seen {
        println!("STDOUT LOG:\n{}", log);
        println!("STDERR LOG:\n{}", err_log);
    }

    assert!(
        seen,
        "Did not see sandbox/terminated notification in output"
    );
    assert!(
        log.contains("session_ended"),
        "Notification reason mismatch in log: {}",
        log
    );
}
