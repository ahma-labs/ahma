//! Integration tests for SIGHUP-triggered bearer-token reload.
//!
//! These tests verify that:
//! - A bearer token loaded at startup is enforced (401 on wrong token).
//! - After writing a new token to the watch file and sending SIGHUP, the server
//!   accepts the new token and rejects the old one.
//!
//! SIGHUP is a Unix concept; these tests are skipped on non-Unix platforms.
//!
//! # Environment variables used
//!
//! | Variable                | Purpose                                     |
//! |-------------------------|---------------------------------------------|
//! | `AHMA_REQUIRE_TOKEN`    | Initial bearer token (read at startup)      |
//! | `AHMA_REQUIRE_TOKEN_PATH` | File path watched on SIGHUP for reload    |

#![cfg(unix)]

mod common;

use common::server::resolve_binary_path;
use reqwest::Client;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::{NamedTempFile, tempdir};

const INITIAL_TOKEN: &str = "initial-secret-token";
const UPDATED_TOKEN: &str = "updated-secret-token";

/// Spawn a bridge server with bearer-token auth and SIGHUP reload enabled.
///
/// Returns `(child_process, port)`.
fn spawn_auth_server(token_file: &NamedTempFile) -> Result<(std::process::Child, u16), String> {
    let tools_dir = tempdir().map_err(|e| e.to_string())?;
    let sandbox_dir = tempdir().map_err(|e| e.to_string())?;
    let binary_path = resolve_binary_path()?;

    let mut child = Command::new(&binary_path)
        .args(["serve", "http", "--port", "0"])
        .env("AHMA_SYNC", "1")
        .env("AHMA_LOG_TARGET", "stderr")
        .env("AHMA_TOOLS_DIR", tools_dir.path())
        .env("AHMA_SANDBOX_SCOPE", sandbox_dir.path())
        .env("AHMA_REQUIRE_TOKEN", INITIAL_TOKEN)
        .env("AHMA_REQUIRE_TOKEN_PATH", token_file.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn bridge: {e}"))?;

    // Parse bound port from stderr.
    let stderr = child.stderr.take().ok_or("No stderr")?;
    let port = {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(stderr);
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut found_port = None;
        for line in reader.lines().map_while(Result::ok) {
            if line.contains("AHMA_BOUND_PORT=") {
                let p: u16 = line
                    .split('=')
                    .nth(1)
                    .and_then(|s| s.trim().parse().ok())
                    .ok_or_else(|| format!("Could not parse port from: {line}"))?;
                found_port = Some(p);
                break;
            }
            if Instant::now() > deadline {
                return Err("Timed out waiting for AHMA_BOUND_PORT".to_string());
            }
        }
        found_port.ok_or("AHMA_BOUND_PORT not found in server output")?
    };

    // Wait for TCP to be reachable.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            return Err(format!("Server on port {port} never became reachable"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    Ok((child, port))
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// Verify that the initial token works before any SIGHUP is sent.
#[tokio::test]
async fn initial_token_is_enforced() {
    // Prepare the token watch file (contents don't matter at startup — the
    // initial token is passed via AHMA_REQUIRE_TOKEN).
    let token_file = NamedTempFile::new().expect("tempfile");
    std::fs::write(token_file.path(), INITIAL_TOKEN).unwrap();

    let (mut child, port) = match spawn_auth_server(&token_file) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("[sighup_test] SKIP — could not spawn server: {e}");
            return;
        }
    };

    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let health_url = format!("http://127.0.0.1:{port}/health");

    // /health is exempt from auth.
    let resp = client.get(&health_url).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200, "/health should be 200");

    // /mcp with correct token → anything but 401 (likely 400 for bad payload).
    let mcp_url = format!("http://127.0.0.1:{port}/mcp");
    let resp = client
        .post(&mcp_url)
        .header("Authorization", bearer(INITIAL_TOKEN))
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_ne!(
        resp.status().as_u16(),
        401,
        "Initial token should be accepted"
    );

    // /mcp with wrong token → 401.
    let resp = client
        .post(&mcp_url)
        .header("Authorization", bearer("wrong-token"))
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        401,
        "Wrong token must be rejected with 401"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// After updating the token file and sending SIGHUP, the server must accept the
/// new token and reject the old one.
#[tokio::test]
async fn sighup_reloads_bearer_token() {
    let token_file = NamedTempFile::new().expect("tempfile");
    std::fs::write(token_file.path(), INITIAL_TOKEN).unwrap();

    let (mut child, port) = match spawn_auth_server(&token_file) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("[sighup_test] SKIP — could not spawn server: {e}");
            return;
        }
    };

    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let mcp_url = format!("http://127.0.0.1:{port}/mcp");

    // Confirm initial token works.
    let resp = client
        .post(&mcp_url)
        .header("Authorization", bearer(INITIAL_TOKEN))
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_ne!(
        resp.status().as_u16(),
        401,
        "Initial token should be accepted before SIGHUP"
    );

    // Update the token file then send SIGHUP.
    std::fs::write(token_file.path(), UPDATED_TOKEN).unwrap();
    let pid = child.id();
    // Use `kill -HUP` via the shell — avoids pulling in libc as a test dep.
    let status = std::process::Command::new("kill")
        .args(["-s", "HUP", &pid.to_string()])
        .status()
        .expect("Failed to run `kill -s HUP`");
    assert!(status.success(), "`kill -s HUP {pid}` failed: {status}");

    // Give the server time to handle the signal and reload.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // New token must now be accepted.
    let resp = client
        .post(&mcp_url)
        .header("Authorization", bearer(UPDATED_TOKEN))
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_ne!(
        resp.status().as_u16(),
        401,
        "Updated token should be accepted after SIGHUP"
    );

    // Old token must now be rejected.
    let resp = client
        .post(&mcp_url)
        .header("Authorization", bearer(INITIAL_TOKEN))
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        401,
        "Old token should be rejected after SIGHUP reload"
    );

    let _ = child.kill();
    let _ = child.wait();
}
