//! SPEC R-ISO.5: a bridge a test harness launched dies with its launcher.
//!
//! An operator-started `ahma serve http` outlives whoever launched it, by
//! design. A test-launched one must not: a test's `Drop` guard never runs when
//! the test process is SIGKILLed (a nextest timeout, Ctrl-C), and such a server
//! was once found still running two days after its test run.
//!
//! Unix-only: the parent-death watchdog is `getppid()`-based, and on Windows
//! Job Objects own orphan reaping.
#![cfg(unix)]

use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use ahma_mcp::test_utils::cli::build_binary_cached;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, BufReader};

fn is_alive(pid: i32) -> bool {
    // SAFETY: signal 0 performs only the existence/permission check.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[tokio::test]
async fn test_launched_http_bridge_exits_when_its_launcher_is_killed() {
    let binary = build_binary_cached("ahma_bin", "ahma");
    let tools = tempfile::tempdir().unwrap();

    // An intermediate launcher that backgrounds the bridge and prints its pid:
    // killing the launcher reproduces a test process dying without cleanup.
    let script = r#""$0" --no-sandbox --skip-probes --log-to-stderr --tools-dir "$1" serve http --port 0 & echo $!; wait"#;
    let mut launcher = tokio::process::Command::new("sh")
        .args(["-c", script])
        .arg(&binary)
        .arg(tools.path())
        .env("AHMA_TEST_ISOLATION", "1")
        .env("RUST_LOG", "info")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .spawn()
        .expect("spawn launcher");

    let mut stdout = BufReader::new(launcher.stdout.take().unwrap()).lines();
    let pid: i32 = stdout
        .next_line()
        .await
        .unwrap()
        .expect("launcher printed the bridge pid")
        .trim()
        .parse()
        .unwrap();

    // Wait until the bridge is really up, so the test cannot pass on a server
    // that died of something else.
    let mut stderr = BufReader::new(launcher.stderr.take().unwrap()).lines();
    let started = tokio::time::timeout(TestTimeouts::get(TimeoutCategory::ProcessSpawn), async {
        while let Ok(Some(line)) = stderr.next_line().await {
            if line.contains("Starting HTTP bridge") {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(started, "bridge never reported starting");
    assert!(
        is_alive(pid),
        "bridge must be running before its launcher dies"
    );
    // Keep draining the bridge's stderr so it never blocks on a full pipe.
    tokio::spawn(async move { while let Ok(Some(_)) = stderr.next_line().await {} });

    launcher.start_kill().unwrap();
    let _ = launcher.wait().await;

    let deadline = Instant::now() + TestTimeouts::get(TimeoutCategory::Cleanup);
    while is_alive(pid) && Instant::now() < deadline {
        tokio::time::sleep(TestTimeouts::poll_interval()).await;
    }
    let survived = is_alive(pid);
    if survived {
        // SAFETY: plain kill of the pid this test started.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    assert!(
        !survived,
        "a test-launched bridge must exit when its launcher dies (SPEC R-ISO.5)"
    );
}
