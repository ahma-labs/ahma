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

use parking_lot::RwLock;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ahma_common::net_approval::{NetApprovalCoordinator, NetApprovalDecision, NetResolveOutcome};
use ahma_harness_tools::egress_guard::is_blocked_ip;
use anyhow::{Context, Result};
use rmcp::service::{Peer, RoleServer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use super::allowlist::EgressAllowlist;
use super::net_prompt::{NetApprovalForm, parse_answer, prompt_message};

// ─────────────────────────────────────────────────────────────────────────────
// EgressProxyConfig
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration for an egress proxy instance.
#[derive(Debug, Clone)]
pub struct EgressProxyConfig {
    /// Allowlist controlling which domains may be forwarded.
    pub allowlist: EgressAllowlist,
    /// When `true` (the default), a forwarded request whose target host resolves
    /// to a private/loopback/link-local/cloud-metadata address is refused *at
    /// connect time on the resolved IP* — closing the DNS-rebinding hole where an
    /// allowlisted domain flips its DNS to `127.0.0.1` or `169.254.169.254` after
    /// the hostname passes the allowlist (SSRF). Mirrors `block_private` in
    /// [`ahma_harness_tools::egress_guard`]. Tests that must reach a loopback mock
    /// upstream set this `false`.
    pub block_private: bool,
    /// Interactive network-approval context (SPEC R-NET): when a subprocess
    /// reaches a domain not on `allowlist`, this raises an MCP
    /// `elicitation/create` prompt at the attached peer instead of denying
    /// outright, mirroring the `fetch_webpage` web-approval flow (R-WEB.5).
    /// Defaults to a fresh coordinator with no peer attached, which denies
    /// unlisted domains exactly as before this feature existed.
    pub net_approval: NetApprovalContext,
}

impl Default for EgressProxyConfig {
    fn default() -> Self {
        Self {
            allowlist: EgressAllowlist::default(),
            block_private: true,
            net_approval: NetApprovalContext::default(),
        }
    }
}

/// Bundles the state needed to raise an interactive network-approval prompt
/// for a domain not on the static allowlist: the session coordinator (dedup,
/// session grants/denies) and a handle to the MCP peer to elicit against.
/// Cheap to clone — both fields are `Arc`.
#[derive(Debug, Clone)]
pub struct NetApprovalContext {
    /// Session grant/deny state and in-flight decision dedup.
    pub coordinator: Arc<NetApprovalCoordinator>,
    /// The connected MCP peer to prompt via `elicitation/create`. `None`
    /// means no interactive surface is attached (yet, or ever, for
    /// non-MCP callers like the task-tree orchestrator), so an unlisted
    /// domain is denied outright.
    pub peer: Arc<RwLock<Option<Peer<RoleServer>>>>,
}

impl Default for NetApprovalContext {
    fn default() -> Self {
        Self {
            coordinator: Arc::new(NetApprovalCoordinator::new()),
            peer: Arc::new(RwLock::new(None)),
        }
    }
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
    /// The allowlist actually installed in the accept loop.
    ///
    /// Kept on the handle so the effective policy is *observable* rather than
    /// swallowed by `start`. The seam between "which hosts did we decide on" and
    /// "which hosts is the running proxy enforcing" is exactly where a wiring
    /// mistake hides — a caller that built the right union and passed the wrong
    /// variable would still start a proxy on a real port and look healthy.
    allowlist: Arc<EgressAllowlist>,
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
        let block_private = cfg.block_private;
        let net_approval = cfg.net_approval;
        let task = tokio::spawn({
            let allowlist = Arc::clone(&allowlist);
            async move {
                accept_loop(listener, allowlist, block_private, net_approval).await;
            }
        });

        Ok(Self {
            local_addr,
            allowlist,
            _task: task,
        })
    }

    /// Whether the running proxy would forward a connection to `host` on the
    /// strength of its static allowlist alone (before interactive approval).
    pub fn allows(&self, host: &str) -> bool {
        self.allowlist.allows(host)
    }

    /// The allowlist entries the running proxy is enforcing, in canonical form.
    pub fn allowlist_entries(&self) -> Vec<String> {
        self.allowlist.entries()
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

async fn accept_loop(
    listener: TcpListener,
    allowlist: Arc<EgressAllowlist>,
    block_private: bool,
    net_approval: NetApprovalContext,
) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let al = Arc::clone(&allowlist);
                let net_approval = net_approval.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, al, block_private, net_approval).await
                    {
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

/// Resolve `host:port` and, when `block_private`, drop any address in a
/// private/loopback/link-local/cloud-metadata range. Returns the vetted socket
/// addresses to connect to (so the connection targets an IP that was actually
/// checked — no second resolution that could rebind), or an error string when the
/// name does not resolve or resolves *only* to blocked addresses.
async fn vetted_addrs(
    host: &str,
    port: u16,
    block_private: bool,
) -> std::result::Result<Vec<SocketAddr>, String> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("DNS resolution of '{host}' failed: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("'{host}' resolved to no addresses"));
    }
    if !block_private {
        return Ok(addrs);
    }
    let allowed: Vec<SocketAddr> = addrs
        .into_iter()
        .filter(|a| !is_blocked_ip(&a.ip()))
        .collect();
    if allowed.is_empty() {
        return Err(format!(
            "'{host}' resolves only to private/loopback/link-local addresses (SSRF protection)"
        ));
    }
    Ok(allowed)
}

async fn handle_connection(
    mut client: TcpStream,
    allowlist: Arc<EgressAllowlist>,
    block_private: bool,
    net_approval: NetApprovalContext,
) -> Result<()> {
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
        handle_connect(
            client,
            &first_line,
            &buf[..n],
            allowlist,
            block_private,
            net_approval,
        )
        .await
    } else {
        handle_plain_http(
            client,
            &first_line,
            &buf[..n],
            allowlist,
            block_private,
            net_approval,
        )
        .await
    }
}

/// Decide whether an unlisted domain may proceed, raising an interactive MCP
/// `elicitation/create` prompt at the connected peer when one is attached
/// (SPEC R-NET; mirrors the `fetch_webpage` web-approval flow, R-WEB.5).
/// Returns `true` when the connection should proceed — statically
/// allowlisted, session-granted, or freshly approved — and `false` for every
/// other outcome: no peer attached, the client cannot elicit, the prompt
/// times out or errors, or the human declines. Fails safe throughout: any
/// ambiguous outcome denies rather than allows.
async fn resolve_egress_approval(
    host: &str,
    target: &str,
    allowlist: &EgressAllowlist,
    net_approval: &NetApprovalContext,
) -> bool {
    if allowlist.allows(host) {
        return true;
    }
    if net_approval.coordinator.is_session_granted(host) {
        return true;
    }
    if net_approval.coordinator.is_session_denied(host) {
        return false;
    }

    // Dedup: a decision for this domain already in flight (a concurrent
    // connection) is not double-prompted — deny this one; the in-flight
    // answer will let a retry through once resolved.
    let Some(req) = net_approval.coordinator.begin(host, target) else {
        return false;
    };

    let peer_opt = net_approval.peer.read().clone();
    let elicited: Option<NetApprovalDecision> = match peer_opt {
        None => None,
        Some(peer) => match peer
            .elicit_with_timeout::<NetApprovalForm>(
                prompt_message(host, target),
                Some(Duration::from_secs(120)),
            )
            .await
        {
            Ok(Some(form)) => Some(parse_answer(&form.decision)),
            // Accepted with no content, or an explicit decline: remember deny.
            Ok(None) | Err(rmcp::service::ElicitationError::UserDeclined) => {
                Some(NetApprovalDecision::Deny)
            }
            // The client cannot elicit: no other surface to fall back to for
            // subprocess egress (unlike `fetch_webpage`, this is not a
            // TUI-routed request), so fail safe.
            Err(rmcp::service::ElicitationError::CapabilityNotSupported) => None,
            // Cancelled, timed out, or transport error: don't leave it
            // pending; deny without remembering so a later connection may
            // re-ask.
            Err(e) => {
                debug!("network approval prompt unavailable for '{host}': {e}");
                net_approval.coordinator.cancel(&req.decision_id);
                return false;
            }
        },
    };

    let Some(decision) = elicited else {
        net_approval.coordinator.cancel(&req.decision_id);
        warn!(
            "Egress proxy: no interactive surface available to approve '{host}'; denying \
             (add it to [network] allow in ~/.ahma/settings.toml to permit)"
        );
        return false;
    };

    match net_approval.coordinator.resolve(&req.decision_id, decision) {
        NetResolveOutcome::AllowOnce { domain } => {
            info!(domain = %domain, "network egress approved for this connection");
            true
        }
        NetResolveOutcome::AllowSession { domain } => {
            info!(domain = %domain, "network egress approved for this session");
            true
        }
        NetResolveOutcome::Persist { domain } => {
            match ahma_common::config::settings_path() {
                Some(file) => match ahma_common::net_approval::persist_net_allow(&file, &domain) {
                    Ok(true) => info!(
                        domain = %domain,
                        "network egress approved and saved to [network].allow"
                    ),
                    Ok(false) => info!(
                        domain = %domain,
                        "network egress approved (already in [network].allow)"
                    ),
                    Err(e) => warn!(
                        domain = %domain,
                        "network egress approved for the session but persisting failed: {e}"
                    ),
                },
                None => warn!(
                    "network egress approved but ~/.ahma/settings.toml is not locatable to persist"
                ),
            }
            true
        }
        NetResolveOutcome::Denied { domain } => {
            info!(domain = %domain, "network egress denied by user");
            false
        }
        // A twin surface resolved first, or the decision vanished: fail safe.
        NetResolveOutcome::AlreadyResolved | NetResolveOutcome::Unknown => false,
    }
}

/// Handle HTTP CONNECT (used by HTTPS tunnels).
async fn handle_connect(
    mut client: TcpStream,
    first_line: &str,
    _raw: &[u8],
    allowlist: Arc<EgressAllowlist>,
    block_private: bool,
    net_approval: NetApprovalContext,
) -> Result<()> {
    // CONNECT api.openai.com:443 HTTP/1.1
    let target = first_line
        .split_whitespace()
        .nth(1)
        .context("CONNECT missing target")?;

    // Target is `host:port`; the port is required for CONNECT but default to 443.
    let (host, port) = match target.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().unwrap_or(443)),
        None => (target, 443),
    };

    if !resolve_egress_approval(host, target, &allowlist, &net_approval).await {
        warn!("Egress proxy blocked CONNECT to {host}");
        client
            .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n")
            .await?;
        return Ok(());
    }

    // The hostname is allowlisted, but resolve it and vet the IP before
    // connecting — an allowlisted domain must not tunnel to a private/loopback
    // address via DNS rebinding (SSRF).
    let addrs = match vetted_addrs(host, port, block_private).await {
        Ok(a) => a,
        Err(reason) => {
            warn!("Egress proxy blocked CONNECT to {host}: {reason}");
            client
                .write_all(
                    b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n",
                )
                .await?;
            return Ok(());
        }
    };

    debug!("Egress proxy: CONNECT {target} allowed");

    let mut upstream = TcpStream::connect(addrs.as_slice())
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
    block_private: bool,
    net_approval: NetApprovalContext,
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

    let target = format!("{host}:{port}");
    if !resolve_egress_approval(&host, &target, &allowlist, &net_approval).await {
        warn!("Egress proxy blocked HTTP {first_line} (host={host})");
        client
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
            .await?;
        return Ok(());
    }

    // Vet the resolved IP before connecting (see `handle_connect`): an allowlisted
    // host must not reach a private/loopback address via DNS rebinding (SSRF).
    let addrs = match vetted_addrs(&host, port, block_private).await {
        Ok(a) => a,
        Err(reason) => {
            warn!("Egress proxy blocked HTTP {first_line} (host={host}): {reason}");
            client
                .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                .await?;
            return Ok(());
        }
    };

    debug!("Egress proxy: HTTP {first_line} allowed (host={host}, port={port})");

    // Connect to a vetted resolved address on the specified port.
    let mut upstream = TcpStream::connect(addrs.as_slice())
        .await
        .with_context(|| format!("Failed to connect to upstream {host}:{port}"))?;

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
    use ahma_common::timeouts::TestTimeouts;

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
        // Loopback upstream: opt out of the private-range block for this test.
        let proxy = EgressProxy::start(EgressProxyConfig {
            allowlist,
            block_private: false,
            ..Default::default()
        })
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

    // ─────────────────────────────────────────────────────────────────────
    // Helpers for driving requests through a running proxy.
    //
    // The connection handlers (`handle_connection`, `handle_connect`,
    // `handle_plain_http`) operate on real `TcpStream`s, so their branches are
    // exercised by connecting to a started proxy and sending crafted requests,
    // then asserting on the bytes written back (or the connection being closed).
    // ─────────────────────────────────────────────────────────────────────

    /// Read one chunk of the proxy's response with a timeout.
    /// Returns the bytes read (may be empty if the proxy closed the connection).
    ///
    /// The timeout is a *hang bound*, not an assertion: these tests assert on the
    /// bytes the proxy writes back, never on how quickly it writes them. A
    /// hard-coded 5s was therefore a latent flake — on a loaded 2-core CI runner the
    /// proxy simply hadn't answered yet, and `connect_session_denied_domain_returns_
    /// 407_without_reprompting` failed with "read timed out" on macOS. Use the shared,
    /// platform-scaled policy (30s, ×4 on Windows) so a slow runner is tolerated while
    /// a genuine hang is still caught.
    async fn read_chunk(client: &mut TcpStream) -> Vec<u8> {
        let mut resp = vec![0u8; 2048];
        let timeout = ahma_common::timeouts::TestTimeouts::get(
            ahma_common::timeouts::TimeoutCategory::HttpRequest,
        );
        let n = tokio::time::timeout(timeout, client.read(&mut resp))
            .await
            .expect("proxy did not respond before the hang bound")
            .expect("read failed");
        resp.truncate(n);
        resp
    }

    /// Start a proxy with the private-range block **off** so tests can use
    /// loopback upstreams. Rebind-block tests below opt back in explicitly.
    async fn start_proxy(allowlist: EgressAllowlist) -> EgressProxy {
        EgressProxy::start(EgressProxyConfig {
            allowlist,
            block_private: false,
            ..Default::default()
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn connect_to_blocked_host_returns_407() {
        // deny-all allowlist → CONNECT must be rejected with 407.
        let proxy = start_proxy(EgressAllowlist::deny_all()).await;
        let mut client = TcpStream::connect(proxy.local_addr).await.unwrap();
        client
            .write_all(b"CONNECT blocked.example.com:443 HTTP/1.1\r\nHost: blocked.example.com:443\r\n\r\n")
            .await
            .unwrap();

        let resp = read_chunk(&mut client).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("407 Proxy Authentication Required"),
            "expected 407 block response, got: {resp_str:?}"
        );
        assert!(
            !resp_str.contains("200 Connection Established"),
            "blocked host must not be tunnelled"
        );
    }

    #[tokio::test]
    async fn connect_missing_target_closes_connection() {
        // "CONNECT " with no target → handle_connect returns an error before any
        // response is written, so the client sees an immediate EOF.
        let proxy = start_proxy(EgressAllowlist::from_str("*")).await;
        let mut client = TcpStream::connect(proxy.local_addr).await.unwrap();
        client.write_all(b"CONNECT \r\n\r\n").await.unwrap();

        let resp = read_chunk(&mut client).await;
        assert!(
            resp.is_empty(),
            "missing CONNECT target must close without a response, got: {:?}",
            String::from_utf8_lossy(&resp)
        );
    }

    #[tokio::test]
    async fn connect_allowed_but_upstream_unreachable_closes() {
        // Host is allowed, but the upstream port has no listener, so
        // `TcpStream::connect` fails and the handler returns an error *before*
        // sending "200 Connection Established".
        let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead); // free the port so connects fail

        let proxy = start_proxy(EgressAllowlist::from_str("127.0.0.1")).await;
        let mut client = TcpStream::connect(proxy.local_addr).await.unwrap();
        // Host "127.0.0.1" is allowed; the connect target points at a freed
        // port, so `TcpStream::connect` fails after the allow check.
        let req = format!("CONNECT 127.0.0.1:{} HTTP/1.1\r\n\r\n", dead_addr.port());
        client.write_all(req.as_bytes()).await.unwrap();

        let resp = read_chunk(&mut client).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            !resp_str.contains("200 Connection Established"),
            "unreachable upstream must not yield a 200, got: {resp_str:?}"
        );
    }

    #[tokio::test]
    async fn connect_allowed_tunnels_bidirectionally() {
        // Stand up an upstream that, once connected, reads a request and replies.
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();

        let upstream_task = tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let mut buf = vec![0u8; 64];
            let n = stream.read(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"ping");
            stream.write_all(b"pong").await.unwrap();
        });

        let proxy = start_proxy(EgressAllowlist::from_str("127.0.0.1")).await;
        let mut client = TcpStream::connect(proxy.local_addr).await.unwrap();
        let req = format!(
            "CONNECT 127.0.0.1:{} HTTP/1.1\r\n\r\n",
            upstream_addr.port()
        );
        client.write_all(req.as_bytes()).await.unwrap();

        // First the proxy confirms the tunnel.
        let established = read_chunk(&mut client).await;
        let established_str = String::from_utf8_lossy(&established);
        assert!(
            established_str.contains("200 Connection Established"),
            "expected tunnel confirmation, got: {established_str:?}"
        );

        // Now the tunnel is spliced: send through it and read the upstream reply.
        client.write_all(b"ping").await.unwrap();
        let echoed = read_chunk(&mut client).await;
        assert_eq!(&echoed[..], b"pong", "tunnel must forward upstream bytes");

        upstream_task.await.unwrap();
    }

    #[tokio::test]
    async fn plain_http_missing_host_header_returns_400() {
        let proxy = start_proxy(EgressAllowlist::from_str("*")).await;
        let mut client = TcpStream::connect(proxy.local_addr).await.unwrap();
        // No Host header at all → host_header is empty → 400.
        client
            .write_all(b"GET http://example.com/ HTTP/1.1\r\n\r\n")
            .await
            .unwrap();

        let resp = read_chunk(&mut client).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("400 Bad Request"),
            "missing Host header must yield 400, got: {resp_str:?}"
        );
    }

    #[tokio::test]
    async fn plain_http_blocked_host_returns_403() {
        let proxy = start_proxy(EgressAllowlist::deny_all()).await;
        let mut client = TcpStream::connect(proxy.local_addr).await.unwrap();
        client
            .write_all(b"GET http://example.com/ HTTP/1.1\r\nHost: example.com:8080\r\n\r\n")
            .await
            .unwrap();

        let resp = read_chunk(&mut client).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("403 Forbidden"),
            "blocked host must yield 403, got: {resp_str:?}"
        );
    }

    #[tokio::test]
    async fn plain_http_host_without_port_blocked_returns_403() {
        // Host header has no ':' → split_once returns None → default port 80.
        // Host is still blocked, so the 403 path runs (exercising the None arm).
        let proxy = start_proxy(EgressAllowlist::deny_all()).await;
        let mut client = TcpStream::connect(proxy.local_addr).await.unwrap();
        client
            .write_all(b"GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n")
            .await
            .unwrap();

        let resp = read_chunk(&mut client).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("403 Forbidden"),
            "host without port should still resolve and be blocked, got: {resp_str:?}"
        );
    }

    #[tokio::test]
    async fn plain_http_bad_port_falls_back_to_80_and_blocked_returns_403() {
        // Host has a ':' but a non-numeric port → parse::<u16>() fails →
        // unwrap_or(80). Host is blocked, so we still observe a 403, having
        // exercised the parse-failure fallback branch.
        let proxy = start_proxy(EgressAllowlist::deny_all()).await;
        let mut client = TcpStream::connect(proxy.local_addr).await.unwrap();
        client
            .write_all(b"GET http://example.com/ HTTP/1.1\r\nHost: example.com:not-a-port\r\n\r\n")
            .await
            .unwrap();

        let resp = read_chunk(&mut client).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("403 Forbidden"),
            "bad port should fall back and host be blocked, got: {resp_str:?}"
        );
    }

    /// Start a proxy with the private-range block **on** (production default).
    async fn start_guarded_proxy(allowlist: EgressAllowlist) -> EgressProxy {
        EgressProxy::start(EgressProxyConfig {
            allowlist,
            block_private: true,
            ..Default::default()
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn connect_rebind_to_loopback_blocked() {
        // The hostname is on the allowlist, but it resolves to a loopback address.
        // With the private-range guard on (default), the CONNECT must be refused at
        // the resolved IP — this is the DNS-rebinding / SSRF case (R-WEB.3).
        let proxy = start_guarded_proxy(EgressAllowlist::from_str("localhost")).await;
        let mut client = TcpStream::connect(proxy.local_addr).await.unwrap();
        client
            .write_all(b"CONNECT localhost:8080 HTTP/1.1\r\nHost: localhost:8080\r\n\r\n")
            .await
            .unwrap();

        let resp = read_chunk(&mut client).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("407 Proxy Authentication Required"),
            "an allowlisted host resolving to loopback must be refused, got: {resp_str:?}"
        );
        assert!(
            !resp_str.contains("200 Connection Established"),
            "must not tunnel to a loopback address"
        );
    }

    #[tokio::test]
    async fn plain_http_rebind_to_loopback_blocked() {
        // Same as above for the plain-HTTP path: allowlisted host, loopback IP → 403.
        let proxy = start_guarded_proxy(EgressAllowlist::from_str("localhost")).await;
        let mut client = TcpStream::connect(proxy.local_addr).await.unwrap();
        client
            .write_all(b"GET http://localhost/ HTTP/1.1\r\nHost: localhost:8080\r\n\r\n")
            .await
            .unwrap();

        let resp = read_chunk(&mut client).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("403 Forbidden"),
            "an allowlisted host resolving to loopback must be refused, got: {resp_str:?}"
        );
    }

    #[tokio::test]
    async fn connect_ip_literal_to_metadata_blocked() {
        // A subprocess that CONNECTs straight to the cloud-metadata IP (allowlisted
        // via `*`) is still refused by the resolved-IP guard.
        let proxy = start_guarded_proxy(EgressAllowlist::from_str("*")).await;
        let mut client = TcpStream::connect(proxy.local_addr).await.unwrap();
        client
            .write_all(b"CONNECT 169.254.169.254:80 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();

        let resp = read_chunk(&mut client).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("407 Proxy Authentication Required"),
            "the cloud-metadata IP must be refused even under a `*` allowlist, got: {resp_str:?}"
        );
    }

    #[tokio::test]
    async fn empty_request_closes_gracefully() {
        // Client connects then closes its write half without sending anything.
        // The handler reads 0 bytes and returns Ok(()), dropping the stream.
        let proxy = start_proxy(EgressAllowlist::from_str("*")).await;
        let mut client = TcpStream::connect(proxy.local_addr).await.unwrap();
        client.shutdown().await.unwrap();

        let resp = read_chunk(&mut client).await;
        assert!(
            resp.is_empty(),
            "empty request must be handled without a response, got: {:?}",
            String::from_utf8_lossy(&resp)
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // Interactive network-approval (SPEC R-NET) — proxy-level wiring.
    //
    // The `elicitation/create` round-trip itself requires a live MCP peer and
    // is exercised by `net_prompt::tests` (pure mapping) and
    // `ahma_common::net_approval::tests` (coordinator semantics); these tests
    // cover what the proxy does around that round-trip: a domain granted for
    // the session bypasses the static allowlist entirely, a domain denied for
    // the session is refused without re-prompting, and — critically — a
    // connection with no peer attached fails safe *immediately* (no 120s
    // elicit timeout stall) and frees the dedup gate so a later connection
    // (e.g. once a capable client attaches) may re-ask.
    // ─────────────────────────────────────────────────────────────────────

    async fn start_proxy_with_net_approval(
        allowlist: EgressAllowlist,
        net_approval: NetApprovalContext,
    ) -> EgressProxy {
        EgressProxy::start(EgressProxyConfig {
            allowlist,
            block_private: false,
            net_approval,
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn connect_session_granted_domain_bypasses_static_denylist() {
        // The host is NOT on the static allowlist, but was already approved
        // for the session (as if a prior connection's elicit answered
        // `session`) — the connection must proceed without prompting again.
        // The CONNECT target is the loopback IP itself (no DNS in a test
        // sandbox), so that's what gets the session grant.
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let mut buf = vec![0u8; 64];
            let n = stream.read(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"ping");
            stream.write_all(b"pong").await.unwrap();
        });

        let net_approval = NetApprovalContext::default();
        let req = net_approval
            .coordinator
            .begin("127.0.0.1", "127.0.0.1:0")
            .unwrap();
        net_approval
            .coordinator
            .resolve(&req.decision_id, NetApprovalDecision::AllowSession);

        let proxy = start_proxy_with_net_approval(EgressAllowlist::deny_all(), net_approval).await;
        let mut client = TcpStream::connect(proxy.local_addr).await.unwrap();
        let req = format!(
            "CONNECT 127.0.0.1:{} HTTP/1.1\r\n\r\n",
            upstream_addr.port()
        );
        client.write_all(req.as_bytes()).await.unwrap();

        let established = read_chunk(&mut client).await;
        let established_str = String::from_utf8_lossy(&established);
        assert!(
            established_str.contains("200 Connection Established"),
            "a session-granted domain must tunnel without re-prompting, got: {established_str:?}"
        );

        client.write_all(b"ping").await.unwrap();
        let echoed = read_chunk(&mut client).await;
        assert_eq!(&echoed[..], b"pong");

        upstream_task.await.unwrap();
    }

    #[tokio::test]
    async fn connect_session_denied_domain_returns_407_without_reprompting() {
        let net_approval = NetApprovalContext::default();
        let req = net_approval
            .coordinator
            .begin("denied.example", "denied.example:443")
            .unwrap();
        net_approval
            .coordinator
            .resolve(&req.decision_id, NetApprovalDecision::Deny);

        let proxy =
            start_proxy_with_net_approval(EgressAllowlist::from_str("*"), net_approval).await;
        let mut client = TcpStream::connect(proxy.local_addr).await.unwrap();
        client
            .write_all(b"CONNECT denied.example:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();

        let resp = read_chunk(&mut client).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("407 Proxy Authentication Required"),
            "a session-denied domain must be refused even under a `*` allowlist, got: {resp_str:?}"
        );
    }

    #[tokio::test]
    async fn connect_unknown_domain_with_no_peer_denies_immediately_and_frees_dedup_gate() {
        // No peer attached (the default `NetApprovalContext`): an unlisted
        // domain must be denied without waiting out the 120s elicit timeout,
        // and the in-flight decision must be freed so a second attempt is
        // independently deniable rather than hanging on stale dedup state.
        let proxy = start_proxy(EgressAllowlist::deny_all()).await;

        for _ in 0..2 {
            let mut client = TcpStream::connect(proxy.local_addr).await.unwrap();
            client
                .write_all(b"CONNECT nopeer.example:443 HTTP/1.1\r\n\r\n")
                .await
                .unwrap();
            let resp = tokio::time::timeout(TestTimeouts::scale_secs(5), read_chunk(&mut client))
                .await
                .expect("must deny immediately, not stall on the elicit timeout");
            let resp_str = String::from_utf8_lossy(&resp);
            assert!(
                resp_str.contains("407 Proxy Authentication Required"),
                "got: {resp_str:?}"
            );
        }
    }
}
