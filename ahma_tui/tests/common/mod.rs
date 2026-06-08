//! Shared test helpers for ahma_tui integration tests.
#![allow(dead_code)] // helpers are selectively used by cfg-gated test binaries
use ahma_common::timeouts::TestTimeouts;
use ahma_http_bridge::{BridgeConfig, ListenerKind, start_bridge};
use std::net::SocketAddr;
use std::time::Duration;

/// In-process bridge handle that aborts the server task when dropped.
pub struct BridgeHandle {
    pub base_url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for BridgeHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Start an in-process HTTP bridge on a random free TCP port.
///
/// Uses the port-0 pre-bind trick to discover a free ephemeral port, then
/// starts the bridge on that port. Polls `/health` until ready before returning.
///
/// `server_command` is a no-op binary — the bridge only spawns it when an MCP
/// session is created, so health-check-only tests never trigger a subprocess.
pub async fn start_bridge_tcp(enable_quic: bool) -> BridgeHandle {
    let tmp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind port 0 to discover free port");
    let port = tmp.local_addr().expect("local_addr").port();
    drop(tmp);

    let bind_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let config = BridgeConfig {
        bind_addr,
        listener_kind: ListenerKind::Tcp(bind_addr),
        #[cfg(unix)]
        server_command: "false".to_string(),
        #[cfg(not(unix))]
        server_command: "cmd".to_string(),
        enable_quic,
        ..BridgeConfig::default()
    };

    let task = tokio::spawn(async move {
        let _ = start_bridge(config).await;
    });

    let base_url = format!("http://127.0.0.1:{port}");
    wait_for_health(&base_url).await;
    BridgeHandle { base_url, task }
}

/// In-process Unix-socket bridge handle that aborts and cleans up on drop.
#[cfg(unix)]
pub struct UnixBridgeHandle {
    pub socket_path: std::path::PathBuf,
    task: tokio::task::JoinHandle<()>,
}

#[cfg(unix)]
impl Drop for UnixBridgeHandle {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Start an in-process HTTP bridge on a Unix domain socket.
/// Waits until the socket file appears (up to 5 s) before returning.
#[cfg(unix)]
pub async fn start_bridge_unix(socket_path: std::path::PathBuf) -> UnixBridgeHandle {
    let path_str = socket_path.to_string_lossy().into_owned();
    let dummy_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

    let config = BridgeConfig {
        bind_addr: dummy_addr,
        listener_kind: ListenerKind::Unix(path_str),
        server_command: "false".to_string(),
        enable_quic: false,
        ..BridgeConfig::default()
    };

    let task = tokio::spawn(async move {
        let _ = start_bridge(config).await;
    });

    let poll_interval = TestTimeouts::poll_interval();
    let max_attempts =
        (TestTimeouts::scale_secs(5).as_millis() / poll_interval.as_millis()).max(1) as usize;
    for _ in 0..max_attempts {
        tokio::time::sleep(poll_interval).await;
        if socket_path.exists() {
            return UnixBridgeHandle { socket_path, task };
        }
    }
    panic!(
        "Unix socket {} did not appear within scaled 5 seconds",
        socket_path.display()
    );
}

/// Poll `GET {base_url}/health` at 50 ms intervals until 2xx response or 5 s elapsed.
pub async fn wait_for_health(base_url: &str) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap_or_default();
    let url = format!("{base_url}/health");
    let poll_interval = TestTimeouts::poll_interval();
    let max_attempts =
        (TestTimeouts::scale_secs(5).as_millis() / poll_interval.as_millis()).max(1) as usize;
    for _ in 0..max_attempts {
        tokio::time::sleep(poll_interval).await;
        if client
            .get(&url)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            return;
        }
    }
    panic!("Server at {base_url} did not become healthy within scaled 5 seconds");
}
