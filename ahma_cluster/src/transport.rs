//! Multi-transport dispatch for ahma cluster peer-to-peer communication.
//!
//! [`ClusterTransport`] wraps multiple reqwest clients (HTTP/1.1, HTTP/2, and
//! optionally HTTP/3 / QUIC) and dispatches outbound peer requests using the
//! configured preference order.  On any connection error the next transport is
//! tried automatically.
//!
//! # Transport ordering (default)
//!
//! | Priority | Mode    | Protocol      | URL scheme | Requires TLS |
//! |----------|---------|---------------|------------|--------------|
//! | 1        | QUIC    | HTTP/3 (QUIC) | https://   | self-signed  |
//! | 2        | HTTP/2  | HTTP/2 h2c    | http://    | no           |
//! | 3        | HTTP/1  | HTTP/1.1      | http://    | no           |
//!
//! QUIC requires the `cluster-quic` Cargo feature.  When that feature is not
//! present, `TransportMode::Quic` entries in the preference list are silently
//! treated as `TransportMode::Http2`.
//!
//! # Peer URLs
//!
//! Peer `addr` values are always `http://` URLs.  The transport layer derives
//! `https://` automatically for QUIC (same host + port, UDP transport).
//!
//! # HTTPS / certificate handling
//!
//! The QUIC transport connects over TLS.  If a CA PEM string is supplied via
//! [`ClusterTransport::new`], it is added as a trusted root so the peer's
//! self-signed leaf certificate is accepted.  If no CA PEM is supplied,
//! certificate verification stays **on** by default (self-signed peers fail and
//! the transport falls back to HTTP/2); verification is disabled only when the
//! caller explicitly passes `allow_insecure = true`, which is MITM-able and
//! meant only for a trusted LAN before CA distribution.
//!
//! # `PeerDispatch` implementation
//!
//! `ClusterTransport` implements [`ahma_common::peer_transport::PeerDispatch`]
//! so it can be stored as `Arc<dyn PeerDispatch>` inside [`ClusterScheduler`]
//! and swapped out for the in-memory test double from `ahma_common`.

use std::{sync::Arc, time::Duration};

use ahma_common::{
    config::TransportMode,
    peer_transport::{BoxFuture, PeerDispatch},
};
use anyhow::{Context, Result};
use reqwest::Client;
use serde::Serialize;
use serde_json::Value;
use tracing::{debug, warn};

const DEFAULT_PEER_TIMEOUT_SECS: u64 = 120;

/// Multi-transport outbound dispatcher for cluster peer communication.
///
/// Build with [`ClusterTransport::new`] and use [`ClusterTransport::post`] to
/// dispatch JSON payloads.
pub struct ClusterTransport {
    preference: Vec<TransportMode>,
    /// Plain HTTP/1.1 client (most compatible fallback).
    http1: Client,
    /// HTTP/2 prior-knowledge (h2c) client — no TLS, same `http://` URL.
    http2: Client,
    /// HTTP/3 (QUIC) client — TLS, `https://` URL.
    ///
    /// Only present when the `cluster-quic` feature is compiled in.
    #[cfg(feature = "cluster-quic")]
    quic: Client,
}

impl ClusterTransport {
    /// Build a new `ClusterTransport`.
    ///
    /// * `preference` — ordered list of transport modes to try.  Use
    ///   [`default_preference`] for the recommended default.
    /// * `ca_pem` — PEM-encoded CA certificate used to verify QUIC peers.
    /// * `allow_insecure` — when `true` **and** no `ca_pem` is supplied, QUIC
    ///   accepts *any* peer certificate. This is MITM-able on the network and is
    ///   never the default: without it, a QUIC client with no CA keeps normal
    ///   verification (self-signed peers fail and the transport falls back to
    ///   HTTP/2). Only enable it for a trusted LAN before CA distribution.
    pub fn new(preference: Vec<TransportMode>, ca_pem: Option<&str>, allow_insecure: bool) -> Self {
        let timeout = Duration::from_secs(DEFAULT_PEER_TIMEOUT_SECS);

        // HTTP/1.1 — plain, no frills.
        let http1 = Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_default();

        // HTTP/2 — h2c (HTTP/2 prior-knowledge, no TLS).
        // The bridge's axum server speaks HTTP/2 on plain TCP.
        let http2 = Client::builder()
            .timeout(timeout)
            .http2_prior_knowledge()
            .build()
            .unwrap_or_default();

        // HTTP/3 (QUIC) — TLS, `https://` URL.
        // The bridge's QUIC endpoint is on the same port as TCP, but over UDP,
        // with a self-signed TLS certificate.
        #[cfg(feature = "cluster-quic")]
        let quic = {
            let mut b = Client::builder()
                .timeout(timeout)
                .use_rustls_tls()
                .http3_prior_knowledge();
            match ca_pem {
                Some(pem) => match reqwest::tls::Certificate::from_pem(pem.as_bytes()) {
                    Ok(cert) => b = b.add_root_certificate(cert),
                    // Fail closed: a bad CA PEM keeps normal verification on
                    // (self-signed peers will fail) rather than trusting anyone.
                    Err(e) => warn!(
                        "cluster-quic: could not parse CA PEM ({e}); keeping cert \
                         verification ON — QUIC peers with untrusted certs will fail"
                    ),
                },
                None if allow_insecure => {
                    warn!(
                        "cluster-quic: INSECURE mode — accepting ANY QUIC peer certificate \
                         (no CA configured). Network traffic to peers can be MITM'd. Provide a \
                         CA PEM to secure it."
                    );
                    b = b.danger_accept_invalid_certs(true);
                }
                None => {
                    warn!(
                        "cluster-quic: no CA PEM configured; QUIC peers with self-signed certs \
                         will fail verification and the transport will fall back to HTTP/2. \
                         Provide a CA PEM, or explicitly enable insecure mode for a trusted LAN."
                    );
                }
            }
            b.build().unwrap_or_else(|e| {
                warn!("cluster-quic: failed to build QUIC client ({e}); QUIC disabled");
                Client::default()
            })
        };

        // Suppress unused-variable warnings when cluster-quic is off.
        #[cfg(not(feature = "cluster-quic"))]
        let _ = (ca_pem, allow_insecure);

        Self {
            preference,
            http1,
            http2,
            #[cfg(feature = "cluster-quic")]
            quic,
        }
    }

    /// Returns the default transport preference order: QUIC → HTTP/2 → HTTP/1.
    pub fn default_preference() -> Vec<TransportMode> {
        vec![
            TransportMode::Quic,
            TransportMode::Http2,
            TransportMode::Http1,
        ]
    }

    /// POST `payload` (serialised as JSON) to `{peer_http_addr}{path}`.
    ///
    /// Each transport in the preference list is attempted in order.  The first
    /// successful response is returned.  If all transports fail, the error from
    /// the last attempt is returned.
    ///
    /// `peer_http_addr` must use the `http://` scheme; `https://` is derived
    /// automatically for TLS-based transports (QUIC).
    pub async fn post<T: Serialize + Send + Sync>(
        &self,
        peer_http_addr: &str,
        path: &str,
        payload: &T,
    ) -> Result<reqwest::Response> {
        let base = peer_http_addr.trim_end_matches('/');
        let http_url = format!("{base}{path}");
        // Derive https:// for TLS-based transports (same host:port, UDP for QUIC).
        let https_url = if let Some(stripped) = http_url.strip_prefix("http://") {
            format!("https://{stripped}")
        } else {
            http_url.clone()
        };

        let mut last_err: Option<anyhow::Error> = None;

        for mode in &self.preference {
            let (client, url) = self.client_and_url_for(mode, &http_url, &https_url);
            debug!(transport = ?mode, url, "Attempting cluster peer dispatch");
            match client.post(url).json(payload).send().await {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    warn!(transport = ?mode, url, "Cluster peer dispatch failed: {e}");
                    last_err = Some(anyhow::Error::from(e));
                }
            }
        }

        // Preference list was empty or all modes failed — fall back to HTTP/1.1.
        if last_err.is_none() {
            debug!("Empty transport preference; using HTTP/1.1 fallback");
        }
        self.http1
            .post(&http_url)
            .json(payload)
            .send()
            .await
            .with_context(|| format!("All cluster transports failed for peer at {peer_http_addr}"))
    }

    // Returns the appropriate (client, url) pair for a given transport mode.
    #[inline]
    #[cfg_attr(not(feature = "cluster-quic"), allow(unused_variables))]
    fn client_and_url_for<'a>(
        &'a self,
        mode: &TransportMode,
        http_url: &'a str,
        https_url: &'a str,
    ) -> (&'a Client, &'a str) {
        match mode {
            TransportMode::Http1 => (&self.http1, http_url),
            TransportMode::Http2 => (&self.http2, http_url),
            TransportMode::Quic => {
                #[cfg(feature = "cluster-quic")]
                return (&self.quic, https_url);
                // Without the feature, treat QUIC as HTTP/2 (h2c).
                #[cfg(not(feature = "cluster-quic"))]
                (&self.http2, http_url)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PeerDispatch implementation
// ---------------------------------------------------------------------------

/// Wrap `ClusterTransport` as an `Arc<dyn PeerDispatch>` adapter.
///
/// The dispatch method serialises `payload` as JSON, POSTs it to
/// `{peer_addr}{path}` via the multi-transport fallback logic, then
/// deserialises the response body as a `serde_json::Value`.
impl PeerDispatch for ClusterTransport {
    fn dispatch(&self, peer_addr: &str, path: &str, payload: Value) -> BoxFuture<Result<Value>> {
        // We need ownership of peer_addr and path inside the async block.
        let peer_addr = peer_addr.to_string();
        let path = path.to_string();

        // Clone the reqwest clients so the future can be `'static`.
        let http1 = self.http1.clone();
        let http2 = self.http2.clone();
        #[cfg(feature = "cluster-quic")]
        let quic = self.quic.clone();
        let preference = self.preference.clone();

        Box::pin(async move {
            let base = peer_addr.trim_end_matches('/');
            let http_url = format!("{base}{path}");
            // https_url is only used by the QUIC branch; suppress warning when
            // the `cluster-quic` feature is not compiled in.
            #[cfg_attr(not(feature = "cluster-quic"), allow(unused_variables))]
            let https_url = if let Some(stripped) = http_url.strip_prefix("http://") {
                format!("https://{stripped}")
            } else {
                http_url.clone()
            };

            let mut last_err: Option<anyhow::Error> = None;

            for mode in &preference {
                let (client, url) = match mode {
                    TransportMode::Http1 => (&http1, http_url.as_str()),
                    TransportMode::Http2 => (&http2, http_url.as_str()),
                    TransportMode::Quic => {
                        #[cfg(feature = "cluster-quic")]
                        {
                            (&quic, https_url.as_str())
                        }
                        #[cfg(not(feature = "cluster-quic"))]
                        {
                            (&http2, http_url.as_str())
                        }
                    }
                };
                debug!(transport = ?mode, url, "ClusterTransport::dispatch attempt");
                match client.post(url).json(&payload).send().await {
                    Ok(resp) => {
                        let v: Value = resp
                            .json()
                            .await
                            .context("Failed to parse peer response as JSON")?;
                        return Ok(v);
                    }
                    Err(e) => {
                        warn!(transport = ?mode, url, "dispatch attempt failed: {e}");
                        last_err = Some(anyhow::Error::from(e));
                    }
                }
            }

            // Fallback to HTTP/1.1 if preference list was empty or all failed.
            match http1.post(&http_url).json(&payload).send().await {
                Ok(resp) => {
                    let v: Value = resp
                        .json()
                        .await
                        .context("Failed to parse fallback peer response as JSON")?;
                    Ok(v)
                }
                Err(e) => Err(last_err
                    .unwrap_or_else(|| anyhow::Error::from(e))
                    .context(format!(
                        "All cluster transports failed for peer at {peer_addr}"
                    ))),
            }
        })
    }
}

/// Convenience constructor — returns `ClusterTransport` wrapped in `Arc<dyn PeerDispatch>`.
///
/// Use this when you want to inject `ClusterTransport` into components that
/// accept `Arc<dyn PeerDispatch>` without repeating the cast at each call site.
pub fn new_cluster_dispatch(
    preference: Vec<TransportMode>,
    ca_pem: Option<&str>,
    allow_insecure: bool,
) -> Arc<dyn PeerDispatch> {
    Arc::new(ClusterTransport::new(preference, ca_pem, allow_insecure))
}

/// Returns the default cluster transport preference order: QUIC → HTTP/2 → HTTP/1.
///
/// This is a module-level function so callers (e.g. `ClusterScheduler::new`) can
/// access the default without importing `ClusterTransport` directly.
pub fn default_transport_preference() -> Vec<TransportMode> {
    ClusterTransport::default_preference()
}

// ---------------------------------------------------------------------------
// Debug impl (don't expose inner client state)
// ---------------------------------------------------------------------------

impl std::fmt::Debug for ClusterTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterTransport")
            .field("preference", &self.preference)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_preference_is_quic_first() {
        let pref = ClusterTransport::default_preference();
        assert_eq!(pref[0], TransportMode::Quic);
        assert_eq!(pref[1], TransportMode::Http2);
        assert_eq!(pref[2], TransportMode::Http1);
    }

    #[test]
    fn transport_mode_serde_roundtrip() {
        for (variant, json) in [
            (TransportMode::Quic, r#""quic""#),
            (TransportMode::Http2, r#""http2""#),
            (TransportMode::Http1, r#""http1""#),
        ] {
            let serialised = serde_json::to_string(&variant).unwrap();
            assert_eq!(serialised, json);
            let parsed: TransportMode = serde_json::from_str(json).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn transport_new_with_no_ca() {
        // Should not panic even with no CA and full preference list.
        let t = ClusterTransport::new(ClusterTransport::default_preference(), None, false);
        assert_eq!(t.preference.len(), 3);
    }

    #[test]
    fn transport_new_with_empty_preference() {
        let t = ClusterTransport::new(vec![], None, false);
        assert!(t.preference.is_empty());
    }

    #[test]
    fn https_url_derivation() {
        // Verify client_and_url_for produces the right URL for each mode.
        let t = ClusterTransport::new(ClusterTransport::default_preference(), None, false);
        let http = "http://10.0.0.5:7000/tasks";
        let https = "https://10.0.0.5:7000/tasks";

        let (_, url1) = t.client_and_url_for(&TransportMode::Http1, http, https);
        assert_eq!(url1, http);

        let (_, url2) = t.client_and_url_for(&TransportMode::Http2, http, https);
        assert_eq!(url2, http); // h2c uses http://

        let (_, urlq) = t.client_and_url_for(&TransportMode::Quic, http, https);
        // QUIC uses https:// when feature is enabled; falls back to http:// otherwise.
        #[cfg(feature = "cluster-quic")]
        assert_eq!(urlq, https);
        #[cfg(not(feature = "cluster-quic"))]
        assert_eq!(urlq, http);
    }

    #[test]
    fn transport_mode_config_default_includes_all_modes() {
        use ahma_common::config::ClusterConfig;
        let cfg = ClusterConfig::default();
        assert!(cfg.transport_preference.contains(&TransportMode::Quic));
        assert!(cfg.transport_preference.contains(&TransportMode::Http2));
        assert!(cfg.transport_preference.contains(&TransportMode::Http1));
    }

    #[tokio::test]
    async fn test_transport_post_success() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let mut buf = [0; 1024];
                let _ = stream.read(&mut buf).await;
                let response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 16\r\nConnection: close\r\n\r\n{\"task_id\":\"t1\"}\r\n";
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });

        let transport = ClusterTransport::new(vec![TransportMode::Http1], None, false);
        let resp = transport
            .post(&format!("http://127.0.0.1:{port}"), "/tasks", &"payload")
            .await
            .unwrap();

        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let text = resp.text().await.unwrap();
        assert!(text.contains("t1"));
    }
}
