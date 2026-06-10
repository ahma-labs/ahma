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

/// Run the stdio proxy connecting to the running UDS or HTTP server.
pub async fn run_proxy_client(uds_path: Option<&str>, http_url: Option<&str>) -> Result<()> {
    #[cfg(unix)]
    if let Some(path) = uds_path {
        tracing::info!(socket = path, "Proxying stdio to Unix Domain Socket");
        return run_proxy_client_unix(path).await;
    }

    if let Some(url) = http_url {
        tracing::info!(url = url, "Proxying stdio to HTTP server");
        return run_proxy_client_http(url).await;
    }

    #[cfg(not(unix))]
    let _ = uds_path;

    Err(anyhow!("No socket or HTTP URL provided for proxy client"))
}

#[cfg(unix)]
async fn run_proxy_client_unix(socket_path: &str) -> Result<()> {
    use ahma_http_mcp_client::unix_client::unix_socket_transport;

    let client_transport = unix_socket_transport(socket_path, "http://localhost/mcp")
        .with_context(|| format!("Failed to connect proxy to UDS {socket_path}"))?;
    let stdio_transport = PatchedStdioTransport::new_stdio();

    tracing::info!(socket = socket_path, "Proxy connected to bridge via UDS");
    let result = run_transport_proxy(stdio_transport, client_transport, "unix").await;
    if let Err(ref e) = result {
        tracing::error!(socket = socket_path, error = %e, "Proxy session ended with error");
    }
    result
}

#[cfg(unix)]
async fn run_transport_proxy<S, C>(mut stdio: S, mut client: C, transport: &str) -> Result<()>
where
    S: Transport<RoleServer> + Send + 'static,
    C: Transport<RoleClient> + Send + 'static,
    S::Error: std::fmt::Debug + Send,
    C::Error: std::fmt::Debug + Send,
{
    loop {
        tokio::select! {
            stdio_msg = stdio.receive() => {
                let Some(msg) = stdio_msg else {
                    tracing::info!(transport, "Proxy exiting: stdio EOF (Cursor client disconnected)");
                    break;
                };
                let val = serde_json::to_value(msg).unwrap();
                let tx_msg = serde_json::from_value(val).unwrap();
                if let Err(e) = client.send(tx_msg).await {
                    tracing::error!(
                        transport,
                        error = ?e,
                        "Proxy exiting: failed to forward message to bridge"
                    );
                    break;
                }
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
            }
        }
    }
    Ok(())
}

async fn run_proxy_client_http(base_url: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .context("Failed to build HTTP client for stdio proxy")?;

    let mcp_url = format!("{}/mcp", base_url.trim_end_matches('/'));

    let mut stdio = PatchedStdioTransport::new_stdio();

    // 1. Handshake / Initialize
    let init_msg = stdio.receive().await.ok_or_else(|| {
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

    let resp_bytes = response.bytes().await.context("Failed to read initialize response body")?;
    let resp_msg: TxJsonRpcMessage<RoleServer> =
        serde_json::from_slice(&resp_bytes).context("Failed to parse initialize response JSON")?;
    stdio
        .send(resp_msg)
        .await
        .context("Failed to forward initialize response to stdio")?;

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

    Ok(())
}
