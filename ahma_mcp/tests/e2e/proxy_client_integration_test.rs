use ahma_mcp::test_utils::cli::build_binary_cached;
use ahma_mcp::test_utils::fs::get_workspace_dir as workspace_dir;
use std::time::{Duration, Instant};

fn build_binary() -> std::path::PathBuf {
    build_binary_cached("ahma_bin", "ahma")
}

#[tokio::test]
async fn test_proxy_client_autostart_and_shutdown() {
    let binary = build_binary();
    let workspace = workspace_dir();

    // Create the UDS path in the OS temp dir, not the workspace tree: this test
    // passes `--no-sandbox` to the spawned process, so there is no sandbox-scope
    // reason to keep the socket inside the workspace, and a workspace-rooted path
    // (e.g. under a deeply nested git worktree at `.claude/worktrees/agent-<hex>/`)
    // can exceed the OS's `sockaddr_un.sun_path` capacity (~103 bytes on macOS,
    // ~107 on Linux). `std::env::temp_dir()` stays short regardless of workspace
    // nesting depth. See `ahma_common::test_isolation` for the same pattern.
    let rand_id = rand::random::<u32>();
    let socket_path = std::env::temp_dir().join(format!("ahma_test_{}.sock", rand_id));
    let socket_str = socket_path.to_string_lossy().into_owned();

    // Clean up if a stale file exists
    let _ = std::fs::remove_file(&socket_path);

    // 1. Spawning the server using `serve stdio` command.
    // Since it's the first instance, it should start the background bridge Unix socket at socket_str.
    let mut child = tokio::process::Command::new(&binary)
        .current_dir(&workspace)
        .env("RUST_LOG", "debug")
        .args([
            "--no-sandbox",
            "--unix-socket-path",
            &socket_str,
            "--log-to-stderr",
            "serve",
            "stdio",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn host server");

    use tokio::io::AsyncBufReadExt;
    let mut child_stderr = tokio::io::BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();

    // 2. Poll until the Unix socket file is created and healthy (up to 5s)
    let start = Instant::now();
    let mut healthy = false;
    while start.elapsed() < Duration::from_secs(5) {
        // Read any available stderr output from the child
        while let Ok(Ok(n)) =
            tokio::time::timeout(Duration::from_millis(5), child_stderr.read_line(&mut line)).await
        {
            if n == 0 {
                break;
            }
            eprintln!("[CHILD STDERR] {}", line.trim());
            line.clear();
        }

        #[cfg(unix)]
        if tokio::net::UnixStream::connect(&socket_path).await.is_ok() {
            healthy = true;
            break;
        }
        #[cfg(not(unix))]
        {
            healthy = true;
            break;
        }
        #[cfg(unix)]
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    if !healthy {
        // Read remaining stderr
        while let Ok(Ok(n)) = tokio::time::timeout(
            Duration::from_millis(100),
            child_stderr.read_line(&mut line),
        )
        .await
        {
            if n == 0 {
                break;
            }
            eprintln!("[CHILD STDERR] {}", line.trim());
            line.clear();
        }

        let status = child.try_wait().ok().flatten();
        eprintln!(
            "Child server status when socket was not found: {:?}",
            status
        );
        let _ = child.kill().await;
        let _ = std::fs::remove_file(&socket_path);
        panic!(
            "Background Unix socket bridge should start and be connectable. Child was spawned but socket was not found. Exit status: {:?}",
            status
        );
    }

    // 3. Spawning a second instance using `serve stdio` (with the same unix socket path).
    // Since the Unix socket is already running, it should run as a proxy client.
    let mut child_proxy = tokio::process::Command::new(&binary)
        .current_dir(&workspace)
        .args([
            "--no-sandbox",
            "--unix-socket-path",
            &socket_str,
            "--log-to-stderr",
            "serve",
            "stdio",
        ])
        .stdout(std::process::Stdio::piped())
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn proxy client");

    // Give it a moment to connect
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 4. Kill the proxy client and the host client stdin to trigger the idle timeout
    let _ = child_proxy.kill().await;
    let _ = child_proxy.wait().await;

    // Close host stdin
    drop(child.stdin.take());

    // Wait for the host process to terminate due to the 10-second idle shutdown
    let wait_start = Instant::now();
    let mut exited = false;
    while wait_start.elapsed() < Duration::from_secs(15) {
        if let Ok(Some(_status)) = child.try_wait() {
            exited = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    if !exited {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }

    let _ = std::fs::remove_file(&socket_path);

    #[cfg(unix)]
    assert!(
        exited,
        "Host server should exit cleanly within 15 seconds after stdin closes and 0 active sessions remain"
    );
}

/// Regression guard for the orphaned-`serve stdio` storm: a frontend that is
/// spawned but never sent an MCP handshake (no `initialize`), with its stdin
/// held OPEN (so the stdin-EOF exit path can NOT fire), must still terminate on
/// its own via the handshake deadline. Without this, an editor that repeatedly
/// spawns and abandons MCP servers piles up thousands of live processes.
#[cfg(unix)]
#[tokio::test]
async fn test_frontend_exits_when_handshake_never_arrives() {
    let binary = build_binary();
    let workspace = workspace_dir();

    // Same rationale as above: no sandbox-scope reason to keep the socket inside
    // the workspace (this test also passes `--no-sandbox`), and the OS temp dir
    // stays well under the `sockaddr_un.sun_path` length limit regardless of how
    // deeply nested the workspace checkout is.
    let rand_id = rand::random::<u32>();
    let socket_path = std::env::temp_dir().join(format!("ahma_test_nohs_{}.sock", rand_id));
    let socket_str = socket_path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&socket_path);

    // Spawn the frontend with a short handshake deadline. stdin is piped and we
    // keep the handle (never write, never close), so ONLY the handshake deadline
    // can terminate it.
    let mut child = tokio::process::Command::new(&binary)
        .current_dir(&workspace)
        .env("AHMA_FRONTEND_HANDSHAKE_DEADLINE_SECS", "2")
        .args([
            "--no-sandbox",
            "--unix-socket-path",
            &socket_str,
            "--log-to-stderr",
            "serve",
            "stdio",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn frontend");

    // Hold stdin open for the whole test: dropping it would deliver EOF and let
    // the test pass for the wrong reason.
    let _stdin = child.stdin.take().expect("stdin piped");

    // The frontend must exit on its own: deadline (2s) + background-bridge
    // startup + margin. Generous upper bound, but well under any EOF/idle path
    // (stdin is still open, so EOF can't be why it exited).
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut exit_status = None;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            exit_status = Some(status);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    if exit_status.is_none() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    let _ = std::fs::remove_file(&socket_path);

    assert!(
        exit_status.is_some(),
        "Frontend must self-terminate via the handshake deadline when no \
         handshake arrives, even with stdin held open"
    );
}
