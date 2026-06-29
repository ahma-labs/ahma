//! # Stdio MCP Proxy Client
//!
//! When the localhost bridge server (UDS or HTTP) is already running,
//! this module acts as a transparent proxy that forwards all stdio
//! JSON-RPC traffic to the running server.

use crate::transport_patch::PatchedStdioTransport;
use anyhow::{Context, Result, anyhow};
use futures::StreamExt;
#[cfg(unix)]
use rmcp::service::RoleClient;
use rmcp::service::{RoleServer, TxJsonRpcMessage};
use rmcp::transport::Transport;
use std::time::Duration;
use tokio::sync::mpsc;

/// How many *consecutive* forward failures the proxy tolerates before treating
/// the bridge transport as genuinely dead and exiting. A single failure (e.g. a
/// per-request timeout or a sandbox-initializing 409) is relayed to the client
/// and the session is preserved; only a sustained run of failures — meaning the
/// transport itself is broken, not one request — tears the session down.
const MAX_CONSECUTIVE_FORWARD_FAILURES: u32 = 3;

/// Resolve the frontend handshake deadline: the internal
/// `AHMA_FRONTEND_HANDSHAKE_DEADLINE_SECS` override if set (testing), otherwise
/// [`FRONTEND_HANDSHAKE_DEADLINE_SECS`]. A value of `0` disables the deadline
/// (returns `None`).
fn frontend_handshake_deadline() -> Option<Duration> {
    let secs = std::env::var("AHMA_FRONTEND_HANDSHAKE_DEADLINE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(ahma_common::timeouts::FRONTEND_HANDSHAKE_DEADLINE_SECS);
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// Run the stdio proxy connecting to the running UDS or HTTP server.
///
/// Returns `Ok(true)` when the bridge successfully responded to at least one
/// message (normal session end).  Returns `Ok(false)` or `Err` when the bridge
/// closed the connection before sending any response back to the client, which
/// typically indicates a stale or incompatible bridge daemon.
pub async fn run_proxy_client(uds_path: Option<&str>, http_url: Option<&str>) -> Result<bool> {
    let handshake_deadline = frontend_handshake_deadline();

    #[cfg(unix)]
    if let Some(path) = uds_path {
        tracing::info!(socket = path, "Proxying stdio to Unix Domain Socket");
        return run_proxy_client_unix(path, handshake_deadline).await;
    }

    if let Some(url) = http_url {
        tracing::info!(url = url, "Proxying stdio to HTTP server");
        return run_proxy_client_http(url, handshake_deadline).await;
    }

    #[cfg(not(unix))]
    let _ = uds_path;

    Err(anyhow!("No socket or HTTP URL provided for proxy client"))
}

#[cfg(unix)]
async fn run_proxy_client_unix(
    socket_path: &str,
    handshake_deadline: Option<Duration>,
) -> Result<bool> {
    use ahma_http_mcp_client::unix_client::unix_socket_transport;

    let client_transport = unix_socket_transport(socket_path, "http://localhost/mcp")
        .with_context(|| format!("Failed to connect proxy to UDS {socket_path}"))?;
    let stdio_transport = PatchedStdioTransport::new_stdio();

    tracing::info!(socket = socket_path, "Proxy connected to bridge via UDS");
    let result = run_transport_proxy(
        stdio_transport,
        client_transport,
        "unix",
        handshake_deadline,
    )
    .await;
    if let Err(ref e) = result {
        tracing::error!(socket = socket_path, error = %e, "Proxy session ended with error");
    }
    result
}

#[cfg(unix)]
async fn run_transport_proxy<S, C>(
    mut stdio: S,
    mut client: C,
    transport: &str,
    handshake_deadline: Option<Duration>,
) -> Result<bool>
where
    S: Transport<RoleServer> + Send + 'static,
    C: Transport<RoleClient> + Send + 'static,
    S::Error: std::fmt::Debug + Send,
    C::Error: std::fmt::Debug + Send,
{
    // Track whether we ever forwarded a message to the bridge.  Until the client sends
    // `initialize`, the bridge never creates a session for this proxy (and the underlying
    // rmcp worker is parked awaiting the first message without observing its cancellation
    // token).  In that state `client.close()` would block forever, so we must only attempt
    // the teardown when a session could actually exist.
    let mut forwarded_any = false;
    // Track whether the bridge ever responded (sent a message back to the client).
    // This is the signal used by handle_version_checks to detect a stale bridge:
    // a healthy bridge always replies to `initialize`; a stale one closes silently.
    let mut bridge_responded = false;
    // Count consecutive failures to forward a request to the bridge. Reset on any
    // success. A single failure no longer tears the session down (see
    // MAX_CONSECUTIVE_FORWARD_FAILURES).
    let mut consecutive_forward_failures: u32 = 0;

    // Handshake deadline: if the client never sends its first message (the
    // `initialize` handshake) within this window, the connection was spawned
    // and abandoned — exit so abandoned `serve stdio` spawns cannot accumulate.
    // Disarmed once the first message is forwarded; a live idle session is never
    // killed by this. A far-future sleep stands in for "no deadline".
    let deadline = handshake_deadline.unwrap_or(Duration::from_secs(u64::MAX / 2));
    let handshake_timer = tokio::time::sleep(deadline);
    tokio::pin!(handshake_timer);

    loop {
        tokio::select! {
            _ = &mut handshake_timer, if !forwarded_any => {
                tracing::warn!(
                    transport,
                    ?deadline,
                    "Proxy exiting: no MCP handshake within deadline (connection spawned but abandoned)"
                );
                // Exit the process directly rather than returning up the stack:
                // the stdin reader thread (tokio::io::stdin) is still blocked in a
                // read() on the held-open pipe, so a normal return would hang on
                // runtime shutdown waiting for that thread. The frontend proxy
                // holds no state worth draining.
                std::process::exit(0);
            }
            stdio_msg = stdio.receive() => {
                let Some(msg) = stdio_msg else {
                    tracing::info!(transport, "Proxy exiting: stdio EOF (Cursor client disconnected)");
                    break;
                };
                let val = serde_json::to_value(msg).unwrap();
                let request_id = val.get("id").filter(|id| !id.is_null()).cloned();
                let tx_msg = serde_json::from_value(val).unwrap();
                if let Err(e) = client.send(tx_msg).await {
                    // A single forward failure must NOT tear down the whole
                    // multiplexed session. The bridge returns recoverable
                    // conditions (per-request timeout, sandbox-initializing 409)
                    // as ordinary responses now, but as defense-in-depth we also
                    // refuse to die on one transport-level send error: relay an
                    // error for THIS request id back to the client and keep
                    // serving. Only a sustained run of failures (the transport is
                    // genuinely dead) exits the proxy.
                    consecutive_forward_failures += 1;
                    tracing::warn!(
                        transport,
                        error = ?e,
                        failures = consecutive_forward_failures,
                        "Failed to forward request to bridge; relaying error to client, session preserved"
                    );
                    if let Some(id) = request_id {
                        let err_val = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {
                                "code": -32002,
                                "message": "Bridge could not service this request; it may \
                                            still be running. Retry, or await the completion \
                                            notification."
                            }
                        });
                        if let Ok(err_msg) =
                            serde_json::from_value::<TxJsonRpcMessage<RoleServer>>(err_val)
                        {
                            let _ = stdio.send(err_msg).await;
                        }
                    }
                    if consecutive_forward_failures >= MAX_CONSECUTIVE_FORWARD_FAILURES {
                        tracing::error!(
                            transport,
                            failures = consecutive_forward_failures,
                            "Proxy exiting: bridge transport failed repeatedly (genuinely dead)"
                        );
                        break;
                    }
                    continue;
                }
                consecutive_forward_failures = 0;
                forwarded_any = true;
            }
            client_msg = client.receive() => {
                let Some(msg) = client_msg else {
                    tracing::info!(
                        transport,
                        "Proxy exiting: bridge connection closed"
                    );
                    break;
                };
                let val = serde_json::to_value(msg).unwrap();
                let tx_msg = serde_json::from_value(val).unwrap();
                if let Err(e) = stdio.send(tx_msg).await {
                    tracing::error!(
                        transport,
                        error = ?e,
                        "Proxy exiting: failed to forward message to stdio"
                    );
                    break;
                }
                bridge_responded = true;
            }
        }
    }
    // Notify the bridge that this session is terminating.  For HTTP-backed transports
    // this sends DELETE /mcp so the bridge decrements active_sessions immediately rather
    // than waiting for the 5-second SSE-drop grace period.
    //
    // Only attempt this when at least one message was forwarded: otherwise no session was
    // ever established and `client.close()` would block indefinitely (the rmcp worker is
    // parked before its cancellation-aware main loop).  Even then we bound the call with a
    // timeout as a backstop in case the worker is mid-handshake; the bridge's SSE-drop
    // grace period guarantees eventual cleanup regardless.
    if forwarded_any {
        match tokio::time::timeout(Duration::from_secs(6), client.close()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::debug!(transport, error = ?e, "Proxy close error (non-fatal)")
            }
            Err(_) => {
                tracing::debug!(transport, "Proxy close timed out (non-fatal)")
            }
        }
    }
    Ok(bridge_responded)
}

async fn run_proxy_client_http(
    base_url: &str,
    handshake_deadline: Option<Duration>,
) -> Result<bool> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .context("Failed to build HTTP client for stdio proxy")?;

    let mcp_url = format!("{}/mcp", base_url.trim_end_matches('/'));

    let mut stdio = PatchedStdioTransport::new_stdio();

    // 1. Handshake / Initialize — bounded by the handshake deadline so a
    // connection that is spawned and abandoned (no `initialize` ever sent)
    // exits rather than parking on stdin forever and piling up.
    let first_recv = stdio.receive();
    let init_msg = match handshake_deadline {
        Some(deadline) => match tokio::time::timeout(deadline, first_recv).await {
            Ok(msg) => msg,
            Err(_) => {
                tracing::warn!(
                    ?deadline,
                    "Proxy exiting: no MCP handshake within deadline (connection spawned but abandoned)"
                );
                // Exit directly: the stdin reader thread is still blocked on the
                // held-open pipe, so returning would hang on runtime shutdown.
                std::process::exit(0);
            }
        },
        None => first_recv.await,
    }
    .ok_or_else(|| {
        tracing::error!("Proxy HTTP handshake failed: no initialize message on stdin");
        anyhow!("No initialize message on stdin")
    })?;
    let init_val = serde_json::to_value(&init_msg)?;

    let response = client
        .post(&mcp_url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&init_val)
        .send()
        .await
        .with_context(|| format!("Proxy HTTP initialize POST failed for {mcp_url}"))?;

    if !response.status().is_success() {
        let status = response.status();
        tracing::error!(
            url = %mcp_url,
            status = %status,
            "Proxy HTTP initialize returned non-success status"
        );
        return Err(anyhow!("Initialize failed with HTTP {status}"));
    }

    let session_id = response
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            tracing::error!(
                url = %mcp_url,
                "Proxy HTTP initialize missing mcp-session-id header"
            );
            anyhow!("Missing mcp-session-id header in initialize response")
        })?
        .to_string();

    let resp_bytes = response
        .bytes()
        .await
        .context("Failed to read initialize response body")?;
    let resp_msg: TxJsonRpcMessage<RoleServer> =
        serde_json::from_slice(&resp_bytes).context("Failed to parse initialize response JSON")?;
    stdio
        .send(resp_msg)
        .await
        .context("Failed to forward initialize response to stdio")?;

    // The bridge successfully responded to initialize — it is alive and communicating.
    let bridge_responded = true;

    tracing::info!(
        url = %mcp_url,
        session_id = %session_id,
        "Proxy connected to bridge via HTTP"
    );

    // 2. Start SSE listener in background
    let (sse_tx, mut sse_rx) = mpsc::channel::<TxJsonRpcMessage<RoleServer>>(100);
    let sse_client = client.clone();
    let sse_url = mcp_url.clone();
    let sse_session_id = session_id.clone();

    tokio::spawn(async move {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "mcp-session-id",
            reqwest::header::HeaderValue::from_str(&sse_session_id).unwrap(),
        );
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("text/event-stream"),
        );

        let res = match sse_client.get(&sse_url).headers(headers).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(url = %sse_url, error = %e, "Proxy SSE connection failed");
                return;
            }
        };

        if !res.status().is_success() {
            tracing::error!(
                url = %sse_url,
                status = %res.status(),
                "Proxy SSE stream returned non-success status"
            );
            return;
        }

        let mut stream = res.bytes_stream();
        let mut buffer = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(url = %sse_url, error = %e, "Proxy SSE stream error");
                    break;
                }
            };

            let text = String::from_utf8_lossy(&chunk);
            buffer.push_str(&text);

            while let Some(pos) = buffer.find('\n') {
                let line = buffer.drain(..=pos).collect::<String>();
                let line_trimmed = line.trim();
                if let Some(data) = line_trimmed.strip_prefix("data:") {
                    let data = data.trim();
                    if !data.is_empty()
                        && let Ok(msg) = serde_json::from_str::<TxJsonRpcMessage<RoleServer>>(data)
                        && sse_tx.send(msg).await.is_err()
                    {
                        tracing::info!(url = %sse_url, "Proxy SSE forward channel closed");
                        break;
                    }
                }
            }
        }
        tracing::info!(url = %sse_url, "Proxy SSE stream ended");
    });

    // 3. Stdio loop
    loop {
        tokio::select! {
            stdio_msg = stdio.receive() => {
                let Some(msg) = stdio_msg else {
                    tracing::info!(
                        url = %mcp_url,
                        session_id = %session_id,
                        "Proxy exiting: stdio EOF (Cursor client disconnected)"
                    );
                    break;
                };

                let val = serde_json::to_value(&msg)?;
                let has_id = val.get("id").is_some();
                let is_request = val.get("method").is_some();

                let mut req = client.post(&mcp_url)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .header("mcp-session-id", &session_id)
                    .json(&val);

                if has_id && is_request {
                    req = req.header(reqwest::header::ACCEPT, "application/json");
                }

                let resp = req.send().await.with_context(|| {
                    format!("Proxy HTTP POST to {mcp_url} failed (session={session_id})")
                })?;
                if has_id && is_request {
                    if !resp.status().is_success() {
                        tracing::warn!(
                            url = %mcp_url,
                            status = %resp.status(),
                            "Proxy HTTP tool/request returned non-success status"
                        );
                    }
                    let bytes = resp.bytes().await?;
                    if !bytes.is_empty() {
                        let resp_msg: TxJsonRpcMessage<RoleServer> = serde_json::from_slice(&bytes)?;
                        stdio.send(resp_msg).await?;
                    }
                }
            }

            sse_msg = sse_rx.recv() => {
                let Some(msg) = sse_msg else {
                    tracing::info!(
                        url = %mcp_url,
                        session_id = %session_id,
                        "Proxy exiting: SSE channel closed"
                    );
                    break;
                };
                stdio.send(msg).await?;
            }
        }
    }

    let _ = client
        .delete(&mcp_url)
        .header("mcp-session-id", &session_id)
        .send()
        .await;

    Ok(bridge_responded)
}

// `run_transport_proxy` and the `RoleClient` import are Unix-only, so these
// tests (which drive it directly with mock transports) are gated to match.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use rmcp::service::{RoleClient, RxJsonRpcMessage};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn client_request(id: i64) -> RxJsonRpcMessage<RoleServer> {
        serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": "run_terminal_command", "arguments": {}}
        }))
        .expect("valid client request")
    }

    /// Stdio side: hands the proxy a fixed queue of client requests, then EOF
    /// (`None`), and records every server→client message the proxy sends back.
    struct MockStdio {
        inbound: VecDeque<RxJsonRpcMessage<RoleServer>>,
        sent: Arc<Mutex<Vec<serde_json::Value>>>,
    }
    impl Transport<RoleServer> for MockStdio {
        type Error = std::io::Error;
        // The trait requires `send` to return a `'static` future, so it cannot
        // borrow `&mut self`; clone the shared recorder into an owned future.
        fn send(
            &mut self,
            item: TxJsonRpcMessage<RoleServer>,
        ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
            let sent = self.sent.clone();
            async move {
                sent.lock()
                    .unwrap()
                    .push(serde_json::to_value(item).unwrap());
                Ok(())
            }
        }
        async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
            self.inbound.pop_front()
        }
        async fn close(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    /// Bridge side: fails the first `fail_first_n` sends, then succeeds. Never
    /// delivers a bridge→client message (so the stdio side drives the loop).
    struct MockClient {
        fail_first_n: usize,
        attempts: Arc<AtomicUsize>,
    }
    impl Transport<RoleClient> for MockClient {
        type Error = std::io::Error;
        fn send(
            &mut self,
            _item: TxJsonRpcMessage<RoleClient>,
        ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
            let attempts = self.attempts.clone();
            let fail_first_n = self.fail_first_n;
            async move {
                let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                if n <= fail_first_n {
                    Err(std::io::Error::other("simulated bridge forward failure"))
                } else {
                    Ok(())
                }
            }
        }
        async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
            std::future::pending().await
        }
        async fn close(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn single_forward_failure_relays_error_and_keeps_session_alive() {
        // REGRESSION: one failed forward (e.g. a per-request timeout surfaced as a
        // transport error) must NOT tear the proxy down. The proxy relays a
        // JSON-RPC error for that request id and keeps serving the next request.
        let sent = Arc::new(Mutex::new(Vec::new()));
        let attempts = Arc::new(AtomicUsize::new(0));
        let stdio = MockStdio {
            inbound: VecDeque::from(vec![client_request(1), client_request(2)]),
            sent: sent.clone(),
        };
        let client = MockClient {
            fail_first_n: 1,
            attempts: attempts.clone(),
        };

        let result = run_transport_proxy(stdio, client, "test", None).await;
        assert!(
            result.is_ok(),
            "proxy must survive a single forward failure"
        );
        // Both requests were attempted → the session survived the first failure.
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "expected one relayed error, got {sent:?}");
        assert_eq!(sent[0]["id"], serde_json::json!(1));
        assert_eq!(sent[0]["error"]["code"], serde_json::json!(-32002));
    }

    #[tokio::test]
    async fn sustained_forward_failures_tear_down_the_session() {
        // A genuinely dead transport still exits — but only after a sustained run
        // of failures, not on the first one.
        let sent = Arc::new(Mutex::new(Vec::new()));
        let attempts = Arc::new(AtomicUsize::new(0));
        let stdio = MockStdio {
            inbound: VecDeque::from(vec![
                client_request(1),
                client_request(2),
                client_request(3),
                client_request(4),
            ]),
            sent: sent.clone(),
        };
        let client = MockClient {
            fail_first_n: usize::MAX,
            attempts: attempts.clone(),
        };

        let result = run_transport_proxy(stdio, client, "test", None).await;
        assert!(result.is_ok());
        // Gives up after MAX_CONSECUTIVE_FORWARD_FAILURES; request 4 is never tried.
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            MAX_CONSECUTIVE_FORWARD_FAILURES as usize
        );
        assert_eq!(
            sent.lock().unwrap().len(),
            MAX_CONSECUTIVE_FORWARD_FAILURES as usize
        );
    }
}
