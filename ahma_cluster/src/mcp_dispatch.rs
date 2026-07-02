//! MCP-based cluster peer dispatch (P3).
//!
//! [`McpPeerDispatch`] implements [`PeerDispatch`] by calling the remote peer's
//! MCP endpoint (`POST /mcp`) directly, passing the HMAC-signed
//! [`ClusterManifest`] in the `X-Ahma-Cluster-Manifest` request header.
//!
//! # Why not use `ahma_http_mcp_client`?
//!
//! `ahma_http_mcp_client` is a full MCP *client* that manages OAuth2 and a
//! persistent session.  For cluster dispatch, we need a simpler, stateless
//! POST-per-request model:
//!
//! 1. The manifest already authenticates the call — no bearer-token OAuth flow.
//! 2. Each task dispatch is a one-shot `tools/call` wrapped in a mini MCP
//!    session (initialize → tools/call → session close).
//!
//! # Protocol flow
//!
//! ```text
//! Dispatcher                          Bridge peer
//! ──────────                          ──────────────
//! POST /mcp (initialize)           →  creates session, returns session_id
//! POST /mcp (tools/call)  ─────────→  executes tool, returns result
//! DELETE /mcp (close)              →  terminates session
//! ```
//!
//! Every request carries `X-Ahma-Cluster-Manifest` so the bridge can verify
//! authorship without a separate credentials exchange.
//!
//! [`PeerDispatch`]: ahma_common::peer_transport::PeerDispatch
//! [`ClusterManifest`]: ahma_http_bridge::cluster_auth::ClusterManifest

use ahma_common::{
    config::TransportMode,
    peer_transport::{BoxFuture, PeerDispatch},
};
use ahma_http_bridge::cluster_auth::{CLUSTER_MANIFEST_HEADER, ClusterManifest};
use anyhow::{Context, Result};
use reqwest::{Client, header::HeaderName};
use serde_json::Value;
use std::{str::FromStr, sync::Arc};
use tracing::{debug, warn};

/// `PeerDispatch` that communicates with the remote peer's bridge using the
/// standard MCP `tools/call` protocol authenticated by HMAC manifest.
///
/// This is the P3 replacement for the legacy bespoke `POST /tasks` approach.
#[derive(Clone)]
pub struct McpPeerDispatch {
    /// Shared HMAC key — must match the peer bridge's `cluster_shared_key`.
    shared_key: Vec<u8>,
    /// HTTP client used for all requests.
    client: Client,
    /// Transport preference order (same semantics as `ClusterTransport`).
    preference: Vec<TransportMode>,
}

impl McpPeerDispatch {
    /// Construct a new dispatcher.
    ///
    /// * `shared_key` — HMAC-SHA256 key shared with all cluster peers.
    /// * `preference` — transport preference order.
    /// * `ca_pem` — optional PEM CA certificate for QUIC TLS.
    pub fn new(
        shared_key: impl Into<Vec<u8>>,
        preference: Vec<TransportMode>,
        ca_pem: Option<&str>,
    ) -> Self {
        use std::time::Duration;
        let timeout = Duration::from_secs(120);

        let mut b = Client::builder().timeout(timeout);
        if let Some(pem) = ca_pem {
            if let Ok(cert) = reqwest::tls::Certificate::from_pem(pem.as_bytes()) {
                b = b.add_root_certificate(cert);
            } else {
                warn!("McpPeerDispatch: could not parse CA PEM; skipping cert trust");
            }
        }
        let client = b.build().unwrap_or_default();

        Self {
            shared_key: shared_key.into(),
            client,
            preference,
        }
    }

    /// Wrap in `Arc<dyn PeerDispatch>` for injection into [`ClusterScheduler`].
    ///
    /// [`ClusterScheduler`]: crate::scheduler::ClusterScheduler
    pub fn into_arc_dispatch(self) -> Arc<dyn PeerDispatch> {
        Arc::new(self)
    }
}

impl std::fmt::Debug for McpPeerDispatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpPeerDispatch")
            .field("preference", &self.preference)
            .finish_non_exhaustive()
    }
}

impl PeerDispatch for McpPeerDispatch {
    fn dispatch(&self, peer_addr: &str, _path: &str, payload: Value) -> BoxFuture<Result<Value>> {
        let shared_key = self.shared_key.clone();
        let client = self.client.clone();
        let peer_addr = peer_addr.to_string();
        let preference = self.preference.clone();

        Box::pin(async move {
            // Extract tool_name from the payload for the manifest.
            let tool_name = payload
                .get("params")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("unknown")
                .to_string();

            let task_id = uuid::Uuid::new_v4().to_string();

            // The handshake steps (initialize / notifications/initialized /
            // session close) carry no attacker-influenced payload, so a
            // session-scoped manifest without a body_hash authenticates them.
            let session_manifest = ClusterManifest {
                task_id: task_id.clone(),
                tool_name: tool_name.clone(),
                scope: None,
                nonce: String::new(),
                issued_at: 0,
                body_hash: None,
                signature: String::new(),
            }
            .sign(&shared_key);

            let session_manifest_header_value = session_manifest
                .to_header_value()
                .context("Failed to encode cluster manifest as header")?;

            // The `tools/call` step carries the actual tool name *and*
            // arguments the bridge will execute, so its manifest must be
            // bound to the exact request body — a signature over `tool_name`
            // alone would let a tampered body ride under a validly-signed
            // manifest. Serialize once and reuse the same bytes for both the
            // hash and the request body, so the bridge hashes exactly what
            // was signed (no re-serialization that could produce different
            // bytes).
            let call_body_bytes =
                serde_json::to_vec(&payload).context("Failed to serialize tools/call payload")?;
            let call_manifest = ClusterManifest {
                task_id: task_id.clone(),
                tool_name: tool_name.clone(),
                scope: None,
                nonce: String::new(),
                issued_at: 0,
                body_hash: Some(ClusterManifest::hash_body(&call_body_bytes)),
                signature: String::new(),
            }
            .sign(&shared_key);

            let call_manifest_header_value = call_manifest
                .to_header_value()
                .context("Failed to encode cluster manifest as header")?;

            let manifest_header = HeaderName::from_str(CLUSTER_MANIFEST_HEADER)
                .context("Invalid cluster manifest header name")?;

            let mcp_url = format!("{}/mcp", peer_addr.trim_end_matches('/'));

            // ── Step 1: initialize ────────────────────────────────────────────
            let init_body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "clientInfo": {
                        "name": "ahma-cluster-dispatch",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": {}
                }
            });

            debug!(peer = %peer_addr, tool = %tool_name, task_id = %task_id, "McpPeerDispatch: initializing session");
            let init_resp = post_with_manifest(
                &client,
                &preference,
                &mcp_url,
                &init_body,
                &manifest_header,
                &session_manifest_header_value,
            )
            .await
            .context("McpPeerDispatch: initialize failed")?;

            let session_id = init_resp
                .headers()
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());

            // Parse initialize response to confirm it succeeded.
            let _init_json: Value = init_resp
                .json()
                .await
                .context("McpPeerDispatch: failed to parse initialize response")?;

            // ── Step 2: notifications/initialized ─────────────────────────────
            let notif_body = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
                "params": {}
            });

            let mut notif_req = client.post(&mcp_url).json(&notif_body).header(
                manifest_header.clone(),
                session_manifest_header_value.clone(),
            );
            if let Some(ref sid) = session_id {
                notif_req = notif_req.header("mcp-session-id", sid.as_str());
            }
            // notifications/initialized returns 202 (no body)
            let _ = notif_req.send().await;

            // ── Step 3: tools/call ────────────────────────────────────────────
            debug!(peer = %peer_addr, tool = %tool_name, "McpPeerDispatch: calling tool");
            let mut call_req = client
                .post(&mcp_url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(call_body_bytes)
                .header(manifest_header.clone(), call_manifest_header_value.clone());
            if let Some(ref sid) = session_id {
                call_req = call_req.header("mcp-session-id", sid.as_str());
            }
            let call_resp = call_req
                .send()
                .await
                .context("McpPeerDispatch: tools/call failed")?;

            let result: Value = call_resp
                .json()
                .await
                .context("McpPeerDispatch: failed to parse tools/call response")?;

            // ── Step 4: cleanup — DELETE session ─────────────────────────────
            if let Some(ref sid) = session_id {
                let _ = client
                    .delete(&mcp_url)
                    .header("mcp-session-id", sid.as_str())
                    .header(
                        manifest_header.clone(),
                        session_manifest_header_value.clone(),
                    )
                    .send()
                    .await;
            }

            Ok(result)
        })
    }
}

/// POST `body` to `url` with the cluster manifest header, trying each transport
/// in `preference` order.
async fn post_with_manifest(
    client: &Client,
    preference: &[TransportMode],
    url: &str,
    body: &Value,
    manifest_header: &HeaderName,
    manifest_value: &str,
) -> Result<reqwest::Response> {
    let https_url = url
        .strip_prefix("http://")
        .map(|rest| format!("https://{rest}"))
        .unwrap_or_else(|| url.to_string());

    for mode in preference {
        let target_url = match mode {
            TransportMode::Http1 | TransportMode::Http2 => url,
            TransportMode::Quic => https_url.as_str(),
        };
        let req = client
            .post(target_url)
            .json(body)
            .header(manifest_header.clone(), manifest_value);
        match req.send().await {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                warn!(transport = ?mode, url = target_url, "McpPeerDispatch post failed: {e}")
            }
        }
    }

    // Fallback to plain HTTP/1.1
    client
        .post(url)
        .json(body)
        .header(manifest_header.clone(), manifest_value)
        .send()
        .await
        .with_context(|| format!("McpPeerDispatch: all transports failed for {url}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mcp_dispatch_debug_does_not_expose_key() {
        let d = McpPeerDispatch::new(b"secret-key".to_vec(), vec![], None);
        let s = format!("{d:?}");
        assert!(
            !s.contains("secret-key"),
            "key must not appear in Debug output"
        );
        assert!(s.contains("McpPeerDispatch"), "should include struct name");
    }

    #[test]
    fn mcp_dispatch_into_arc() {
        let d = McpPeerDispatch::new(b"key".to_vec(), vec![], None);
        let _arc: Arc<dyn PeerDispatch> = d.into_arc_dispatch();
    }

    // ── McpPeerDispatch::new — CA PEM handling ─────────────────────────────

    #[test]
    fn new_with_invalid_ca_pem_does_not_panic_and_skips_trust() {
        // Garbage PEM must hit the `else` (parse-failure) branch of
        // `McpPeerDispatch::new`, log a warning, and still produce a usable
        // dispatcher rather than panicking or failing construction.
        let d = McpPeerDispatch::new(b"key".to_vec(), vec![], Some("not a valid pem at all"));
        let s = format!("{d:?}");
        assert!(s.contains("McpPeerDispatch"));
    }

    #[test]
    fn new_with_valid_ca_pem_configures_trust_and_debug_stays_key_free() {
        // A real, syntactically valid self-signed cert exercises the
        // `Ok(cert) => add_root_certificate` branch.
        let rcgen::CertifiedKey { cert, .. } =
            rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
                .expect("self-signed cert generation must succeed");
        let pem = cert.pem();

        let d = McpPeerDispatch::new(b"pem-test-key".to_vec(), vec![], Some(&pem));
        let s = format!("{d:?}");
        assert!(s.contains("McpPeerDispatch"));
        assert!(!s.contains("pem-test-key"), "key must not leak via Debug");
    }

    // ── dispatch() happy path ───────────────────────────────────────────────

    #[tokio::test]
    async fn dispatch_happy_path_sends_signed_manifest_and_binds_body_hash() {
        use std::sync::Mutex;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let captured: Arc<Mutex<Vec<wiremock::Request>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();

        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(move |req: &wiremock::Request| {
                captured_clone.lock().unwrap().push(req.clone());
                let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
                match body.get("method").and_then(|v| v.as_str()) {
                    Some("initialize") => ResponseTemplate::new(200)
                        .insert_header("mcp-session-id", "sess-abc123")
                        .set_body_json(json!({
                            "jsonrpc": "2.0", "id": 1,
                            "result": {"protocolVersion": "2025-03-26"}
                        })),
                    Some("notifications/initialized") => ResponseTemplate::new(202),
                    Some("tools/call") => ResponseTemplate::new(200).set_body_json(json!({
                        "jsonrpc": "2.0", "id": 1,
                        "result": {"content": [{"type": "text", "text": "42"}]}
                    })),
                    _ => ResponseTemplate::new(200).set_body_json(json!({})),
                }
            })
            .mount(&server)
            .await;

        Mock::given(method("DELETE"))
            .and(path("/mcp"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let shared_key = b"integration-test-key".to_vec();
        let dispatcher = McpPeerDispatch::new(shared_key.clone(), vec![TransportMode::Http1], None);

        let payload = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "echo_tool", "arguments": {"value": "hi"}}
        });

        let result = dispatcher
            .dispatch(&server.uri(), "/mcp", payload.clone())
            .await
            .expect("dispatch should succeed against a well-behaved mock peer");

        assert_eq!(
            result["result"]["content"][0]["text"].as_str(),
            Some("42"),
            "dispatch must return the tools/call result body: {result}"
        );

        let requests = captured.lock().unwrap();
        let expected_body_bytes = serde_json::to_vec(&payload).unwrap();
        let call_req = requests
            .iter()
            .find(|r| {
                serde_json::from_slice::<Value>(&r.body)
                    .ok()
                    .and_then(|b| b.get("method").and_then(|m| m.as_str().map(str::to_string)))
                    == Some("tools/call".to_string())
            })
            .expect("tools/call request must have been sent");

        // The wire body must be byte-identical to what dispatch() serialized
        // once and reused for both hashing and sending.
        assert_eq!(call_req.body, expected_body_bytes);

        let manifest_value = call_req
            .headers
            .get(CLUSTER_MANIFEST_HEADER)
            .expect("tools/call request must carry the cluster manifest header")
            .to_str()
            .unwrap();
        let manifest = ClusterManifest::from_header_value(manifest_value)
            .expect("manifest header must decode");

        manifest
            .verify(&shared_key)
            .expect("manifest signature must verify with the shared key");
        manifest
            .verify_body(&call_req.body)
            .expect("body_hash must match the exact bytes sent on the wire");
        assert_eq!(manifest.tool_name, "echo_tool");
    }

    // ── dispatch() — proves the session/call manifest split closes the gap ──

    #[tokio::test]
    async fn dispatch_session_manifest_cannot_authenticate_the_tools_call_body() {
        use std::sync::Mutex;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let captured: Arc<Mutex<Vec<wiremock::Request>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();

        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(move |req: &wiremock::Request| {
                captured_clone.lock().unwrap().push(req.clone());
                let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
                match body.get("method").and_then(|v| v.as_str()) {
                    Some("initialize") => ResponseTemplate::new(200)
                        .insert_header("mcp-session-id", "sess-split")
                        .set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": {}})),
                    Some("notifications/initialized") => ResponseTemplate::new(202),
                    Some("tools/call") => ResponseTemplate::new(200)
                        .set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}})),
                    _ => ResponseTemplate::new(200).set_body_json(json!({})),
                }
            })
            .mount(&server)
            .await;

        Mock::given(method("DELETE"))
            .and(path("/mcp"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let shared_key = b"split-test-key".to_vec();
        let dispatcher = McpPeerDispatch::new(shared_key.clone(), vec![], None);
        let payload = json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "danger_tool", "arguments": {"cmd": "rm -rf /"}}
        });

        dispatcher
            .dispatch(&server.uri(), "/mcp", payload)
            .await
            .expect("dispatch should succeed");

        let requests = captured.lock().unwrap();
        let get_manifest = |m: &str| -> (ClusterManifest, Vec<u8>) {
            let req = requests
                .iter()
                .find(|r| {
                    serde_json::from_slice::<Value>(&r.body)
                        .ok()
                        .and_then(|b| b.get("method").and_then(|x| x.as_str().map(str::to_string)))
                        == Some(m.to_string())
                })
                .unwrap_or_else(|| panic!("no request for method {m}"));
            let header = req
                .headers
                .get(CLUSTER_MANIFEST_HEADER)
                .unwrap()
                .to_str()
                .unwrap();
            (
                ClusterManifest::from_header_value(header).unwrap(),
                req.body.clone(),
            )
        };

        let (session_manifest, _) = get_manifest("initialize");
        let (call_manifest, call_body) = get_manifest("tools/call");

        assert!(
            session_manifest.body_hash.is_none(),
            "handshake steps must use a session-scoped manifest with no body_hash"
        );
        assert!(
            call_manifest.body_hash.is_some(),
            "tools/call must use a body-bound manifest"
        );
        assert_eq!(
            session_manifest.task_id, call_manifest.task_id,
            "both manifests correlate to the same dispatched task"
        );
        assert_ne!(
            session_manifest.signature, call_manifest.signature,
            "the two manifests must be signed independently"
        );

        // The exact vulnerability this split closes: a manifest with no
        // body_hash (valid for the handshake) must never authenticate an
        // arbitrary request body.
        let err = session_manifest
            .verify_body(&call_body)
            .expect_err("a session manifest without body_hash must not authenticate any body");
        assert!(
            err.to_string().contains("body_hash"),
            "error must explain the missing body_hash: {err}"
        );

        // The call manifest, by contrast, is genuinely bound to those exact
        // bytes and rejects anything else.
        call_manifest
            .verify_body(&call_body)
            .expect("call manifest must authenticate the exact body that was sent");
        assert!(
            call_manifest.verify_body(b"{\"tampered\":true}").is_err(),
            "call manifest must reject a body it was not signed for"
        );
    }

    // ── post_with_manifest — transport preference and fallback ─────────────

    #[tokio::test]
    async fn post_with_manifest_falls_back_to_plain_http_when_preferred_transport_fails() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await; // plain HTTP, no TLS.
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .mount(&server)
            .await;

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap();
        let manifest_header = HeaderName::from_str(CLUSTER_MANIFEST_HEADER).unwrap();
        let url = format!("{}/mcp", server.uri());
        let body = json!({"jsonrpc": "2.0", "method": "initialize", "params": {}});

        // TransportMode::Quic rewrites the URL to https://, which fails
        // against a plain-HTTP mock server (TLS handshake error). The loop
        // must log and continue, then the function falls through to the
        // plain-HTTP retry rather than propagating that failure.
        let result = post_with_manifest(
            &client,
            &[TransportMode::Quic],
            &url,
            &body,
            &manifest_header,
            "dummy-manifest-value",
        )
        .await;

        let resp = result.expect("must fall back to plain HTTP after the QUIC/HTTPS attempt fails");
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn post_with_manifest_returns_error_when_all_transports_fail() {
        use tokio::net::TcpListener;

        // Bind then immediately drop, to get a port nothing is listening on.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap();
        let manifest_header = HeaderName::from_str(CLUSTER_MANIFEST_HEADER).unwrap();
        let url = format!("http://127.0.0.1:{port}/mcp");
        let body = json!({"jsonrpc": "2.0", "method": "initialize", "params": {}});

        let result = post_with_manifest(
            &client,
            &[TransportMode::Http1],
            &url,
            &body,
            &manifest_header,
            "dummy-manifest-value",
        )
        .await;

        let err = result.expect_err("no listener is bound; every transport attempt must fail");
        assert!(
            err.to_string().contains("all transports failed"),
            "error should name the exhausted-fallback path: {err}"
        );
    }

    // ── dispatch() error paths ───────────────────────────────────────────────

    #[tokio::test]
    async fn dispatch_returns_error_when_peer_is_unreachable() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let dispatcher = McpPeerDispatch::new(
            b"unreachable-key".to_vec(),
            vec![TransportMode::Http1],
            None,
        );
        let peer_addr = format!("http://127.0.0.1:{port}");
        let payload = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "any_tool", "arguments": {}}
        });

        let err = dispatcher
            .dispatch(&peer_addr, "/mcp", payload)
            .await
            .expect_err("dispatch to an unreachable peer must fail");

        assert!(
            err.to_string().contains("initialize failed"),
            "error should identify the initialize step: {err}"
        );
    }

    #[tokio::test]
    async fn dispatch_returns_error_when_tools_call_step_transport_fails() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_task = tokio::spawn(async move {
            // Step 1: initialize — respond successfully with a session id,
            // then close the connection so the client must reconnect for
            // step 2.
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let body = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nmcp-session-id: sess-fail\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            }

            // Step 2: notifications/initialized. Accept the connection, then
            // immediately stop listening — *before* replying — so the next
            // connection attempt (tools/call, step 3) is refused
            // deterministically instead of racing the listener's shutdown.
            if let Ok((mut stream, _)) = listener.accept().await {
                drop(listener);
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let response =
                    "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });

        let dispatcher = McpPeerDispatch::new(
            b"tools-call-fail-key".to_vec(),
            vec![TransportMode::Http1],
            None,
        );
        let peer_addr = format!("http://127.0.0.1:{port}");
        let payload = json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "any_tool", "arguments": {}}
        });

        let err = dispatcher
            .dispatch(&peer_addr, "/mcp", payload)
            .await
            .expect_err("tools/call must fail once the peer stops accepting connections");

        assert!(
            err.to_string().contains("tools/call failed"),
            "error should identify the tools/call step, not initialize: {err}"
        );

        let _ = server_task.await;
    }

    #[tokio::test]
    async fn dispatch_returns_error_when_tools_call_response_is_not_json() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(move |req: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
                match body.get("method").and_then(|v| v.as_str()) {
                    Some("initialize") => ResponseTemplate::new(200)
                        .insert_header("mcp-session-id", "sess-badjson")
                        .set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": {}})),
                    Some("notifications/initialized") => ResponseTemplate::new(202),
                    // tools/call: 200 OK but the body is not valid JSON.
                    Some("tools/call") => {
                        ResponseTemplate::new(200).set_body_string("this is not json")
                    }
                    _ => ResponseTemplate::new(200).set_body_json(json!({})),
                }
            })
            .mount(&server)
            .await;

        let dispatcher =
            McpPeerDispatch::new(b"badjson-key".to_vec(), vec![TransportMode::Http1], None);
        let payload = json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "any_tool", "arguments": {}}
        });

        let err = dispatcher
            .dispatch(&server.uri(), "/mcp", payload)
            .await
            .expect_err("a non-JSON tools/call response must be surfaced as an error");

        assert!(
            err.to_string()
                .contains("failed to parse tools/call response"),
            "error should identify the parse-failure step: {err}"
        );
    }
}
