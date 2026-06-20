mod common;

use common::{McpTestClient, TransportMode, spawn_in_process_server};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

#[derive(Clone, Copy, Debug)]
enum PeerBehavior {
    SendFakePing,
    SendRealRoots,
}

struct FakePeerFactory {
    behavior: PeerBehavior,
}

impl ahma_common::peer_factory::PeerFactory for FakePeerFactory {
    fn create(
        &self,
    ) -> ahma_common::peer_factory::BoxFuture<anyhow::Result<ahma_common::peer_factory::PeerStreams>>
    {
        let behavior = self.behavior;
        Box::pin(async move {
            let (bridge_end, peer_end) = tokio::io::duplex(4096);
            let (bridge_read, bridge_write) = tokio::io::split(bridge_end);
            let (peer_read, mut peer_write) = tokio::io::split(peer_end);

            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
                let mut reader = BufReader::new(peer_read);
                let mut line = String::new();

                println!("[FakePeer {:?}] Spawned", behavior);

                // 1. Read initialize request
                if let Ok(n) = reader.read_line(&mut line).await {
                    println!(
                        "[FakePeer {:?}] Received init: {} bytes, content: {}",
                        behavior,
                        n,
                        line.trim()
                    );
                    if let Ok(req) = serde_json::from_str::<Value>(&line)
                        && req.get("method").and_then(|m| m.as_str()) == Some("initialize")
                    {
                        let req_id = req.get("id").unwrap();
                        let resp = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": req_id,
                            "result": {
                                "protocolVersion": "2024-11-05",
                                "capabilities": {
                                    "roots": {
                                        "listChanged": true
                                    }
                                },
                                "serverInfo": {
                                    "name": "fake-peer",
                                    "version": "1.0"
                                }
                            }
                        });
                        let mut resp_str = serde_json::to_string(&resp).unwrap();
                        resp_str.push('\n');
                        println!(
                            "[FakePeer {:?}] Sending init response: {}",
                            behavior,
                            resp_str.trim()
                        );
                        let _ = peer_write.write_all(resp_str.as_bytes()).await;
                    }
                }

                // 2. Read notifications/initialized
                line.clear();
                if let Ok(n) = reader.read_line(&mut line).await {
                    println!(
                        "[FakePeer {:?}] Received post-init line: {} bytes, content: {}",
                        behavior,
                        n,
                        line.trim()
                    );
                }

                // 3. Send server-to-client request based on behavior
                match behavior {
                    PeerBehavior::SendFakePing => {
                        let ping = "{\"jsonrpc\":\"2.0\",\"id\":\"fake-ping-id\",\"method\":\"ping\",\"params\":{}}\n";
                        println!(
                            "[FakePeer {:?}] Sending fake ping request: {}",
                            behavior,
                            ping.trim()
                        );
                        let _ = peer_write.write_all(ping.as_bytes()).await;
                    }
                    PeerBehavior::SendRealRoots => {
                        let roots = "{\"jsonrpc\":\"2.0\",\"id\":\"my-roots-req-id\",\"method\":\"roots/list\",\"params\":{}}\n";
                        println!(
                            "[FakePeer {:?}] Sending roots request: {}",
                            behavior,
                            roots.trim()
                        );
                        let _ = peer_write.write_all(roots.as_bytes()).await;
                    }
                }

                // 4. Wait for client response
                line.clear();
                let expected_id = match behavior {
                    PeerBehavior::SendFakePing => "fake-ping-id",
                    PeerBehavior::SendRealRoots => "my-roots-req-id",
                };
                while let Ok(n) = reader.read_line(&mut line).await {
                    if n == 0 {
                        break;
                    }
                    println!("[FakePeer {:?}] Received line: {}", behavior, line.trim());
                    // Only a genuine roots/list response configures the sandbox.
                    // A fake ping is unrelated to sandbox setup, so it must NOT
                    // fabricate a `configured` signal: the subprocess's
                    // `notifications/sandbox/configured` is authoritative for
                    // unlocking tool calls (it means the subprocess sandbox is
                    // enforced), so emitting it for a fake ping would correctly
                    // unlock and defeat the point of this negative test.
                    if line.contains(expected_id) && matches!(behavior, PeerBehavior::SendRealRoots)
                    {
                        let configured = "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/sandbox/configured\",\"params\":{}}\n";
                        println!(
                            "[FakePeer {:?}] Sending configured notification: {}",
                            behavior,
                            configured.trim()
                        );
                        let _ = peer_write.write_all(configured.as_bytes()).await;
                    } else if line.contains("tools/call")
                        && let Ok(req) = serde_json::from_str::<Value>(&line)
                        && let Some(req_id) = req.get("id")
                    {
                        let resp = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": req_id,
                            "result": {
                                "content": [{"type": "text", "text": "Fake tool executed"}]
                            }
                        });
                        let mut resp_str = serde_json::to_string(&resp).unwrap();
                        resp_str.push('\n');
                        println!(
                            "[FakePeer {:?}] Responding to tools/call: {}",
                            behavior,
                            resp_str.trim()
                        );
                        let _ = peer_write.write_all(resp_str.as_bytes()).await;
                    }
                    line.clear();
                }
            });

            Ok(ahma_common::peer_factory::PeerStreams {
                stdin: Box::new(bridge_write),
                stdout: Box::new(bridge_read),
                stderr: None,
                shutdown_fn: None,
            })
        })
    }
}

#[tokio::test]
async fn test_response_routing_does_not_lock_sandbox_with_fake_id() {
    let factory = Arc::new(FakePeerFactory {
        behavior: PeerBehavior::SendFakePing,
    });
    let server = spawn_in_process_server(factory)
        .await
        .expect("failed to spawn server");

    let mut mcp = McpTestClient::with_url(&server.base_url()).with_transport(TransportMode::Json);

    // Initialize only
    let _init_resp = mcp
        .initialize_only("routing-test-client-fake")
        .await
        .expect("initialize_only failed");

    // Open SSE stream
    let _sse = mcp
        .open_handshake_sse(mcp.session_id().unwrap())
        .await
        .expect("open_handshake_sse failed");

    // Send initialized notification
    mcp.send_initialized()
        .await
        .expect("send_initialized failed");

    // Tool call should fail before sandbox lock
    let call_res = mcp
        .call_tool("run_terminal_command", json!({ "command": "pwd" }))
        .await;
    assert!(
        !call_res.success,
        "Tool call should fail before sandbox lock"
    );
    assert!(
        call_res.error.as_ref().unwrap().contains("-32001"),
        "Expected -32001, got: {:?}",
        call_res.error
    );

    // Send a client response payload to a fake request with result and roots.
    let client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .unwrap();
    let mcp_url = format!("{}/mcp", server.base_url());
    let temp_dir = tempfile::tempdir().unwrap();
    let temp_uri = common::encode_file_uri(temp_dir.path());
    let response_payload = json!({
        "jsonrpc": "2.0",
        "id": "fake-ping-id",
        "result": {
            "roots": [
                {
                    "uri": temp_uri,
                    "name": "fake"
                }
            ]
        }
    });

    let resp = client
        .post(&mcp_url)
        .header("Content-Type", "application/json")
        .header("Mcp-Session-Id", mcp.session_id().unwrap())
        .json(&response_payload)
        .send()
        .await
        .expect("Failed to send fake response");

    assert!(
        resp.status().is_success(),
        "Response submission should succeed"
    );

    // Tool call should STILL fail with 409 (-32001) because the fake ID should be ignored
    let call_res2 = mcp
        .call_tool("run_terminal_command", json!({ "command": "pwd" }))
        .await;
    assert!(!call_res2.success, "Tool call should still fail");
    assert!(
        call_res2.error.as_ref().unwrap().contains("-32001"),
        "Expected sandbox still locked/initializing error (-32001). Got: {:?}",
        call_res2.error
    );
}

#[tokio::test]
async fn test_response_routing_locks_sandbox_with_real_id() {
    let factory = Arc::new(FakePeerFactory {
        behavior: PeerBehavior::SendRealRoots,
    });
    let server = spawn_in_process_server(factory)
        .await
        .expect("failed to spawn server");

    let mut mcp = McpTestClient::with_url(&server.base_url()).with_transport(TransportMode::Json);

    // Initialize only
    let _init_resp = mcp
        .initialize_only("routing-test-client-real")
        .await
        .expect("initialize_only failed");

    // Open SSE stream (this will trigger the FakePeerFactory to send the roots/list request)
    let _sse = mcp
        .open_handshake_sse(mcp.session_id().unwrap())
        .await
        .expect("open_handshake_sse failed");

    // Send initialized notification
    mcp.send_initialized()
        .await
        .expect("send_initialized failed");

    // Tool call should fail before sandbox lock
    let call_res = mcp
        .call_tool("run_terminal_command", json!({ "command": "pwd" }))
        .await;
    assert!(
        !call_res.success,
        "Tool call should fail before sandbox lock"
    );
    assert!(
        call_res.error.as_ref().unwrap().contains("-32001"),
        "Expected -32001, got: {:?}",
        call_res.error
    );

    // Send a client response payload to the real roots/list request ID
    let client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .unwrap();
    let mcp_url = format!("{}/mcp", server.base_url());
    let temp_dir = tempfile::tempdir().unwrap();
    let temp_uri = common::encode_file_uri(temp_dir.path());
    let response_payload = json!({
        "jsonrpc": "2.0",
        "id": "my-roots-req-id",
        "result": {
            "roots": [
                {
                    "uri": temp_uri,
                    "name": "fake"
                }
            ]
        }
    });

    let resp = client
        .post(&mcp_url)
        .header("Content-Type", "application/json")
        .header("Mcp-Session-Id", mcp.session_id().unwrap())
        .json(&response_payload)
        .send()
        .await
        .expect("Failed to send real response");

    assert!(
        resp.status().is_success(),
        "Response submission should succeed"
    );

    // Give the peer/bridge a moment to process the configured notification
    sleep(Duration::from_millis(200)).await;

    // Tool call should NOT return 409 (-32001) anymore, because the sandbox is now configured/Active!
    let call_res2 = mcp
        .call_tool("run_terminal_command", json!({ "command": "pwd" }))
        .await;
    if call_res2.success {
        // success
    } else {
        let err = call_res2.error.as_ref().unwrap();
        assert!(
            !err.contains("-32001"),
            "Expected sandbox to be unlocked, but got -32001 conflict: {}",
            err
        );
    }
}
