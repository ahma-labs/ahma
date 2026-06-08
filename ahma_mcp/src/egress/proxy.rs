//! Lightweight HTTP/HTTPS forward proxy for per-task egress sandboxing.
//!
//! The proxy binds to an OS-assigned port on `127.0.0.1`, is passed to the
//! sandboxed subprocess via `HTTP_PROXY` / `HTTPS_PROXY`, and forwards or
//! rejects connections based on the vault's [`EgressAllowlist`].
//!
//! ## Protocol support
//!
//! - **HTTP CONNECT** (used for HTTPS tunnelling): domain extracted from the
//!   CONNECT target; allowed → TCP tunnel; blocked → `407 Proxy Authentication
//!   Required` (conventionally used to signal proxy block).
//! - **Plain HTTP** (for `http://` URLs): Host header extracted; allowed →
//!   forwarded upstream; blocked → `403 Forbidden`.
//!
//! ## Lifetime
//!
//! The proxy runs until the [`EgressProxy`] handle is dropped, at which point
//! the listener task is aborted.  This ties the proxy lifetime to the task vault
//! session.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use super::allowlist::EgressAllowlist;

// ─────────────────────────────────────────────────────────────────────────────
// EgressProxyConfig
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration for an egress proxy instance.
#[derive(Debug, Clone, Default)]
pub struct EgressProxyConfig {
    /// Allowlist controlling which domains may be forwarded.
    pub allowlist: EgressAllowlist,
}

// ─────────────────────────────────────────────────────────────────────────────
// EgressProxy
// ─────────────────────────────────────────────────────────────────────────────

/// A running egress proxy.
///
/// Dropping this handle aborts the proxy's background task.
pub struct EgressProxy {
    /// The local address the proxy is bound to.
    pub local_addr: SocketAddr,
    /// Background task handle (aborted on drop).
    _task: tokio::task::JoinHandle<()>,
}

impl EgressProxy {
    /// Start the proxy and return immediately.
    pub async fn start(cfg: EgressProxyConfig) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("Failed to bind egress proxy")?;
        let local_addr = listener.local_addr()?;
        info!("Egress proxy listening on {local_addr}");

        let allowlist = Arc::new(cfg.allowlist);
        let task = tokio::spawn(async move {
            accept_loop(listener, allowlist).await;
        });

        Ok(Self {
            local_addr,
            _task: task,
        })
    }

    /// Return the `HTTP_PROXY` URL for this proxy.
    pub fn proxy_url(&self) -> String {
        format!("http://{}", self.local_addr)
    }

    /// Return the environment variable map to inject into subprocesses.
    pub fn env_vars(&self) -> Vec<(String, String)> {
        let url = self.proxy_url();
        vec![
            ("HTTP_PROXY".to_string(), url.clone()),
            ("HTTPS_PROXY".to_string(), url.clone()),
            ("http_proxy".to_string(), url.clone()),
            ("https_proxy".to_string(), url.clone()),
            (
                "NO_PROXY".to_string(),
                "127.0.0.1,::1,localhost".to_string(),
            ),
        ]
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// accept loop
// ─────────────────────────────────────────────────────────────────────────────

async fn accept_loop(listener: TcpListener, allowlist: Arc<EgressAllowlist>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let al = Arc::clone(&allowlist);
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, al).await {
                        debug!("Egress proxy connection from {peer} error: {e}");
                    }
                });
            }
            Err(e) => {
                warn!("Egress proxy accept error: {e}");
                // Small back-off to avoid tight loop on persistent errors.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

async fn handle_connection(mut client: TcpStream, allowlist: Arc<EgressAllowlist>) -> Result<()> {
    // Read the first line of the HTTP request to determine the method and target.
    let mut buf = vec![0u8; 4096];
    let n = client
        .read(&mut buf)
        .await
        .context("Failed to read from client")?;
    if n == 0 {
        return Ok(());
    }

    let request_head = String::from_utf8_lossy(&buf[..n]);
    let first_line = request_head.lines().next().unwrap_or("").to_string();

    if first_line.starts_with("CONNECT ") {
        handle_connect(client, &first_line, &buf[..n], allowlist).await
    } else {
        handle_plain_http(client, &first_line, &buf[..n], allowlist).await
    }
}

/// Handle HTTP CONNECT (used by HTTPS tunnels).
async fn handle_connect(
    mut client: TcpStream,
    first_line: &str,
    _raw: &[u8],
    allowlist: Arc<EgressAllowlist>,
) -> Result<()> {
    // CONNECT api.openai.com:443 HTTP/1.1
    let target = first_line
        .split_whitespace()
        .nth(1)
        .context("CONNECT missing target")?;

    let host = target.split(':').next().unwrap_or(target);

    if !allowlist.allows(host) {
        warn!("Egress proxy blocked CONNECT to {host}");
        client
            .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n")
            .await?;
        return Ok(());
    }

    debug!("Egress proxy: CONNECT {target} allowed");

    let mut upstream = TcpStream::connect(target)
        .await
        .with_context(|| format!("Failed to connect to upstream {target}"))?;

    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;

    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// Handle plain HTTP forwarding.
async fn handle_plain_http(
    mut client: TcpStream,
    first_line: &str,
    raw: &[u8],
    allowlist: Arc<EgressAllowlist>,
) -> Result<()> {
    // Extract Host header.
    let raw_str = String::from_utf8_lossy(raw);
    let host_header = raw_str
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("host:"))
        .and_then(|l| l.split_once(':').map(|x| x.1.trim()))
        .unwrap_or_default();

    if host_header.is_empty() {
        warn!("Egress proxy blocked HTTP {first_line} (missing Host header)");
        client
            .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
            .await?;
        return Ok(());
    }

    // Split host_header into domain and optional port
    let (host, port) = match host_header.split_once(':') {
        Some((h, p)) => {
            let port_parsed = p.parse::<u16>().unwrap_or(80);
            (h.to_string(), port_parsed)
        }
        None => (host_header.to_string(), 80),
    };

    if !allowlist.allows(&host) {
        warn!("Egress proxy blocked HTTP {first_line} (host={host})");
        client
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
            .await?;
        return Ok(());
    }

    debug!("Egress proxy: HTTP {first_line} allowed (host={host}, port={port})");

    // Connect to the host on the specified port.
    let upstream_addr = format!("{host}:{port}");
    let mut upstream = TcpStream::connect(&upstream_addr)
        .await
        .with_context(|| format!("Failed to connect to upstream {upstream_addr}"))?;

    // Forward the buffered request then splice the connection.
    upstream.write_all(raw).await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn proxy_starts_and_binds_port() {
        let proxy = EgressProxy::start(EgressProxyConfig::default())
            .await
            .unwrap();
        assert_eq!(proxy.local_addr.ip().to_string(), "127.0.0.1");
        assert!(proxy.local_addr.port() > 0);
    }

    #[tokio::test]
    async fn proxy_url_is_localhost() {
        let proxy = EgressProxy::start(EgressProxyConfig::default())
            .await
            .unwrap();
        assert!(proxy.proxy_url().starts_with("http://127.0.0.1:"));
    }

    #[tokio::test]
    async fn env_vars_include_http_proxy() {
        let proxy = EgressProxy::start(EgressProxyConfig::default())
            .await
            .unwrap();
        let vars = proxy.env_vars();
        let has_http = vars.iter().any(|(k, _)| k == "HTTP_PROXY");
        let has_https = vars.iter().any(|(k, _)| k == "HTTPS_PROXY");
        assert!(has_http);
        assert!(has_https);
    }

    #[tokio::test]
    async fn test_proxy_handles_custom_port() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let upstream_port = upstream_addr.port();

        let allowlist = EgressAllowlist::from_str("127.0.0.1");
        let proxy = EgressProxy::start(EgressProxyConfig { allowlist })
            .await
            .unwrap();

        let upstream_task = tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let mut buf = vec![0u8; 1024];
            let n = stream.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(request.contains("GET http://"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\nHello World!")
                .await
                .unwrap();
        });

        let mut client = TcpStream::connect(proxy.local_addr).await.unwrap();
        let req = format!(
            "GET http://127.0.0.1:{upstream_port}/ HTTP/1.1\r\nHost: 127.0.0.1:{upstream_port}\r\n\r\n"
        );
        client.write_all(req.as_bytes()).await.unwrap();

        let mut resp = vec![0u8; 1024];
        let n = client.read(&mut resp).await.unwrap();
        let resp_str = String::from_utf8_lossy(&resp[..n]);
        assert!(resp_str.contains("HTTP/1.1 200 OK"));
        assert!(resp_str.contains("Hello World!"));

        upstream_task.await.unwrap();
    }
}
