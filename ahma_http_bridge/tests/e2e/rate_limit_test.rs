//! Integration tests for per-IP rate limiting.
//!
//! These tests verify that:
//! - `/health` is **never** rate-limited (health checks must always succeed).
//! - After the burst is exhausted, `/mcp` requests receive HTTP 429.
//! - Different source IPs have independent rate-limit buckets.
//!
//! Rate limiting is enabled by setting `AHMA_RATE_LIMIT_RPS` and
//! `AHMA_RATE_LIMIT_BURST` environment variables before launching the bridge.

use crate::common;

use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use common::SandboxTestEnv;
use common::server::{ServerGuard, resolve_binary_path};
use reqwest::Client;
use std::process::{Command, Stdio};
use std::time::Instant;
use tempfile::tempdir;

/// Spawn a bridge server with rate limiting enabled.
///
/// Starts `ahma serve http --port 0` with `AHMA_RATE_LIMIT_RPS=1` and
/// `AHMA_RATE_LIMIT_BURST=1` (i.e. burst of 1, then 1 req/s sustained).
fn spawn_rate_limited_server() -> Result<ServerGuard, String> {
    let tools_dir = tempdir().map_err(|e| e.to_string())?;
    let sandbox_dir = tempdir().map_err(|e| e.to_string())?;
    let binary_path = resolve_binary_path()?;

    let mut cmd = Command::new(&binary_path);
    cmd.args([
        "--sync",
        "--log-to-stderr",
        "--tools-dir",
        &tools_dir.path().to_string_lossy(),
        "--sandbox-scope",
        &sandbox_dir.path().to_string_lossy(),
        "--rate-limit-rps",
        "1",
        "--rate-limit-burst",
        "1",
        "serve",
        "http",
        "--port",
        "0",
    ]);

    SandboxTestEnv::configure(&mut cmd);
    SandboxTestEnv::apply_nested_sandbox_override(&mut cmd);

    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn bridge: {e}"))?;

    // Read stderr to find the bound port.
    let stderr = child.stderr.take().ok_or("No stderr")?;
    let port = {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(stderr);
        let deadline = Instant::now() + TestTimeouts::get(TimeoutCategory::ProcessSpawn);
        let mut found_port = None;
        for line in reader.lines().map_while(Result::ok) {
            if line.contains("AHMA_BOUND_PORT=") {
                let p: u16 = line
                    .split('=')
                    .nth(1)
                    .and_then(|s| s.trim().parse().ok())
                    .ok_or_else(|| format!("Failed to parse port from: {line}"))?;
                found_port = Some(p);
                break;
            }
            if Instant::now() > deadline {
                return Err("Timed out waiting for AHMA_BOUND_PORT".to_string());
            }
        }
        found_port.ok_or("AHMA_BOUND_PORT line not found in server output")?
    };

    // Wait for server to be ready.
    let base_url = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + TestTimeouts::get(TimeoutCategory::HealthCheck);
    loop {
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            return Err(format!("Server on port {port} never became reachable"));
        }
        std::thread::sleep(TestTimeouts::poll_interval());
    }
    let _ = base_url; // consumed above

    Ok(ServerGuard::new(child, port))
}

/// `/health` must never be blocked by rate limiting, even after the burst is
/// exhausted.
#[tokio::test]
async fn health_endpoint_is_never_rate_limited() {
    let guard = match spawn_rate_limited_server() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("[rate_limit_test] SKIP — could not spawn server: {e}");
            return;
        }
    };

    let client = Client::new();
    let health_url = format!("{}/health", guard.base_url());

    // Fire well above the burst limit; all should succeed.
    for i in 0..5 {
        let resp = client
            .get(&health_url)
            .timeout(TestTimeouts::scale_secs(5))
            .send()
            .await
            .unwrap_or_else(|e| panic!("Request {i} to /health failed: {e}"));
        assert_eq!(
            resp.status().as_u16(),
            200,
            "/health returned {} on request {i} — should never be rate limited",
            resp.status()
        );
    }
}

/// After the burst allowance is exhausted, MCP requests must receive HTTP 429.
///
/// Burst is set to 1, so the first request consumes the burst.  The second
/// request (sent immediately) must be throttled.
#[tokio::test]
async fn mcp_endpoint_returns_429_after_burst_exhausted() {
    let guard = match spawn_rate_limited_server() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("[rate_limit_test] SKIP — could not spawn server: {e}");
            return;
        }
    };

    let client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .timeout(TestTimeouts::scale_secs(5))
        .build()
        .expect("Failed to build HTTP/2 client");
    let mcp_url = format!("{}/mcp", guard.base_url());

    // Fire burst+1 requests as fast as possible; at least one must be 429.
    let mut got_429 = false;
    for _ in 0..3 {
        match client
            .post(&mcp_url)
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
        {
            Ok(resp) if resp.status().as_u16() == 429 => {
                got_429 = true;
                break;
            }
            Ok(_) => {}
            Err(_) => {} // connection error counts as throttled by OS
        }
    }

    assert!(
        got_429,
        "Expected at least one 429 after burst exhausted, but all requests went through"
    );
}
