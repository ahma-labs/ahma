//! TUI connection resolution: probe available local transports and pick the best one.
//!
//! Default behaviour when `--connect` is not supplied:
//!
//! * **Unix** — try the local Unix domain socket (default `/tmp/ahma.sock`, or
//!   `AHMA_UNIX_SOCKET` env var) first, then fall back to `http://localhost:3000`.
//! * **Windows / non-Unix** — go straight to `http://localhost:3000`.
//!
//! After a successful TCP/HTTP probe the server's `Alt-Svc` response header is
//! checked for an `h3` token.  When QUIC is advertised **and** a persistent local
//! TLS certificate exists under `~/.ahma/tls/`, the resolved transport is upgraded
//! to [`ResolvedTransport::Http3`] so the TUI can label the connection correctly.
//!
//! When `--connect <URL>` is explicitly supplied the value is used as the sole
//! candidate; no fallback is attempted.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use tracing::debug;

// ─────────────────────────────────────────────────────────────────────────────
// Public types
// ─────────────────────────────────────────────────────────────────────────────

/// The low-level transport used to reach the server.
#[derive(Debug, Clone)]
pub enum ResolvedTransport {
    /// HTTP (plain or TLS) over TCP.  The inner string is the base URL,
    /// e.g. `"http://localhost:3000"`.
    Http(String),
    /// HTTP/3 over QUIC.  QUIC was detected via the server's `Alt-Svc` response
    /// header and a persistent local TLS certificate is available.  Health polls
    /// from the TUI use regular HTTPS (TCP) since QUIC connectivity was already
    /// verified at connection time.  The inner string is the base HTTP URL.
    Http3(String),
    /// HTTP over a Unix domain socket.  The inner string is the socket path,
    /// e.g. `"/tmp/ahma.sock"`.
    #[cfg(unix)]
    UnixSocket(String),
}

/// A confirmed working connection to an ahma server.
#[derive(Debug, Clone)]
pub struct ResolvedConnection {
    /// Human-readable string shown in the TUI header (e.g. `"http://localhost:3000"`
    /// or `"unix:///tmp/ahma.sock"`).
    pub display_url: String,
    /// The transport to use for all subsequent health polls.
    pub transport: ResolvedTransport,
}

impl ResolvedConnection {
    /// One-word label for the transport, shown next to the server URL.
    pub fn transport_label(&self) -> &'static str {
        match &self.transport {
            ResolvedTransport::Http(_) => "HTTP",
            ResolvedTransport::Http3(_) => "HTTP/3 (QUIC)",
            #[cfg(unix)]
            ResolvedTransport::UnixSocket(_) => "Unix socket",
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Resolution
// ─────────────────────────────────────────────────────────────────────────────

/// Resolve the best available connection to an ahma server.
///
/// * `explicit` — the value of `--connect`, if provided.  When `Some`, it is
///   treated as the sole candidate and the function returns an error if it is
///   unreachable.
/// * When `None`, the function tries candidates in order and returns the first
///   healthy one.  On Unix this is: Unix socket, then `http://localhost:3000`.
///   On non-Unix platforms it is: `http://localhost:3000` only.
pub async fn resolve_connection(explicit: Option<&str>) -> Result<ResolvedConnection> {
    if let Some(url) = explicit {
        return resolve_explicit(url).await;
    }
    resolve_default_candidates().await
}

async fn resolve_explicit(url: &str) -> Result<ResolvedConnection> {
    let candidate = parse_candidate(url);
    if probe_candidate(&candidate).await {
        let resolved = candidate_to_resolved(candidate);
        // Attempt QUIC upgrade if the server advertises h3 and local cert exists.
        if let Some(upgraded) = try_upgrade_to_http3(&resolved).await {
            return Ok(upgraded);
        }
        return Ok(resolved);
    }
    bail!(
        "The server at {} is unreachable. \
         Make sure `ahma serve` is running or check --connect.",
        url
    )
}

async fn resolve_default_candidates() -> Result<ResolvedConnection> {
    let candidates = default_candidates();
    let mut errors: Vec<String> = Vec::new();

    for candidate in &candidates {
        debug!("TUI: probing candidate {:?}", candidate.display_url);
        if probe_candidate(candidate).await {
            tracing::info!(
                "TUI: connected via {} ({})",
                candidate.transport_label(),
                candidate.display_url
            );
            // Try to upgrade TCP connections to HTTP/3 when QUIC is available.
            if let Some(upgraded) = try_upgrade_to_http3(candidate).await {
                tracing::info!(
                    "TUI: upgraded to {} ({})",
                    upgraded.transport_label(),
                    upgraded.display_url
                );
                return Ok(upgraded);
            }
            return Ok(candidate.clone());
        }
        errors.push(candidate.display_url.clone());
    }

    // Try starting our own server
    tracing::info!("TUI: No server found. Starting a background server...");
    match start_background_server().await {
        Ok(candidate) => {
            // Try to upgrade TCP connections to HTTP/3 when QUIC is available.
            if let Some(upgraded) = try_upgrade_to_http3(&candidate).await {
                tracing::info!(
                    "TUI: upgraded to {} ({})",
                    upgraded.transport_label(),
                    upgraded.display_url
                );
                return Ok(upgraded);
            }
            return Ok(candidate);
        }
        Err(e) => {
            errors.push(format!("start server error: {e}"));
        }
    }

    bail!(
        "No ahma server found and failed to start one. Tried: {}. \
         Start a server with `ahma serve http` or `ahma serve unix`, \
         or use --connect <URL> to specify a custom address.",
        errors.join(", ")
    )
}

#[cfg(unix)]
fn default_server_candidate() -> ResolvedConnection {
    let socket_path = unix_socket_default_path();
    ResolvedConnection {
        display_url: format!("unix://{socket_path}"),
        transport: ResolvedTransport::UnixSocket(socket_path),
    }
}

#[cfg(not(unix))]
fn default_server_candidate() -> ResolvedConnection {
    ResolvedConnection {
        display_url: "http://localhost:3000".to_string(),
        transport: ResolvedTransport::Http("http://localhost:3000".to_string()),
    }
}

async fn start_background_server() -> Result<ResolvedConnection> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ahma"));
    let mut cmd = tokio::process::Command::new(&exe);

    #[cfg(unix)]
    {
        cmd.arg("serve").arg("unix");
        // Detach from parent process group
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    {
        cmd.arg("serve").arg("http");
    }

    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    match cmd.spawn() {
        Ok(_) => {
            tracing::info!("TUI: spawned background server from {:?}", exe);
        }
        Err(e) => {
            bail!("Failed to spawn background server: {e}");
        }
    }

    // Poll candidate up to 5 seconds
    let candidate = default_server_candidate();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if probe_candidate(&candidate).await {
            return Ok(candidate);
        }
    }
    bail!("Background server failed to respond to health checks within 5s");
}

// ─────────────────────────────────────────────────────────────────────────────
// Candidate list
// ─────────────────────────────────────────────────────────────────────────────

fn default_candidates() -> Vec<ResolvedConnection> {
    let mut candidates: Vec<ResolvedConnection> = Vec::new();

    #[cfg(unix)]
    {
        let socket_path = unix_socket_default_path();
        let display = format!("unix://{socket_path}");
        candidates.push(ResolvedConnection {
            display_url: display,
            transport: ResolvedTransport::UnixSocket(socket_path),
        });
    }

    candidates.push(ResolvedConnection {
        display_url: "http://localhost:3000".to_string(),
        transport: ResolvedTransport::Http("http://localhost:3000".to_string()),
    });

    candidates
}

#[cfg(unix)]
fn unix_socket_default_path() -> String {
    std::env::var("AHMA_UNIX_SOCKET").unwrap_or_else(|_| "/tmp/ahma.sock".to_string())
}

/// Parse a user-supplied `--connect` value into a `ResolvedConnection`.
fn parse_candidate(url: &str) -> ResolvedConnection {
    // unix:///path  or  unix://path  (both mean filesystem socket)
    if let Some(rest) = url.strip_prefix("unix://") {
        let socket_path = rest.to_string();
        #[cfg(unix)]
        return ResolvedConnection {
            display_url: url.to_string(),
            transport: ResolvedTransport::UnixSocket(socket_path),
        };
        #[cfg(not(unix))]
        {
            let _ = socket_path; // suppress unused warning
            tracing::warn!(
                "Unix domain sockets are not supported on this platform; falling back to HTTP."
            );
        }
    }

    // Strip fragment (unix:///tmp/ahma.sock#/mcp) for display; we only probe /health
    let base = url
        .split_once('#')
        .map(|(base, _)| base)
        .unwrap_or(url)
        .trim_end_matches('/')
        .to_string();

    ResolvedConnection {
        display_url: base.clone(),
        transport: ResolvedTransport::Http(base),
    }
}

fn candidate_to_resolved(c: ResolvedConnection) -> ResolvedConnection {
    c
}

// ─────────────────────────────────────────────────────────────────────────────
// Probing
// ─────────────────────────────────────────────────────────────────────────────

/// Returns `true` if the server behind this candidate responds to `/health`
/// with a 2xx status.
pub async fn probe_candidate(candidate: &ResolvedConnection) -> bool {
    match &candidate.transport {
        ResolvedTransport::Http(url) | ResolvedTransport::Http3(url) => probe_http(url).await,
        #[cfg(unix)]
        ResolvedTransport::UnixSocket(path) => probe_unix_socket(path).await,
    }
}

async fn probe_http(base_url: &str) -> bool {
    let health_url = format!("{}/health", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap_or_default();
    client
        .get(&health_url)
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// Try to detect HTTP/3 (QUIC) availability via the `Alt-Svc` response header.
///
/// Returns `Some(upgraded)` with the transport upgraded to [`ResolvedTransport::Http3`]
/// when ALL of the following are true:
/// * The `/health` response includes an `Alt-Svc` header advertising `h3`.
/// * A persistent local TLS certificate exists at `~/.ahma/tls/`
///   (needed to trust the server's QUIC endpoint).
///
/// Returns `None` otherwise (the caller should keep the original HTTP candidate).
pub async fn try_upgrade_to_http3(candidate: &ResolvedConnection) -> Option<ResolvedConnection> {
    let base_url = match &candidate.transport {
        ResolvedTransport::Http(url) => url.clone(),
        // Already upgraded or not TCP-based; nothing to do.
        _ => return None,
    };

    // We only upgrade when the persistent TLS cert is present.
    if !ahma_common::local_tls::LocalTlsConfig::from_env().exists() {
        debug!("TUI: skipping H3 upgrade — no local TLS cert at ~/.ahma/tls/");
        return None;
    }

    let health_url = format!("{}/health", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .ok()?;

    let response = client.get(&health_url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }

    let alt_svc = response
        .headers()
        .get("alt-svc")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if parse_h3_from_alt_svc(alt_svc).is_some() {
        debug!("TUI: H3 advertised via Alt-Svc and local cert present — upgrading transport");
        Some(ResolvedConnection {
            display_url: candidate.display_url.clone(),
            transport: ResolvedTransport::Http3(base_url),
        })
    } else {
        None
    }
}

/// Parse an `h3` token from an `Alt-Svc` header value.
///
/// Returns `Some(port_hint)` where `port_hint` is the port string from
/// `h3=":3000"`, or `Some("")` if no port was specified.  Returns `None` if
/// no `h3` token is present.
///
/// # Examples
/// ```text
/// "h3=\":3000\""       → Some("3000")
/// "h3, h2"             → Some("")
/// "h2=\":443\""        → None
/// ```
pub fn parse_h3_from_alt_svc(alt_svc: &str) -> Option<&str> {
    // Alt-Svc value is a comma-separated list of: token="authority" or token
    for segment in alt_svc.split(',') {
        let seg = segment.trim();
        // Match "h3" or "h3=..."
        if seg == "h3" || seg.starts_with("h3=") || seg.starts_with("h3 ") {
            // Extract port from h3=":3000"
            let port = seg
                .strip_prefix("h3=\":")
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or("");
            return Some(port);
        }
    }
    None
}

/// Probe a Unix domain socket by sending a raw HTTP/1.0 GET /health request
/// and checking the response status line for a 2xx code.
///
/// We use raw I/O here to avoid pulling in a full HTTP-over-UDS library just
/// for a health check.
#[cfg(unix)]
async fn probe_unix_socket(socket_path: &str) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let connect = tokio::net::UnixStream::connect(socket_path);
    let Some(mut stream) = tokio::time::timeout(Duration::from_secs(2), connect)
        .await
        .ok()
        .and_then(|r| r.ok())
    else {
        debug!("TUI: Unix socket not reachable: {socket_path}");
        return false;
    };

    let request = b"GET /health HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    if stream.write_all(request).await.is_err() {
        return false;
    }

    let mut buf = Vec::with_capacity(256);
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut buf)).await;

    parse_status_2xx(&buf)
}

/// Parse the first line of an HTTP response and return `true` for 2xx codes.
#[cfg(any(unix, test))]
fn parse_status_2xx(response: &[u8]) -> bool {
    let text = std::str::from_utf8(response).unwrap_or("");
    let first_line = text.lines().next().unwrap_or("");
    // e.g. "HTTP/1.0 200 OK"  or  "HTTP/1.1 200 OK"
    let mut parts = first_line.splitn(3, ' ');
    parts.next(); // skip protocol version
    if let Some(code) = parts.next()
        && let Ok(status) = code.parse::<u16>()
    {
        return (200..300).contains(&status);
    }
    false
}

fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let mut parts = v.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

#[cfg(unix)]
async fn query_uds_health(path: &str) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let connect = tokio::net::UnixStream::connect(path);
    let mut stream = tokio::time::timeout(Duration::from_millis(200), connect)
        .await
        .ok()?
        .ok()?;

    let request = b"GET /health HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    if stream.write_all(request).await.is_err() {
        return None;
    }

    let mut buf = Vec::with_capacity(512);
    let _ = tokio::time::timeout(Duration::from_millis(200), stream.read_to_end(&mut buf)).await;

    let response_str = std::str::from_utf8(&buf).ok()?;
    let body = response_str.split("\r\n\r\n").nth(1)?;
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    parsed.get("version")?.as_str().map(String::from)
}

async fn query_tcp_health(url: &str) -> Option<String> {
    let health_url = format!("{}/health", url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(200))
        .build()
        .unwrap_or_default();
    let resp = client.get(&health_url).send().await.ok()?;
    if resp.status().is_success() {
        let parsed: serde_json::Value = resp.json().await.ok()?;
        return parsed.get("version")?.as_str().map(String::from);
    }
    None
}

pub async fn get_candidate_version(candidate: &ResolvedConnection) -> Option<String> {
    match &candidate.transport {
        ResolvedTransport::Http(url) | ResolvedTransport::Http3(url) => query_tcp_health(url).await,
        #[cfg(unix)]
        ResolvedTransport::UnixSocket(path) => query_uds_health(path).await,
    }
}

#[cfg(unix)]
async fn trigger_uds_restart(path: &str) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let connect = tokio::net::UnixStream::connect(path);
    let Ok(Ok(mut stream)) = tokio::time::timeout(Duration::from_millis(200), connect).await else {
        return false;
    };

    let request = b"POST /restart HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
    if stream.write_all(request).await.is_err() {
        return false;
    }

    let mut buf = Vec::with_capacity(512);
    let _ = tokio::time::timeout(Duration::from_millis(200), stream.read_to_end(&mut buf)).await;

    let response_str = std::str::from_utf8(&buf).unwrap_or("");
    response_str.starts_with("HTTP/1.") && response_str.contains(" 200 ")
}

async fn trigger_tcp_restart(url: &str) -> bool {
    let restart_url = format!("{}/restart", url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(200))
        .build()
        .unwrap_or_default();
    if let Ok(resp) = client.post(&restart_url).send().await {
        return resp.status().is_success();
    }
    false
}

pub async fn trigger_candidate_restart(candidate: &ResolvedConnection) -> bool {
    match &candidate.transport {
        ResolvedTransport::Http(url) | ResolvedTransport::Http3(url) => {
            trigger_tcp_restart(url).await
        }
        #[cfg(unix)]
        ResolvedTransport::UnixSocket(path) => trigger_uds_restart(path).await,
    }
}

async fn handle_existing_candidate(
    candidate: &ResolvedConnection,
    client_version: &str,
    bridge_version: String,
) -> Result<Option<()>> {
    if bridge_version == client_version {
        return Ok(Some(()));
    }

    let c_ver = parse_version(client_version);
    let b_ver = parse_version(&bridge_version);
    let client_is_newer = match (c_ver, b_ver) {
        (Some(c), Some(b)) => c > b,
        _ => true,
    };

    if client_is_newer {
        tracing::info!(
            "TUI version (v{}) is newer than running bridge version (v{}). Requesting bridge restart...",
            client_version,
            bridge_version
        );

        let _ = trigger_candidate_restart(candidate).await;

        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            if get_candidate_version(candidate).await.is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(None)
    } else {
        if std::env::var("AHMA_RESTARTED").is_err() {
            tracing::info!(
                "TUI version (v{}) is older than running bridge version (v{}). Attempting self-restart (re-exec)...",
                client_version,
                bridge_version
            );

            let exe = std::env::current_exe()?;
            let args: Vec<String> = std::env::args().skip(1).collect();
            let mut cmd = std::process::Command::new(exe);
            cmd.args(&args);
            cmd.env("AHMA_RESTARTED", "1");

            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                let err = cmd.exec();
                return Err(anyhow::anyhow!("Failed to re-exec TUI process: {}", err));
            }
            #[cfg(not(unix))]
            {
                let mut child = cmd
                    .stdin(std::process::Stdio::inherit())
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit())
                    .spawn()?;
                let status = child.wait()?;
                std::process::exit(status.code().unwrap_or(0));
            }
        } else {
            bail!(
                "Version mismatch: TUI version (v{}) is older than running bridge version (v{}). Please update TUI binary.",
                client_version,
                bridge_version
            );
        }
    }
}

fn spawn_server_process(exe: &std::path::Path, args: &[&str]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(exe);
        cmd.args(args);
        cmd.process_group(0);
        cmd.spawn()?;
    }

    #[cfg(not(unix))]
    {
        let mut cmd = std::process::Command::new(exe);
        cmd.args(args);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd.spawn()?;
    }
    Ok(())
}

/// Ensure a local server is running by probing available local transports.
/// If none is reachable, spawns a background `ahma serve unix` (on macOS/Linux)
/// or `ahma serve http` (on Windows) and polls until healthy.
pub async fn ensure_server_running() -> Result<()> {
    let client_version = env!("CARGO_PKG_VERSION");
    let candidates = default_candidates();
    for candidate in &candidates {
        if let Some(bridge_version) = get_candidate_version(candidate).await {
            if let Some(()) =
                handle_existing_candidate(candidate, client_version, bridge_version).await?
            {
                return Ok(());
            }
            break;
        }
    }

    let exe = std::env::current_exe()?;
    #[cfg(unix)]
    let args = ["serve", "unix"];
    #[cfg(not(unix))]
    let args = ["serve", "http"];

    tracing::info!("Spawning background server: {} {:?}", exe.display(), args);
    spawn_server_process(&exe, &args)?;

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        for candidate in &candidates {
            if probe_candidate(candidate).await {
                tracing::info!(
                    "Background server started and healthy at {}",
                    candidate.display_url
                );
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    bail!("Failed to start background server within 2 seconds")
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_candidates_unix_first() {
        let candidates = default_candidates();
        assert!(!candidates.is_empty(), "must have at least one candidate");

        #[cfg(unix)]
        {
            assert!(
                matches!(candidates[0].transport, ResolvedTransport::UnixSocket(_)),
                "first candidate on Unix must be a Unix socket"
            );
        }

        // Last candidate is always HTTP localhost
        let last = candidates.last().unwrap();
        assert!(
            matches!(last.transport, ResolvedTransport::Http(_)),
            "last candidate must be HTTP"
        );
    }

    #[test]
    fn default_candidates_non_unix_http_only() {
        // On Unix this test is weaker (both transports present), but on
        // non-Unix platforms there must be exactly one HTTP candidate.
        let candidates = default_candidates();
        let http_count = candidates
            .iter()
            .filter(|c| matches!(c.transport, ResolvedTransport::Http(_)))
            .count();
        assert!(http_count >= 1, "must include at least one HTTP candidate");
    }

    #[test]
    fn parse_candidate_http_url() {
        let c = parse_candidate("http://localhost:8080");
        assert!(matches!(c.transport, ResolvedTransport::Http(_)));
        assert_eq!(c.display_url, "http://localhost:8080");
    }

    #[test]
    fn parse_candidate_strips_fragment() {
        let c = parse_candidate("http://localhost:3000#/mcp");
        assert!(matches!(c.transport, ResolvedTransport::Http(_)));
        // fragment stripped
        assert!(!c.display_url.contains('#'));
    }

    #[cfg(unix)]
    #[test]
    fn parse_candidate_unix_url() {
        let c = parse_candidate("unix:///tmp/ahma.sock");
        assert!(matches!(c.transport, ResolvedTransport::UnixSocket(_)));
    }

    #[test]
    fn parse_status_2xx_ok() {
        assert!(parse_status_2xx(b"HTTP/1.0 200 OK\r\n"));
        assert!(parse_status_2xx(b"HTTP/1.1 204 No Content\r\n"));
    }

    #[test]
    fn parse_status_2xx_reject_non_2xx() {
        assert!(!parse_status_2xx(b"HTTP/1.1 404 Not Found\r\n"));
        assert!(!parse_status_2xx(b"HTTP/1.1 500 Internal Server Error\r\n"));
        assert!(!parse_status_2xx(b""));
        assert!(!parse_status_2xx(b"garbage"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_unix_socket_returns_false_for_missing_socket() {
        // A socket path that cannot exist
        let result = probe_unix_socket("/tmp/ahma_tui_test_nonexistent.sock").await;
        assert!(!result, "non-existent socket must return false");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_unix_socket_live_health() {
        use tokio::io::AsyncReadExt;

        // Spin up a minimal Unix socket server that returns "HTTP/1.0 200 OK"
        let tmp = tempfile::TempDir::new().unwrap();
        let socket_path = tmp.path().join("test.sock").to_string_lossy().into_owned();

        let socket_path_clone = socket_path.clone();
        let server = tokio::spawn(async move {
            let listener = tokio::net::UnixListener::bind(&socket_path_clone).unwrap();
            if let Ok((mut conn, _)) = listener.accept().await {
                // drain request
                let mut buf = [0u8; 512];
                let _ = tokio::time::timeout(Duration::from_millis(200), conn.read(&mut buf)).await;
                // send response
                use tokio::io::AsyncWriteExt;
                let _ = conn
                    .write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .await;
            }
        });

        // Give the server a moment to bind
        tokio::time::sleep(Duration::from_millis(50)).await;

        let result = probe_unix_socket(&socket_path).await;
        server.abort();
        assert!(result, "live socket returning 200 must probe as healthy");
    }

    // ─── Phase 2: HTTP/3 detection ────────────────────────────────────────────

    #[test]
    fn parse_h3_alt_svc_quoted_port() {
        let result = parse_h3_from_alt_svc(r#"h3=":3000""#);
        assert_eq!(result, Some("3000"));
    }

    #[test]
    fn parse_h3_alt_svc_multiple_values() {
        let result = parse_h3_from_alt_svc(r#"h2=":443", h3=":443""#);
        assert_eq!(result, Some("443"));
    }

    #[test]
    fn parse_h3_alt_svc_bare_token() {
        let result = parse_h3_from_alt_svc("h3");
        assert_eq!(result, Some(""));
    }

    #[test]
    fn parse_h3_alt_svc_h2_only() {
        let result = parse_h3_from_alt_svc(r#"h2=":443""#);
        assert!(result.is_none(), "h2-only Alt-Svc must not match h3");
    }

    #[test]
    fn parse_h3_alt_svc_empty() {
        let result = parse_h3_from_alt_svc("");
        assert!(result.is_none());
    }

    #[test]
    fn transport_label_http3_returns_quic_label() {
        let conn = ResolvedConnection {
            display_url: "http://localhost:3000".to_string(),
            transport: ResolvedTransport::Http3("http://localhost:3000".to_string()),
        };
        assert_eq!(conn.transport_label(), "HTTP/3 (QUIC)");
    }

    #[test]
    fn transport_label_http_returns_http() {
        let conn = ResolvedConnection {
            display_url: "http://localhost:3000".to_string(),
            transport: ResolvedTransport::Http("http://localhost:3000".to_string()),
        };
        assert_eq!(conn.transport_label(), "HTTP");
    }
}
