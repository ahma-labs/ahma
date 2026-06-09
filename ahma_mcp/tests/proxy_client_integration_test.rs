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

    // Create UDS path inside the workspace target directory to satisfy sandbox path limits
    let rand_id = rand::random::<u32>();
    let socket_path = workspace
        .join("target")
        .join(format!("ahma_test_{}.sock", rand_id));
    let socket_str = socket_path.to_string_lossy().into_owned();

    // Clean up if a stale file exists
    let _ = std::fs::remove_file(&socket_path);

    // Find a free TCP port to avoid conflicts.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind free port")
        .local_addr()
        .expect("local_addr")
        .port();

    // 1. Spawning the server using `serve stdio` command.
    // Since it's the first instance, it should start the background bridge Unix socket at socket_str.
    let mut child = tokio::process::Command::new(&binary)
        .current_dir(&workspace)
        .env("RUST_LOG", "debug")
        .env("AHMA_HTTP_PORT", port.to_string())
        .env("AHMA_UNIX_SOCKET", &socket_str)
        .env_remove("NEXTEST")
        .env_remove("CARGO_MANIFEST_DIR")
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
        .env("AHMA_HTTP_PORT", port.to_string())
        .env("AHMA_UNIX_SOCKET", &socket_str)
        .env_remove("NEXTEST")
        .env_remove("CARGO_MANIFEST_DIR")
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
