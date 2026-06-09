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

            let manifest = ClusterManifest {
                task_id: task_id.clone(),
                tool_name: tool_name.clone(),
                scope: None,
                nonce: String::new(),
                issued_at: 0,
                signature: String::new(),
            }
            .sign(&shared_key);

            let manifest_header_value = manifest
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
                &manifest_header_value,
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

            let mut notif_req = client
                .post(&mcp_url)
                .json(&notif_body)
                .header(manifest_header.clone(), manifest_header_value.clone());
            if let Some(ref sid) = session_id {
                notif_req = notif_req.header("mcp-session-id", sid.as_str());
            }
            // notifications/initialized returns 202 (no body)
            let _ = notif_req.send().await;

            // ── Step 3: tools/call ────────────────────────────────────────────
            debug!(peer = %peer_addr, tool = %tool_name, "McpPeerDispatch: calling tool");
            let mut call_req = client
                .post(&mcp_url)
                .json(&payload)
                .header(manifest_header.clone(), manifest_header_value.clone());
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
                    .header(manifest_header.clone(), manifest_header_value.clone())
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
}
