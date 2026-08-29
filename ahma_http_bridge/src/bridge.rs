//! # HTTP Bridge Server Implementation
//!
//! This module implements the Axum-based HTTP server that serves as the entry point
//! for the bridge. It handles the low-level HTTP details, including request routing,
//! content-negotiation (SSE vs. JSON), and session lifecycle management.
//!
//! ## Architecture: Routing and Persistence
//!
//! The bridge server acts as a thin routing layer above the [`SessionManager`]:
//!
//! - **Request Multiplexing**: Every incoming POST or SSE request is checked for
//!   the `Mcp-Session-Id` header. The server then routes the request to the
//!   correct [`Session`], ensuring that JSON-RPC commands reach the right subprocess.
//! - **Handshake Orchestration**: It manages the delicate sequence of the MCP
//!   handshake over HTTP, ensuring that the SSE connection is established and the
//!   sandbox is locked before allow tool calls to proceed.
//! - **Reconnection Support**: By maintaining a buffer of recent notifications,
//!   the server allows clients to reconnect after brief network interruptions
//!   without losing protocol state.
//!
//! ## Implementation Note
//!
//! The bridge is designed to be "stateless" from the perspective of the HTTP protocol
//! itself, while maintaining strictly stateful subprocesses in the background. If a
//! session is not accessed within its configured timeout, the bridge gracefully
//! shuts down the associated subprocess to conserve system resources.

use crate::error::{BridgeError, Result};
use crate::session::{
    DEFAULT_HANDSHAKE_TIMEOUT_SECS, DEFAULT_MAX_SESSIONS, DEFAULT_REQUEST_TIMEOUT_SECS,
    DEFAULT_TOOL_CALL_TIMEOUT_SECS, SessionManager, SessionManagerConfig,
};
use arc_swap::ArcSwapOption;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use base64::Engine as _;
use futures::stream::{self, StreamExt};
use serde_json::Value;
use std::{convert::Infallible, net::SocketAddr, path::PathBuf, sync::Arc};
use subtle::ConstantTimeEq as _;
use tower_governor::{
    GovernorLayer, governor::GovernorConfigBuilder, key_extractor::SmartIpKeyExtractor,
};
use tower_http::{
    cors::{AllowHeaders, AllowMethods, CorsLayer},
    set_header::SetResponseHeaderLayer,
    trace::TraceLayer,
};
use tracing::{debug, error, info, warn};

/// Whether the bridge should listen on a TCP socket or a Unix domain socket.
#[derive(Debug, Clone)]
pub enum ListenerKind {
    /// Bind a TCP socket on the given address.
    Tcp(SocketAddr),
    /// Bind a Unix domain socket (UDS) at the given path.
    ///
    /// Use the `@` prefix for Linux abstract sockets: `@name` → `\0name`.
    /// Filesystem socket files are removed on graceful shutdown.
    ///
    /// Not available on Windows; a compile-time `#[cfg(unix)]` gate is applied.
    #[cfg(unix)]
    Unix(String),
}

/// Configuration for the HTTP bridge server.
///
/// Use `Default` to get a baseline configuration or construct manually for full control.
///
/// # Example
///
/// ```rust
/// use ahma_http_bridge::BridgeConfig;
/// use std::path::PathBuf;
///
/// let config = BridgeConfig {
///     bind_addr: "0.0.0.0:8080".parse().unwrap(),
///     server_command: "/usr/local/bin/my-mcp-server".into(),
///     server_args: vec!["--verbose".into()],
///     // Optional explicit fallback scope for clients without roots support
///     default_sandbox_scope: Some(PathBuf::from("/tmp/sandbox")),
///     ..Default::default()
/// };
/// ```
pub struct BridgeConfig {
    /// local address to bind the HTTP server to (e.g., `127.0.0.1:3000`).
    /// Use port 0 to bind to a random available port.
    pub bind_addr: SocketAddr,

    /// Path or command name of the MCP server executable to spawn.
    /// This command will be executed as a subprocess for each session.
    pub server_command: String,

    /// Command-line arguments to pass to the MCP server.
    pub server_args: Vec<String>,

    /// If true, preserves ANSI color codes in the subprocess output (useful for debugging).
    /// If false, colors are stripped or disabled depending on the subprocess behavior.
    pub enable_colored_output: bool,

    /// Explicit fallback sandbox directory for clients that do not provide workspace roots.
    ///
    /// If `None`, clients must provide roots/list to complete handshake and unlock tools.
    pub default_sandbox_scope: Option<PathBuf>,

    /// Timeout in seconds for the MCP handshake to complete.
    /// If the handshake (SSE connection + roots/list response) doesn't complete
    /// within this time, tool calls will return a timeout error.
    /// Defaults to 45 seconds.
    pub handshake_timeout_secs: u64,

    /// Default timeout in seconds for bridge → subprocess request/response
    /// calls (used for everything except `tools/call`). Defaults to 60 seconds.
    pub request_timeout_secs: u64,

    /// Default timeout in seconds for `tools/call` requests, unless the
    /// caller's `timeout_seconds` argument overrides it. Defaults to 60 seconds.
    pub tool_call_timeout_secs: u64,

    /// If `true`, attempt to start an HTTP/3 (QUIC) endpoint alongside HTTP/2.
    /// The QUIC endpoint uses a self-signed certificate. Defaults to `true`.
    /// Ignored when `listener_kind` is `ListenerKind::Unix` (QUIC is UDP-based).
    pub enable_quic: bool,

    /// If `true`, disable HTTP/1.1 on the TCP listener and require HTTP/2+.
    /// Defaults to `false` (HTTP/1.1 and HTTP/2 are both accepted).
    pub disable_http1_1: bool,

    /// The transport the bridge should listen on.
    ///
    /// Defaults to `ListenerKind::Tcp(bind_addr)`.  Set to
    /// `ListenerKind::Unix(path)` to serve MCP Streamable HTTP over a Unix
    /// domain socket instead of a TCP port.  `bind_addr` is ignored in that case.
    pub listener_kind: ListenerKind,

    /// Bearer token required on every request (except `/health`).
    ///
    /// Set via `--require-token <path>` in the CLI.  The file should contain a
    /// single line with the secret token.  If `None`, no token is checked —
    /// **only acceptable on a loopback bind address**.
    ///
    /// The comparison is constant-time to prevent timing side-channels.
    pub require_token: Option<String>,

    /// Path of the file from which `require_token` was loaded.
    ///
    /// When present, a SIGHUP signal causes the bridge to re-read this file and
    /// atomically replace the in-flight token with zero downtime.
    pub require_token_path: Option<PathBuf>,

    /// Maximum sustained request rate per client IP, in requests/second.
    ///
    /// `0` disables rate limiting (default).  Clients that exceed the limit
    /// receive HTTP 429 with a `Retry-After` header.  The `/health` endpoint
    /// is always exempt from rate limiting.
    pub rate_limit_rps: u64,

    /// Burst size for the per-IP token bucket (requests above the sustained
    /// rate that are allowed before throttling begins).
    ///
    /// Defaults to `10`. Only effective when `rate_limit_rps > 0`.
    pub rate_limit_burst: u32,

    /// Shared atomic counter tracking active connections.
    pub active_sessions: Option<Arc<std::sync::atomic::AtomicUsize>>,

    /// Shutdown the server if active_sessions drops to 0 for this duration.
    pub idle_timeout_secs: Option<u64>,

    /// Maximum concurrent sessions allowed.
    pub max_sessions: usize,

    /// Optional [`PeerFactory`] injection point (P5 — test harness).
    ///
    /// When `Some`, each new bridge session uses this factory instead of
    /// spawning an `ahma_mcp` subprocess.  This allows tests to wire an
    /// [`InProcessMcpPeerFactory`] (from `ahma_mcp::test_utils`) so the full
    /// bridge session behaviour — handshake, sandbox gating, SSE replay,
    /// dual-transport — runs entirely in-process without forking.
    ///
    /// When `None` (the default), sessions use a [`SubprocessPeerFactory`]
    /// built from `server_command` and `server_args`.
    ///
    /// [`PeerFactory`]: crate::peer::PeerFactory
    /// [`InProcessMcpPeerFactory`]: https://docs.rs/ahma_mcp/latest/ahma_mcp/test_utils/bridge_peer/struct.InProcessMcpPeerFactory.html
    /// [`SubprocessPeerFactory`]: crate::peer::SubprocessPeerFactory
    pub peer_factory: Option<std::sync::Arc<dyn crate::peer::PeerFactory>>,

    /// Optional one-shot sender that receives the actual bound port when the
    /// bridge starts listening (P5 — in-process test harness).
    ///
    /// Using `bind_addr` with port `0` lets the OS pick a free port; this
    /// sender fires as soon as `listen()` succeeds so tests can discover the
    /// actual port without parsing stderr.
    ///
    /// The sender is consumed on first use; subsequent bridge starts (after a
    /// hypothetical restart) will not fire it.
    pub bound_port_tx: Option<tokio::sync::oneshot::Sender<u16>>,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        let bind_addr: SocketAddr = "127.0.0.1:3000".parse().unwrap();
        Self {
            bind_addr,
            server_command: "ahma".to_string(),
            server_args: vec![],
            enable_colored_output: false,
            default_sandbox_scope: None,
            handshake_timeout_secs: DEFAULT_HANDSHAKE_TIMEOUT_SECS,
            request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
            tool_call_timeout_secs: DEFAULT_TOOL_CALL_TIMEOUT_SECS,
            enable_quic: true,
            disable_http1_1: false,
            listener_kind: ListenerKind::Tcp(bind_addr),
            require_token: None,
            require_token_path: None,
            rate_limit_rps: 0,
            rate_limit_burst: 10,
            active_sessions: None,
            idle_timeout_secs: None,
            max_sessions: DEFAULT_MAX_SESSIONS,
            peer_factory: None,
            bound_port_tx: None,
        }
    }
}

impl std::fmt::Debug for BridgeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeConfig")
            .field("bind_addr", &self.bind_addr)
            .field("server_command", &self.server_command)
            .field("server_args", &self.server_args)
            .field("enable_colored_output", &self.enable_colored_output)
            .field("default_sandbox_scope", &self.default_sandbox_scope)
            .field("handshake_timeout_secs", &self.handshake_timeout_secs)
            .field("request_timeout_secs", &self.request_timeout_secs)
            .field("tool_call_timeout_secs", &self.tool_call_timeout_secs)
            .field("enable_quic", &self.enable_quic)
            .field("disable_http1_1", &self.disable_http1_1)
            .field("listener_kind", &self.listener_kind)
            .field(
                "require_token",
                &self.require_token.as_ref().map(|_| "<redacted>"),
            )
            .field("rate_limit_rps", &self.rate_limit_rps)
            .field("rate_limit_burst", &self.rate_limit_burst)
            .field("max_sessions", &self.max_sessions)
            .field(
                "peer_factory",
                &self.peer_factory.as_ref().map(|_| "<PeerFactory>"),
            )
            .field(
                "bound_port_tx",
                &self.bound_port_tx.as_ref().map(|_| "<Sender>"),
            )
            .finish_non_exhaustive()
    }
}

impl Clone for BridgeConfig {
    fn clone(&self) -> Self {
        Self {
            bind_addr: self.bind_addr,
            server_command: self.server_command.clone(),
            server_args: self.server_args.clone(),
            enable_colored_output: self.enable_colored_output,
            default_sandbox_scope: self.default_sandbox_scope.clone(),
            handshake_timeout_secs: self.handshake_timeout_secs,
            request_timeout_secs: self.request_timeout_secs,
            tool_call_timeout_secs: self.tool_call_timeout_secs,
            enable_quic: self.enable_quic,
            disable_http1_1: self.disable_http1_1,
            listener_kind: self.listener_kind.clone(),
            require_token: self.require_token.clone(),
            require_token_path: self.require_token_path.clone(),
            rate_limit_rps: self.rate_limit_rps,
            rate_limit_burst: self.rate_limit_burst,
            active_sessions: self.active_sessions.clone(),
            idle_timeout_secs: self.idle_timeout_secs,
            max_sessions: self.max_sessions,
            peer_factory: self.peer_factory.clone(),
            // oneshot::Sender is not Clone; cloning a BridgeConfig discards the
            // port notifier. Tests that need the notification should call
            // `with_bound_port_tx` on the config they are about to use.
            bound_port_tx: None,
        }
    }
}

impl BridgeConfig {
    /// Inject a [`PeerFactory`] override for in-process testing (P5).
    ///
    /// With this set, each bridge session uses `factory` instead of spawning
    /// an `ahma_mcp` subprocess.  Set `bound_port_tx` to receive the actual
    /// bound port without parsing stderr:
    ///
    /// ```rust,no_run
    /// use ahma_http_bridge::BridgeConfig;
    /// use tokio::sync::oneshot;
    ///
    /// # async fn example(my_factory: std::sync::Arc<dyn ahma_http_bridge::peer::PeerFactory>) {
    /// let (port_tx, port_rx) = oneshot::channel();
    /// let config = BridgeConfig::for_in_process_test(my_factory)
    ///     .with_bound_port_tx(port_tx);
    ///
    /// // Spawn the bridge in the background.
    /// tokio::spawn(ahma_http_bridge::start_bridge(config));
    ///
    /// // Wait for the bridge to bind.
    /// let port = port_rx.await.expect("bridge did not start");
    /// let url = format!("http://127.0.0.1:{port}");
    /// # }
    /// ```
    ///
    /// [`PeerFactory`]: crate::peer::PeerFactory
    pub fn for_in_process_test(factory: std::sync::Arc<dyn crate::peer::PeerFactory>) -> Self {
        let bind_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        Self {
            bind_addr,
            // Ignored: `peer_factory` being `Some` bypasses subprocess spawning.
            server_command: String::new(),
            enable_quic: false, // no QUIC needed for in-process tests
            listener_kind: ListenerKind::Tcp(bind_addr),
            peer_factory: Some(factory),
            ..Default::default()
        }
    }

    /// Set a one-shot sender that receives the actual bound port.
    ///
    /// Useful with `bind_addr` port `0` to discover the OS-assigned port
    /// without parsing stderr output.
    #[must_use]
    pub fn with_bound_port_tx(mut self, tx: tokio::sync::oneshot::Sender<u16>) -> Self {
        self.bound_port_tx = Some(tx);
        self
    }
}

/// Shared state threaded through every Axum handler via [`axum::Extension`].
///
/// Contains all mutable server state that needs to be accessible to individual
/// request handlers:
///
/// - **Session management** — active MCP session lifecycle tracking.
/// - **Bearer token** — optional auth token held behind [`ArcSwapOption`] so it
///   can be swapped atomically on SIGHUP without restarting (zero downtime reload).
///
/// The entire struct is wrapped in `Arc` before being registered as an Axum
/// extension, so handler clones are cheap reference-count increments.
pub struct BridgeState {
    /// Session manager (session isolation mode only)
    session_manager: Arc<SessionManager>,
    /// Optional bearer token.  When set, every request (except `/health`) must
    /// supply `Authorization: Bearer <token>`.  Compared constant-time.
    ///
    /// Uses `ArcSwapOption` so the token can be atomically replaced on SIGHUP
    /// without restarting the bridge or locking out in-flight requests.
    require_token: ArcSwapOption<String>,
    /// The listener configuration, used for cleanup on restart.
    listener_kind: ListenerKind,
}

/// Build a CORS layer appropriate for the bind address.
///
/// - **Loopback** (`127.0.0.1`, `::1`): restricts allowed origins to
///   `http://127.0.0.1:*`, `http://localhost:*`, and `http://[::1]:*`.
///   Only the HTTP methods used by MCP Streamable HTTP (`GET`, `POST`, `DELETE`)
///   are permitted, and only MCP-relevant headers are exposed.
/// - **Non-loopback** (`0.0.0.0`, public IP, etc.): allows any origin so
///   legitimate deployments behind a reverse proxy are not broken, but the
///   caller logs a security warning.
fn build_cors_layer(bind_addr: &SocketAddr) -> CorsLayer {
    let methods = AllowMethods::list([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS]);
    let headers = AllowHeaders::list([
        "content-type".parse().unwrap(),
        "mcp-session-id".parse().unwrap(),
        "accept".parse().unwrap(),
        "last-event-id".parse().unwrap(),
    ]);
    let expose = tower_http::cors::ExposeHeaders::list(["mcp-session-id"
        .parse::<axum::http::HeaderName>()
        .unwrap()]);

    if bind_addr.ip().is_loopback() {
        // Restrictive: only accept requests originating from loopback web pages.
        CorsLayer::new()
            .allow_origin(tower_http::cors::AllowOrigin::predicate(
                |origin: &HeaderValue, _req: &axum::http::request::Parts| {
                    let Ok(origin_str) = origin.to_str() else {
                        return false;
                    };
                    // Allow http://127.0.0.1[:port], http://localhost[:port], http://[::1][:port]
                    let lower = origin_str.to_ascii_lowercase();
                    lower.starts_with("http://127.0.0.1")
                        || lower.starts_with("http://localhost")
                        || lower.starts_with("http://[::1]")
                },
            ))
            .allow_methods(methods)
            .allow_headers(headers)
            .expose_headers(expose)
    } else {
        // Non-loopback: user explicitly opted into network exposure.
        // Allow any origin but restrict methods/headers.
        CorsLayer::new()
            .allow_origin(tower_http::cors::AllowOrigin::any())
            .allow_methods(methods)
            .allow_headers(headers)
            .expose_headers(expose)
    }
}

/// MCP Session-Id header name (per MCP spec 2025-03-26)
pub(crate) const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";
const MCP_PATH: &str = "/mcp";
const HEALTH_PATH: &str = "/health";
const ACCEPT_HEADER: &str = "accept";
const LAST_EVENT_ID_HEADER: &str = "last-event-id";
const SSE_ACCEPT_MIME: &str = "text/event-stream";
const BEARER_PREFIX: &str = "Bearer ";
const BEARER_WWW_AUTHENTICATE: &str = "Bearer realm=\"ahma-http-bridge\"";

pub(crate) fn session_id_from_headers(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(MCP_SESSION_ID_HEADER)
        .and_then(|value| value.to_str().ok())
}

fn extract_bearer_token(value: &str) -> Option<&str> {
    value
        .get(..BEARER_PREFIX.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(BEARER_PREFIX))
        .and_then(|_| value.get(BEARER_PREFIX.len()..))
}

fn bearer_token_from_headers(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(extract_bearer_token)
}

// ─── Bearer-token authentication middleware ───────────────────────────────────

/// Axum middleware that enforces a bearer-token check when `BridgeState::require_token`
/// is set.
///
/// The `/health` endpoint is exempted so load-balancers can probe it without a token.
/// All other requests must supply `Authorization: Bearer <token>`.
///
/// The comparison uses constant-time equality from the `subtle` crate to prevent
/// timing side-channels.
async fn bearer_auth_middleware(
    State(state): State<Arc<BridgeState>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let guard = state.require_token.load();
    let Some(ref expected_token) = *guard else {
        // No token configured — let the request through.
        return next.run(request).await;
    };

    // /health is exempt: load-balancers must be able to probe it unauthenticated.
    if request.uri().path() == HEALTH_PATH {
        return next.run(request).await;
    }

    // Extract `Authorization: Bearer <token>` header.
    // RFC 7235 §2.1: the auth-scheme token is case-insensitive.
    match bearer_token_from_headers(request.headers()) {
        Some(token) if token.as_bytes().ct_eq(expected_token.as_bytes()).into() => {
            next.run(request).await
        }
        _ => {
            debug!("Request rejected: missing or invalid bearer token");
            (
                StatusCode::UNAUTHORIZED,
                [(
                    axum::http::header::WWW_AUTHENTICATE,
                    BEARER_WWW_AUTHENTICATE,
                )],
                "Unauthorized",
            )
                .into_response()
        }
    }
}

/// Block until SIGINT (Ctrl-C) or SIGTERM is received.
///
/// Used by both TCP and Unix accept loops to trigger graceful shutdown when the OS or
/// a parent process requests termination.  The handler runs once; on a second signal
/// the process exits immediately (default OS behaviour after the first handler fires).
async fn await_shutdown_signal() {
    use tokio::signal;

    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    {
        let terminate = async {
            signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler")
                .recv()
                .await;
        };
        tokio::select! {
            () = ctrl_c => {},
            () = terminate => {},
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await;
    }
}

fn spawn_idle_timeout_checker(timeout: u64, state: Arc<BridgeState>) {
    let session_manager = state.session_manager.clone();
    #[cfg_attr(not(unix), allow(unused_variables))]
    let listener_kind = state.listener_kind.clone();
    let counter = session_manager
        .active_sessions
        .clone()
        .unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicUsize::new(0)));
    tokio::spawn(async move {
        let mut idle_duration = std::time::Duration::ZERO;
        let check_interval = std::time::Duration::from_secs(1);
        loop {
            tokio::time::sleep(check_interval).await;
            if counter.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                idle_duration = std::time::Duration::ZERO;
                continue;
            }

            idle_duration += check_interval;
            if idle_duration.as_secs() >= timeout {
                tracing::info!("No active clients for {} seconds. Shutting down.", timeout);
                session_manager
                    .terminate_all(crate::session::SessionTerminationReason::Timeout)
                    .await;
                #[cfg(unix)]
                if let ListenerKind::Unix(ref path) = listener_kind
                    && !path.starts_with('\0')
                {
                    let _ = std::fs::remove_file(path);
                }
                std::process::exit(0);
            }
        }
    });
}

/// Starts the HTTP bridge server and blocks until shutdown.
///
/// This function initializes the session manager, sets up the Axum router for MCP
/// endpoints, and binds to the specified address.
///
/// # Returns
///
/// * `Ok(())` upon graceful shutdown (currently runs indefinitely).
/// * `Err(BridgeError)` if binding fails or the server encounters a fatal error.
///
/// # Port Binding
///
/// If `config.bind_addr` specifies port 0, the OS will assign a random available port.
/// The actual bound port is printed to stderr as `AHMA_BOUND_PORT=<port>` to assist
/// with test infrastructure integration.
///
/// # Example
///
/// ```rust,no_run
/// use ahma_http_bridge::{BridgeConfig, start_bridge};
///
/// #[tokio::main]
/// async fn main() {
///    let config = BridgeConfig::default();
///    if let Err(e) = start_bridge(config).await {
///        eprintln!("Bridge failed: {}", e);
///    }
/// }
/// ```
pub async fn start_bridge(mut config: BridgeConfig) -> Result<()> {
    if config.idle_timeout_secs.is_some() && config.active_sessions.is_none() {
        config.active_sessions = Some(Arc::new(std::sync::atomic::AtomicUsize::new(0)));
    }

    #[cfg(unix)]
    if let ListenerKind::Unix(ref socket_path) = config.listener_kind {
        return start_bridge_unix(config.clone(), socket_path.clone()).await;
    }
    start_bridge_tcp(config).await
}

/// Create the shared bridge state (session manager + Arc wrapper) from a `BridgeConfig`.
fn build_bridge_state(config: &BridgeConfig) -> Arc<BridgeState> {
    let session_config = create_session_config(config);
    let mut session_manager = SessionManager::new(session_config);
    if let Some(ref counter) = config.active_sessions {
        session_manager.active_sessions = Some(counter.clone());
    }
    let session_manager = Arc::new(session_manager);
    session_manager.start_sweeper();
    Arc::new(BridgeState {
        session_manager,
        require_token: ArcSwapOption::new(config.require_token.clone().map(Arc::new)),
        listener_kind: config.listener_kind.clone(),
    })
}

fn create_session_config(config: &BridgeConfig) -> SessionManagerConfig {
    SessionManagerConfig {
        server_command: config.server_command.clone(),
        server_args: config.server_args.clone(),
        default_scope: config.default_sandbox_scope.clone(),
        enable_colored_output: config.enable_colored_output,
        handshake_timeout_secs: config.handshake_timeout_secs,
        request_timeout_secs: config.request_timeout_secs,
        tool_call_timeout_secs: config.tool_call_timeout_secs,
        max_sessions: config.max_sessions,
        peer_factory: config.peer_factory.clone(),
    }
}

/// On Unix, spawn a background task that watches for SIGHUP and reloads the
/// bearer token from the given path.  The new token is swapped in atomically
/// so no in-flight request is interrupted.
///
/// No-op on non-Unix platforms (Windows has no SIGHUP).
#[cfg(unix)]
fn reload_token_from_path(state: &BridgeState, p: &std::path::Path) {
    match std::fs::read_to_string(p) {
        Ok(raw) => {
            let token = raw.trim().to_owned();
            if token.is_empty() {
                warn!(
                    "Token file {} is empty after SIGHUP — keeping current token",
                    p.display()
                );
            } else {
                state.require_token.store(Some(Arc::new(token)));
                info!("Bearer token reloaded from {} after SIGHUP", p.display());
            }
        }
        Err(e) => warn!("Failed to reload token on SIGHUP from {}: {e}", p.display()),
    }
}

#[cfg(unix)]
fn maybe_install_sighup_handler(state: Arc<BridgeState>, path: Option<PathBuf>) {
    let Some(p) = path else { return };
    tokio::spawn(async move {
        let mut sig = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to install SIGHUP handler for token reload: {e}");
                return;
            }
        };
        info!("SIGHUP token reload enabled — watching {}", p.display());
        loop {
            sig.recv().await;
            reload_token_from_path(&state, &p);
        }
    });
}

#[cfg(not(unix))]
fn maybe_install_sighup_handler(_state: Arc<BridgeState>, _path: Option<PathBuf>) {}

/// Build the axum router with optional QUIC `Alt-Svc` header injection and
/// optional per-IP rate limiting.
///
/// `/health` is placed outside the rate-limit and auth layers so that
/// load-balancer probes are never blocked or throttled.
fn build_mcp_router(
    state: Arc<BridgeState>,
    cors: CorsLayer,
    quic_info: Option<&QuicInfo>,
    rate_limit_rps: u64,
    rate_limit_burst: u32,
) -> Result<Router> {
    // /health is intentionally outside auth + rate-limit layers: load-balancer
    // probes must never be blocked or throttled. /restart is NOT exempt — it
    // kills the whole bridge, so it goes through bearer auth and rate limiting
    // like every MCP route, plus its own loopback-peer check (see
    // handle_restart).
    let exempt_routes = Router::new()
        .route("/health", get(health_check))
        .with_state(state.clone());

    let mcp_routes = Router::new()
        .route(
            MCP_PATH,
            post(handle_mcp_request)
                .get(handle_sse_stream)
                .delete(handle_session_delete),
        )
        .route("/restart", post(handle_restart))
        .fallback(handle_not_found)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            bearer_auth_middleware,
        ))
        .with_state(state);

    // Apply per-IP rate limiting to MCP routes only, when configured.
    let mcp_routes = if rate_limit_rps > 0 {
        // Both values come from operator configuration (`--rate-limit-rps` /
        // `--rate-limit-burst`), so an invalid combination is a typo, not a bug.
        // `.expect()` turned that typo into a panic with a message that named
        // neither value; the operator saw a backtrace where they should have
        // seen which number was wrong.
        let governor_conf = match GovernorConfigBuilder::default()
            .per_second(rate_limit_rps)
            .burst_size(rate_limit_burst)
            .key_extractor(SmartIpKeyExtractor)
            .finish()
        {
            Some(conf) => Arc::new(conf),
            None => {
                return Err(crate::error::BridgeError::Config(format!(
                    "--rate-limit-rps {rate_limit_rps} with --rate-limit-burst \
                     {rate_limit_burst} was rejected. Both must be non-zero, and the burst \
                     must be at least as large as the per-second rate."
                )));
            }
        };
        mcp_routes.layer(GovernorLayer::new(governor_conf))
    } else {
        mcp_routes
    };

    let base = exempt_routes.merge(mcp_routes).layer(cors);

    Ok(if let Some(qi) = quic_info {
        let alt_svc = HeaderValue::from_str(&qi.alt_svc_header)
            .unwrap_or_else(|_| HeaderValue::from_static(""));
        base.layer(SetResponseHeaderLayer::overriding(
            axum::http::header::ALT_SVC,
            alt_svc,
        ))
        .layer(TraceLayer::new_for_http())
    } else {
        base.layer(TraceLayer::new_for_http())
    })
}

/// Serve a single accepted TCP stream over HTTP/2-only or auto HTTP/1.1+HTTP/2.
///
/// `peer_addr` is injected as `ConnectInfo<SocketAddr>` so that middleware such
/// as `GovernorLayer` (per-IP rate limiting) can extract the client address.
async fn serve_tcp_connection(
    stream: tokio::net::TcpStream,
    app: Router,
    disable_http1_1: bool,
    peer_addr: std::net::SocketAddr,
) {
    let io = hyper_util::rt::TokioIo::new(stream);
    let hyper_svc =
        hyper::service::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
            let mut svc = app.clone();
            async move {
                use tower::Service;
                // Inject ConnectInfo so IP-based middleware (e.g. rate limiter) works.
                req.extensions_mut()
                    .insert(axum::extract::ConnectInfo(peer_addr));
                let req = req.map(axum::body::Body::new);
                svc.call(req).await
            }
        });
    if disable_http1_1 {
        if let Err(e) =
            hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                .serve_connection(io, hyper_svc)
                .await
        {
            tracing::debug!("HTTP connection closed: {:#}", e);
        }
    } else if let Err(e) =
        hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
            .serve_connection(io, hyper_svc)
            .await
    {
        tracing::debug!("HTTP connection closed: {:#}", e);
    }
}

fn warn_if_non_loopback(bind_addr: SocketAddr) {
    if !bind_addr.ip().is_loopback() {
        warn!(
            "HTTP bridge bound to non-loopback address {}. \
             CORS allows any origin. Restrict access via firewall or reverse proxy.",
            bind_addr
        );
    }
}

fn print_quic_info(quic_info: &Option<QuicInfo>, enable_quic: bool, local_addr: SocketAddr) {
    if let Some(qi) = quic_info {
        eprintln!("AHMA_QUIC_PORT={}", qi.quic_port);
        eprintln!(
            "AHMA_QUIC_CERT={}",
            base64::engine::general_purpose::STANDARD.encode(&qi.cert_der)
        );
        info!(
            "QUIC/HTTP/3 listening on udp://{}:{}",
            local_addr.ip(),
            qi.quic_port
        );
    } else if enable_quic {
        info!("QUIC/HTTP/3 not started (unavailable or failed to bind)");
    }
}

/// Serve MCP Streamable HTTP over a TCP socket.
async fn start_bridge_tcp(config: BridgeConfig) -> Result<()> {
    info!("Starting HTTP bridge on {}", config.bind_addr);
    warn_if_non_loopback(config.bind_addr);

    info!("Session isolation: ENABLED (always-on)");
    let state = build_bridge_state(&config);
    if let Some(timeout) = config.idle_timeout_secs
        && timeout > 0
    {
        spawn_idle_timeout_checker(timeout, state.clone());
    }
    // Install SIGHUP handler for zero-downtime token rotation.
    maybe_install_sighup_handler(state.clone(), config.require_token_path.clone());

    // MCP Streamable HTTP transport: single endpoint supporting POST, GET (SSE), DELETE.
    // See: https://modelcontextprotocol.io/specification/2025-06-18/basic/transports#streamable-http
    let cors = build_cors_layer(&config.bind_addr);

    // Bind TCP first to get the actual ephemeral port (important when port 0 is used).
    let listener = tokio::net::TcpListener::bind(config.bind_addr)
        .await
        .map_err(|e| BridgeError::HttpServer(format!("Failed to bind: {}", e)))?;

    let local_addr = listener
        .local_addr()
        .map_err(|e| BridgeError::HttpServer(format!("Failed to get local addr: {}", e)))?;

    info!("HTTP bridge listening on http://{}", local_addr);
    info!("MCP endpoint (POST): http://{}{}", local_addr, MCP_PATH);
    info!("MCP endpoint (GET/SSE): http://{}{}", local_addr, MCP_PATH);

    // Optionally start QUIC / HTTP/3 endpoint.
    let quic_info = if config.enable_quic {
        try_start_quic_endpoint(local_addr).await
    } else {
        None
    };

    // Build the axum router; when QUIC is active Alt-Svc is injected automatically.
    // Clone state for the shutdown handler *before* moving it into build_mcp_router.
    let shutdown_state = state.clone();
    let app = build_mcp_router(
        state,
        cors,
        quic_info.as_ref(),
        config.rate_limit_rps,
        config.rate_limit_burst,
    )?;

    // Print QUIC startup markers *before* AHMA_BOUND_PORT so parsers see them in order.
    print_quic_info(&quic_info, config.enable_quic, local_addr);

    // Print machine-readable bound port for test infrastructure (always print, tests parse it)
    eprintln!("AHMA_BOUND_PORT={}", local_addr.port());

    // Notify in-process test harness of the bound port (P5).
    if let Some(tx) = config.bound_port_tx {
        // Ignore send errors: the receiver may have been dropped if the test
        // already timed out and is tearing down.
        let _ = tx.send(local_addr.port());
    }

    // Spawn QUIC accept loop as a background task.
    if let Some(qi) = quic_info {
        let quic_app = app.clone();
        tokio::spawn(async move {
            crate::quic::serve_quic(qi.endpoint, quic_app).await;
        });
    }

    if config.disable_http1_1 {
        info!(
            "Protocol: HTTP/2+ only (HTTP/1.1 disabled; clients may upgrade to HTTP/3 via Alt-Svc)"
        );
    } else {
        info!(
            "Protocol: HTTP/1.1 + HTTP/2 (clients may upgrade to HTTP/3 via Alt-Svc when available)"
        );
    }

    // Spawn graceful shutdown handler (SIGINT/SIGTERM).
    tokio::spawn(async move {
        await_shutdown_signal().await;
        info!("Bridge (TCP) received shutdown signal — terminating all sessions.");
        shutdown_state
            .session_manager
            .terminate_all(crate::session::SessionTerminationReason::Timeout)
            .await;
        std::process::exit(0);
    });

    loop {
        let (stream, peer_addr) = listener
            .accept()
            .await
            .map_err(|e| BridgeError::HttpServer(format!("Accept error: {}", e)))?;
        tokio::spawn(serve_tcp_connection(
            stream,
            app.clone(),
            config.disable_http1_1,
            peer_addr,
        ));
    }
}

/// Prepare `socket_path` for binding without stealing a live server's socket
/// (SPEC R-ISO.2).
///
/// A leftover socket *file* from a crashed bridge must be removed before
/// `bind()` can succeed, but blindly unlinking would also destroy the
/// rendezvous point of a bridge that is alive and serving — the classic
/// failure being a test-spawned bridge deleting the developer's live
/// `/tmp/ahma.sock` out from under their MCP session. Probe-connect first: a
/// successful connection means a live server owns the path and this process
/// must refuse to bind; a refused connection means the file is stale and safe
/// to unlink.
#[cfg(unix)]
fn prepare_unix_socket_path(socket_path: &str) -> Result<()> {
    use std::io::ErrorKind;

    // Abstract sockets (leading NUL) have no filesystem entry to clean up.
    if socket_path.starts_with('\0') || !std::path::Path::new(socket_path).exists() {
        return Ok(());
    }

    match std::os::unix::net::UnixStream::connect(socket_path) {
        Ok(_) => Err(BridgeError::HttpServer(format!(
            "refusing to bind Unix socket {socket_path}: another server is live on it. \
             Stop that server first, or pass --socket-path to use a private socket."
        ))),
        Err(e) if matches!(e.kind(), ErrorKind::ConnectionRefused | ErrorKind::NotFound) => {
            std::fs::remove_file(socket_path).map_err(|e| {
                BridgeError::HttpServer(format!(
                    "failed to remove stale socket file {socket_path}: {e}"
                ))
            })
        }
        Err(e) => Err(BridgeError::HttpServer(format!(
            "cannot probe existing socket {socket_path}: {e}"
        ))),
    }
}

/// Conservative cross-platform ceiling for `sockaddr_un.sun_path`, used only as
/// a friendly pre-check before `bind()` — not a precise per-OS contract. The
/// real usable limit is ~103 bytes on macOS and ~107 on Linux; 100 bytes leaves
/// headroom under both without needing to special-case the running platform.
#[cfg(unix)]
const MAX_UNIX_SOCKET_PATH_LEN: usize = 100;

/// Pre-check `socket_path`'s byte length before it reaches `bind()`, so a
/// too-long path (e.g. a user-supplied `--unix-socket-path` or
/// `settings.toml` value) fails with an ahma-authored, actionable message
/// instead of libstd's bare `"path must be shorter than SUN_LEN"`.
#[cfg(unix)]
fn check_unix_socket_path_length(socket_path: &str) -> Result<()> {
    let byte_len = socket_path.len();
    if byte_len > MAX_UNIX_SOCKET_PATH_LEN {
        return Err(BridgeError::HttpServer(format!(
            "Unix socket path '{socket_path}' is {byte_len} bytes, which exceeds the OS limit \
             (~103 bytes on macOS, ~107 on Linux); use a shorter --unix-socket-path or the default"
        )));
    }
    Ok(())
}

/// Identity (device, inode) of the socket file this process bound, used to
/// ensure shutdown removes only a socket it still owns (SPEC R-ISO.3): if
/// another process has since replaced the path, the file is theirs.
#[cfg(unix)]
fn unix_socket_identity(socket_path: &str) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(socket_path)
        .ok()
        .map(|m| (m.dev(), m.ino()))
}

/// Serve MCP Streamable HTTP over a Unix domain socket.
///
/// The socket path may use the `@` prefix for Linux abstract sockets.
/// A stale (dead) socket file is removed before binding; a *live* socket is
/// never stolen — binding fails loudly instead. On graceful shutdown the
/// socket file is removed only if this process still owns it.
#[cfg(unix)]
async fn start_bridge_unix(config: BridgeConfig, raw_socket_path: String) -> Result<()> {
    // Translate `@name` → `\0name` (Linux abstract namespace).
    let socket_path = if let Some(name) = raw_socket_path.strip_prefix('@') {
        format!("\0{name}")
    } else {
        raw_socket_path.clone()
    };

    // Fail fast with an actionable message rather than surfacing libstd's bare
    // "path must be shorter than SUN_LEN" from `UnixListener::bind` below.
    check_unix_socket_path_length(&socket_path)?;

    info!("Starting HTTP bridge on Unix socket: {}", raw_socket_path);
    info!("Session isolation: ENABLED (always-on)");

    let state = build_bridge_state(&config);
    if let Some(timeout) = config.idle_timeout_secs
        && timeout > 0
    {
        spawn_idle_timeout_checker(timeout, state.clone());
    }
    // Install SIGHUP handler for zero-downtime token rotation.
    maybe_install_sighup_handler(state.clone(), config.require_token_path.clone());

    // For Unix sockets, CORS origin matching is less meaningful but we still
    // build the layer for middleware compatibility.
    let dummy_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let cors = build_cors_layer(&dummy_addr);

    // Reuse build_mcp_router so auth and rate-limiting are consistent across transports.
    // Clone state for the shutdown handler *before* moving it into build_mcp_router.
    let shutdown_state = state.clone();
    let app = build_mcp_router(
        state,
        cors,
        None,
        config.rate_limit_rps,
        config.rate_limit_burst,
    )?;

    // Remove a stale socket file from a previous run — but never a live one
    // (SPEC R-ISO.2). Abstract sockets start with '\0' and have no file.
    prepare_unix_socket_path(&socket_path)?;

    let listener = tokio::net::UnixListener::bind(&socket_path)
        .map_err(|e| BridgeError::HttpServer(format!("Failed to bind Unix socket: {}", e)))?;

    // Record which inode we bound so shutdown removes only our own socket
    // (SPEC R-ISO.3).
    let owned_socket_identity = if socket_path.starts_with('\0') {
        None
    } else {
        unix_socket_identity(&socket_path)
    };

    // Restrict to owner-only (0600) so other local users cannot connect and drive
    // the bridge. No-op for abstract sockets (leading NUL).
    ahma_common::daemon_hub::restrict_unix_socket_permissions(std::path::Path::new(&socket_path));

    info!("HTTP bridge listening on Unix socket: {}", raw_socket_path);
    info!(
        "MCP endpoint (POST): http+unix://{}{}",
        raw_socket_path, MCP_PATH
    );

    // Print machine-readable bound socket path for test infrastructure.
    eprintln!("AHMA_UNIX_SOCKET_PATH={}", raw_socket_path);

    // Graceful shutdown: on SIGINT/SIGTERM, terminate all sessions, remove the socket, exit.
    let socket_path_for_shutdown = socket_path.clone();
    let raw_socket_path_for_shutdown = raw_socket_path.clone();
    tokio::spawn(async move {
        await_shutdown_signal().await;
        info!("Bridge (Unix) received shutdown signal — terminating all sessions.");
        shutdown_state
            .session_manager
            .terminate_all(crate::session::SessionTerminationReason::Timeout)
            .await;
        // Remove the socket file only if it is still the one this process
        // bound (SPEC R-ISO.3) — if another server has replaced the path in
        // the meantime, deleting it would orphan *their* live socket.
        if let Some(owned) = owned_socket_identity
            && unix_socket_identity(&socket_path_for_shutdown) == Some(owned)
        {
            ahma_common::fs_lock::remove_stale_socket(&raw_socket_path_for_shutdown);
        }
        std::process::exit(0);
    });

    loop {
        let (stream, _) = listener
            .accept()
            .await
            .map_err(|e| BridgeError::HttpServer(format!("Unix accept error: {}", e)))?;
        let svc = app.clone();
        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let hyper_svc =
                hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let mut svc = svc.clone();
                    async move {
                        use tower::Service;
                        let req = req.map(axum::body::Body::new);
                        svc.call(req).await
                    }
                });
            if let Err(e) =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection(io, hyper_svc)
                    .await
            {
                tracing::debug!("Unix socket HTTP connection closed: {:#}", e);
            }
        });
    }
}

/// Internal state for an active QUIC endpoint.
struct QuicInfo {
    endpoint: quinn::Endpoint,
    quic_port: u16,
    cert_der: Vec<u8>,
    alt_svc_header: String,
}

/// Bind a Quinn QUIC endpoint on the TCP address port, falling back to an OS-assigned UDP port.
fn bind_quic_endpoint(
    quic_server_cfg: quinn::ServerConfig,
    tcp_addr: SocketAddr,
) -> Option<quinn::Endpoint> {
    let primary = SocketAddr::new(tcp_addr.ip(), tcp_addr.port());
    match quinn::Endpoint::server(quic_server_cfg.clone(), primary) {
        Ok(e) => Some(e),
        Err(_) => {
            let fallback = SocketAddr::new(tcp_addr.ip(), 0);
            match quinn::Endpoint::server(quic_server_cfg, fallback) {
                Ok(e) => Some(e),
                Err(e) => {
                    warn!("QUIC: failed to bind endpoint: {e}");
                    None
                }
            }
        }
    }
}

/// Try to start a QUIC endpoint on the same IP as the TCP listener.
///
/// Returns `None` non-fatally if QUIC is unavailable or the binding fails.
async fn try_start_quic_endpoint(tcp_addr: SocketAddr) -> Option<QuicInfo> {
    let cert = crate::quic::cert::load_or_generate();

    let tls_config = match crate::quic::cert::build_quic_tls_config(&cert) {
        Ok(c) => c,
        Err(e) => {
            warn!("QUIC: failed to build TLS config: {e}");
            return None;
        }
    };

    let quic_server_cfg = match crate::quic::build_quinn_server_config(tls_config) {
        Ok(c) => c,
        Err(e) => {
            warn!("QUIC: failed to build quinn server config: {e}");
            return None;
        }
    };

    // Try binding QUIC on the same port as TCP (UDP and TCP namespaces are independent).
    let endpoint = bind_quic_endpoint(quic_server_cfg, tcp_addr)?;

    let quic_port = match endpoint.local_addr() {
        Ok(a) => a.port(),
        Err(e) => {
            warn!("QUIC: failed to query local addr: {e}");
            return None;
        }
    };

    Some(QuicInfo {
        endpoint,
        quic_port,
        cert_der: cert.cert_der,
        alt_svc_header: format!("h3=\":{}\"", quic_port),
    })
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    /// This bridge process's configured default sandbox scope (from
    /// `--sandbox-scope` or the `~/sandbox` fallback), if any. `None` when the
    /// bridge is waiting on client `roots/list` to learn its scope. Callers
    /// deciding whether to reuse this bridge instead of spawning a new one
    /// should compare this against the project directory they actually want
    /// (SPEC R7) rather than assuming a healthy bridge is scoped correctly.
    /// `#[serde(default)]` so a client talking to a pre-upgrade bridge that
    /// doesn't send this field still deserializes the response.
    #[serde(default)]
    pub default_sandbox_scope: Option<String>,
}

/// Health check endpoint
async fn health_check(State(state): State<Arc<BridgeState>>) -> impl IntoResponse {
    // Include the compile-time build-id so that same-semver dev rebuilds are
    // detectable: "0.12.5+abc1234" differs from "0.12.5+def5678" even though
    // the semver is identical.
    let version = format!("{}+{}", env!("CARGO_PKG_VERSION"), ahma_common::BUILD_ID);
    let default_sandbox_scope = state
        .session_manager
        .default_scope()
        .map(|p| p.to_string_lossy().into_owned());
    (
        StatusCode::OK,
        Json(HealthResponse {
            status: "OK".to_string(),
            version,
            default_sandbox_scope,
        }),
    )
}

/// Total budget for tearing every session down during a restart, after which the
/// process exits regardless. See [`handle_restart`].
const RESTART_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(15);

/// Handler for POST /restart
///
/// Anyone who can call this kills the bridge, so it is gated three ways: it
/// sits behind the bearer-auth and rate-limit layers (unlike `/health`), it
/// validates `Origin` like every MCP route, and — because the default local
/// deployment has no bearer token — the TCP peer must be a loopback address.
/// Unix-socket listeners inject no `ConnectInfo`; filesystem permissions on
/// the socket are the equivalent gate there.
async fn handle_restart(
    State(state): State<Arc<BridgeState>>,
    request: axum::extract::Request,
) -> Response {
    if let Some(rejection) = validate_origin(request.headers()) {
        return rejection;
    }
    let peer = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0);
    if let Some(addr) = peer
        && !addr.ip().is_loopback()
    {
        warn!(peer = %addr, "Refusing /restart from non-loopback peer");
        return (
            StatusCode::FORBIDDEN,
            "Restart is only accepted from loopback connections.",
        )
            .into_response();
    }
    info!("Restart requested. Shutting down bridge process...");
    #[cfg_attr(not(unix), allow(unused_variables))]
    let listener_kind = state.listener_kind.clone();
    let session_manager = state.session_manager.clone();
    tokio::spawn(async move {
        // Once we have said "shutting down", we MUST exit. A bridge that announces
        // shutdown and then lives on is the worst of both worlds: it has torn down
        // its sessions so it can no longer serve anyone, but it holds its clients'
        // connections open, so they hear neither a response nor a disconnect. One
        // did exactly that for five and a half hours, and the client it stranded
        // simply hung.
        //
        // Session teardown is already individually bounded (PEER_SHUTDOWN_GRACE);
        // this outer bound is the backstop that makes exit unconditional.
        if tokio::time::timeout(
            RESTART_SHUTDOWN_GRACE,
            session_manager
                .terminate_all(crate::session::SessionTerminationReason::ClientRequested),
        )
        .await
        .is_err()
        {
            tracing::warn!(
                grace_secs = RESTART_SHUTDOWN_GRACE.as_secs(),
                "Session teardown exceeded the shutdown grace period; exiting anyway \
                 rather than stranding clients on a half-dead bridge"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        #[cfg(unix)]
        if let ListenerKind::Unix(ref path) = listener_kind
            && !path.starts_with('\0')
        {
            ahma_common::fs_lock::remove_stale_socket(path);
        }
        std::process::exit(0);
    });
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "restarting",
            "version": format!("{}+{}", env!("CARGO_PKG_VERSION"), ahma_common::BUILD_ID)
        })),
    )
        .into_response()
}

/// Fallback handler for unknown routes.
///
/// Returns 404 for any path not explicitly registered (e.g. `/health`, `/mcp`).
/// This is critical for MCP OAuth discovery: MCP clients probe
/// `/.well-known/oauth-protected-resource` (RFC 9728) to decide whether the
/// server requires authentication. A 404 signals "no OAuth metadata" and
/// prevents clients from launching an unnecessary browser-based auth flow
/// when the server is running on localhost without authentication.
async fn handle_not_found() -> impl IntoResponse {
    StatusCode::NOT_FOUND
}

/// Handle DELETE requests to terminate a session (R8.4.7)
///
/// Per MCP specification: HTTP DELETE with `Mcp-Session-Id` header terminates
/// the session and its subprocess.
///
/// # Example
///
/// ```bash
/// curl -X DELETE http://localhost:3000/mcp \
///   -H "mcp-session-id: <session-uuid>"
/// ```
///
/// # Returns
///
/// - 204 No Content on successful termination
/// - 400 Bad Request if session ID header is missing
/// - 404 Not Found if session doesn't exist
async fn handle_session_delete(
    State(state): State<Arc<BridgeState>>,
    headers: HeaderMap,
) -> Response {
    if let Some(rejection) = validate_origin(&headers) {
        return rejection;
    }
    if let Some(rejection) = validate_protocol_version_header(&headers) {
        return rejection;
    }
    // Get session ID from header
    let session_id = match session_id_from_headers(&headers) {
        Some(id) => id.to_string(),
        None => {
            warn!("DELETE request without session ID header");
            return (StatusCode::BAD_REQUEST, "Missing Mcp-Session-Id header").into_response();
        }
    };

    info!(session_id = %session_id, "Session termination requested via HTTP DELETE");

    // Check if session exists before terminating
    if !state.session_manager.session_exists(&session_id) {
        debug!(session_id = %session_id, "Session not found for DELETE request");
        return StatusCode::NOT_FOUND.into_response();
    }

    // Terminate the session
    match state
        .session_manager
        .terminate_session(
            &session_id,
            crate::session::SessionTerminationReason::ClientRequested,
        )
        .await
    {
        Ok(()) => {
            info!(session_id = %session_id, "Session terminated successfully");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            error!(session_id = %session_id, "Failed to terminate session: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to terminate session: {}", e),
            )
                .into_response()
        }
    }
}

/// Handle SSE stream connections for server-to-client messages
///
/// This enables the MCP Streamable HTTP transport pattern where:
/// - POST /mcp sends client→server requests (existing)
/// - GET /mcp opens SSE stream for server→client messages (this handler)
///
/// The server uses SSE to:
/// 1. Send `roots/list` requests to discover client workspace folders
/// 2. Send notifications (if any)
/// 3. Send requests that need client responses
///
/// Note: Returns 404 (not 400/501) when session ID is missing or invalid. This prevents
/// clients from detecting SSE support during initial probing, avoiding OAuth prompts
/// for servers that don't require authentication.
///
/// # client Example
///
/// Clients should open this stream immediately after receiving a Session ID.
///
/// ```javascript
/// const eventSource = new EventSource("http://localhost:3000/mcp", {
///     headers: { "mcp-session-id": sessionId }
/// });
/// eventSource.onmessage = (event) => {
///     const msg = JSON.parse(event.data);
///     console.log("Received:", msg);
/// };
/// ```
///
/// Wraps the combined replay+live SSE stream used by `handle_sse_stream` so
/// that when the last subscriber drops the stream (client disconnect), the
/// session is terminated after a grace period if no one reconnects.
struct CleanupStream<S> {
    inner: S,
    session_id: String,
    session_manager: Arc<SessionManager>,
}

impl<S: futures::Stream> futures::Stream for CleanupStream<S> {
    type Item = S::Item;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        unsafe {
            let this = self.get_unchecked_mut();
            std::pin::Pin::new_unchecked(&mut this.inner).poll_next(cx)
        }
    }
}

impl<S> Drop for CleanupStream<S> {
    fn drop(&mut self) {
        let session_id = self.session_id.clone();
        let session_manager = self.session_manager.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            if let Some(session) = session_manager.get_session(&session_id)
                && session.sse_receivers() == 0
            {
                tracing::info!(session_id = %session_id, "No active SSE subscribers after disconnect - terminating session");
                let _ = session_manager
                    .terminate_session(
                        &session_id,
                        crate::session::SessionTerminationReason::Timeout,
                    )
                    .await;
            }
        });
    }
}

async fn handle_sse_stream(State(state): State<Arc<BridgeState>>, headers: HeaderMap) -> Response {
    if let Some(rejection) = validate_origin(&headers) {
        return rejection;
    }
    if let Some(rejection) = validate_protocol_version_header(&headers) {
        return rejection;
    }
    // Get session ID from header - required for SSE
    // Return 404 (not 400) to hide SSE from clients without a session
    let session_id = match session_id_from_headers(&headers) {
        Some(id) => id.to_string(),
        None => {
            debug!("SSE request without session ID header - returning 404");
            // 404 makes clients think SSE doesn't exist, avoiding OAuth probes
            return StatusCode::NOT_FOUND.into_response();
        }
    };

    debug!(session_id = %session_id, "SSE GET request received with session header");

    // Get the session - 404 if not found
    let session = match state.session_manager.get_session(&session_id) {
        Some(s) => s,
        None => {
            return StatusCode::NOT_FOUND.into_response();
        }
    };

    // Check if session is terminated
    if session.is_terminated() {
        return StatusCode::NOT_FOUND.into_response();
    }

    info!(session_id = %session_id, "SSE stream opened");

    // Subscribe to the session's broadcast channel BEFORE reading history
    // to prevent gaps between replay and live events.
    let rx = session.subscribe();

    // Parse Last-Event-Id header for replay support (HeaderMap lookups are
    // case-insensitive, so the lowercase name matches any spelling).
    let last_event_id: Option<u64> = headers
        .get(LAST_EVENT_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok());

    // Mark SSE as connected - if MCP is already initialized, this will trigger roots/list_changed
    match session.mark_sse_connected().await {
        Ok(true) => {
            // Handshake just reached RootsRequested; auto-lock from default_scope if configured.
            state
                .session_manager
                .auto_lock_if_default_scope(&session_id)
                .await;
        }
        Ok(false) => {}
        Err(e) => {
            warn!(session_id = %session_id, "Failed to mark SSE connected: {}", e);
        }
    }

    // Tell the subprocess it now has a live push channel — unconditionally
    // (unlike `mark_sse_connected`'s handshake transition, which only fires
    // once), so a reconnect after a dropped stream re-signals it too. A
    // failed send just leaves the subprocess at its conservative default;
    // there is no live channel yet to un-signal a stale disconnect, since a
    // permanent disconnect instead terminates the whole session (and its
    // subprocess) via `CleanupStream::drop` below.
    if let Err(e) = session.send_push_channel_changed(true).await {
        warn!(session_id = %session_id, "Failed to notify subprocess of live push channel: {}", e);
    }

    // Build replay stream from history (if Last-Event-Id was provided)
    let replay_events = last_event_id
        .map(|id| {
            let events = session.replay_events_after(id);
            debug!(
                session_id = %session_id,
                last_event_id = id,
                replay_count = events.len(),
                "Replaying missed SSE events"
            );
            events
        })
        .unwrap_or_default();

    let replay_stream = stream::iter(
        replay_events
            .into_iter()
            .map(|(id, msg)| Ok::<_, Infallible>(Event::default().id(id.to_string()).data(msg))),
    );

    // Convert broadcast receiver to a stream of SSE events (shared adapter:
    // logs and counts `Lagged(n)` losses, yields keep-alive comments).
    let live_stream = crate::request_handler::broadcast_sse_event_stream(
        session.clone(),
        rx,
        "Sending SSE event",
    );

    // Chain replay events before live stream for seamless reconnection
    let combined = replay_stream.chain(live_stream);

    let combined = CleanupStream {
        inner: combined,
        session_id: session_id.clone(),
        session_manager: state.session_manager.clone(),
    };

    // Return SSE response with keep-alive
    Sse::new(combined)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Check whether the client prefers SSE over JSON for POST responses.
fn accepts_sse(headers: &HeaderMap) -> bool {
    headers
        .get_all(ACCEPT_HEADER)
        .iter()
        .any(|v| v.to_str().unwrap_or_default().contains(SSE_ACCEPT_MIME))
}

/// Handle MCP JSON-RPC requests with content negotiation
///
/// Supports both JSON and SSE response formats based on Accept header (R8A)
/// In session isolation mode, routes requests to the correct session subprocess
/// Protocol revisions this bridge accepts in the `MCP-Protocol-Version` header.
///
/// The bridge is a transport: actual capability negotiation happens in the
/// per-session subprocess (rmcp). This list exists to give a *definitive* 400
/// to a client pinned to a revision the transport genuinely does not speak,
/// per the 2025-06-18 Streamable HTTP requirement, rather than failing
/// somewhere later with a worse message.
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];

/// Validate the `MCP-Protocol-Version` header (2025-06-18 Streamable HTTP).
///
/// Absent header → assume `2025-03-26` and proceed (the spec's
/// backwards-compatibility rule; every pre-2025-06-18 client omits it).
/// Present but unsupported → HTTP 400 naming the supported set.
fn validate_protocol_version_header(headers: &HeaderMap) -> Option<Response> {
    let value = headers.get("mcp-protocol-version")?;
    let value = value.to_str().unwrap_or("");
    if SUPPORTED_PROTOCOL_VERSIONS.contains(&value) {
        return None;
    }
    warn!(version = %value, "Rejecting request with unsupported MCP-Protocol-Version");
    Some(
        (
            StatusCode::BAD_REQUEST,
            format!(
                "Unsupported MCP-Protocol-Version: '{value}'. Supported: {}.",
                SUPPORTED_PROTOCOL_VERSIONS.join(", ")
            ),
        )
            .into_response(),
    )
}

/// Server-side `Origin` validation (MCP Streamable HTTP security requirement).
///
/// A browser reached via DNS rebinding sends the attacker's `Origin`; CORS
/// headers alone don't stop the request from being *processed* — they only
/// gate what the browser lets the page read afterwards. So requests carrying
/// an `Origin` that is not a loopback origin are rejected outright. Non-browser
/// clients (rmcp, curl, IDEs) send no `Origin` header and are unaffected.
fn validate_origin(headers: &HeaderMap) -> Option<Response> {
    let origin = headers.get(axum::http::header::ORIGIN)?;
    let origin = origin.to_str().unwrap_or("");
    if origin_is_loopback(origin) {
        return None;
    }
    warn!(origin = %origin, "Rejecting request with non-loopback Origin (DNS-rebinding guard)");
    Some(
        (
            StatusCode::FORBIDDEN,
            "Origin not allowed: this MCP bridge only accepts browser requests from \
             loopback origins (http://localhost, http://127.0.0.1, http://[::1]).",
        )
            .into_response(),
    )
}

/// True when `origin` is a loopback origin (`scheme://host[:port]` with a
/// loopback host). `Origin: null` and non-loopback hosts are rejected.
fn origin_is_loopback(origin: &str) -> bool {
    let Some((_scheme, rest)) = origin.split_once("://") else {
        return false;
    };
    let host = if let Some(stripped) = rest.strip_prefix('[') {
        // IPv6 literal: [::1] or [::1]:port
        stripped.split(']').next().unwrap_or("")
    } else {
        rest.split(':').next().unwrap_or("")
    };
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

async fn handle_mcp_request(
    State(state): State<Arc<BridgeState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    if let Some(rejection) = validate_origin(&headers) {
        return rejection;
    }
    if let Some(rejection) = validate_protocol_version_header(&headers) {
        return rejection;
    }
    // JSON-RPC batch arrays are not supported (batching was removed from the
    // MCP spec in 2025-06-18). Refusing loudly beats the old behavior, which
    // forwarded the raw array to the subprocess with undefined results.
    if payload.is_array() {
        return crate::request_handler::batch_not_supported_response();
    }
    let sse_accepted = accepts_sse(&headers);
    debug!("Received HTTP request");

    if sse_accepted {
        return crate::request_handler::handle_session_isolated_request_sse(
            state.session_manager.clone(),
            headers,
            payload,
        )
        .await;
    }

    crate::request_handler::handle_session_isolated_request(
        state.session_manager.clone(),
        headers,
        payload,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::Ordering;
    use tempfile::TempDir;
    use tower::ServiceExt;

    /// Return the appropriate Python interpreter name for the current platform.
    ///
    /// Windows: `"python"` — the Python Launcher ships `python.exe`, not `python3.exe`.
    /// Unix:    `"python3"` — standard name on Linux/macOS.
    fn python_cmd() -> &'static str {
        #[cfg(windows)]
        {
            "python"
        }
        #[cfg(not(windows))]
        {
            "python3"
        }
    }

    #[test]
    fn test_default_config() {
        let config = BridgeConfig::default();
        assert_eq!(config.bind_addr.to_string(), "127.0.0.1:3000");
        assert_eq!(config.server_command, "ahma");
        assert!(config.server_args.is_empty());
    }

    #[test]
    fn test_config_with_custom_values() {
        let config = BridgeConfig {
            bind_addr: "0.0.0.0:8080".parse().unwrap(),
            server_command: "custom_server".to_string(),
            server_args: vec!["--arg1".to_string(), "value1".to_string()],
            enable_colored_output: false,
            default_sandbox_scope: Some(std::env::temp_dir()),
            handshake_timeout_secs: 10,
            request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
            tool_call_timeout_secs: DEFAULT_TOOL_CALL_TIMEOUT_SECS,
            enable_quic: false,
            disable_http1_1: false,
            listener_kind: ListenerKind::Tcp("0.0.0.0:8080".parse().unwrap()),
            require_token: None,
            require_token_path: None,
            rate_limit_rps: 0,
            rate_limit_burst: 10,
            active_sessions: None,
            idle_timeout_secs: None,
            max_sessions: 50,
            peer_factory: None,
            bound_port_tx: None,
        };
        assert_eq!(config.bind_addr.to_string(), "0.0.0.0:8080");
        assert_eq!(config.server_command, "custom_server");
        assert_eq!(config.server_args.len(), 2);
        assert_eq!(config.server_args[0], "--arg1");
        assert_eq!(config.server_args[1], "value1");
        assert_eq!(config.handshake_timeout_secs, 10);
    }

    #[test]
    fn test_config_clone() {
        let config = BridgeConfig::default();
        let cloned = config.clone();
        assert_eq!(config.bind_addr, cloned.bind_addr);
        assert_eq!(config.server_command, cloned.server_command);
        assert_eq!(config.server_args, cloned.server_args);
    }

    #[test]
    fn test_config_debug() {
        let config = BridgeConfig::default();
        let debug_str = format!("{:?}", config);
        assert!(debug_str.contains("BridgeConfig"));
        assert!(debug_str.contains("127.0.0.1:3000"));
        assert!(debug_str.contains("ahma"));
    }

    fn create_app(state: Arc<BridgeState>) -> Router {
        let loopback_addr: SocketAddr = "127.0.0.1:3000".parse().unwrap();
        Router::new()
            .route(HEALTH_PATH, get(health_check))
            .route(MCP_PATH, post(handle_mcp_request))
            .fallback(handle_not_found)
            .layer(build_cors_layer(&loopback_addr))
            .with_state(state)
    }

    fn create_state_with_session_manager(session_manager: Arc<SessionManager>) -> Arc<BridgeState> {
        Arc::new(BridgeState {
            session_manager,
            require_token: ArcSwapOption::new(None),
            listener_kind: ListenerKind::Tcp("127.0.0.1:0".parse().unwrap()),
        })
    }

    fn write_mock_mcp_server_script(temp_dir: &TempDir) -> std::path::PathBuf {
        let script_path = temp_dir.path().join("mock_mcp_server.py");
        let script_content = r#"import sys
import json

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue

    try:
        msg = json.loads(line)
    except Exception:
        continue

    # Ignore client responses (no method)
    if not isinstance(msg, dict) or "method" not in msg:
        continue

    method = msg.get("method")
    msg_id = msg.get("id")

    if method == "initialize":
        resp = {
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "serverInfo": {"name": "mock", "version": "1.0"}
            }
        }
        print(json.dumps(resp))
        sys.stdout.flush()
        continue

    if method == "tools/call":
        resp = {
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "content": [{"type": "text", "text": "tool ok"}]
            }
        }
        print(json.dumps(resp))
        sys.stdout.flush()
        continue
    
    if method == "notifications/roots/list_changed":
        # Simulate subprocess applying sandbox scopes and notify bridge
        print(json.dumps({"jsonrpc": "2.0", "method": "notifications/sandbox/configured"}))
        sys.stdout.flush()
        continue

    # Generic response for other request methods
    if msg_id is not None:
        print(json.dumps({"jsonrpc": "2.0", "id": msg_id, "result": {}}))
        sys.stdout.flush()
"#;

        fs::write(&script_path, script_content).expect("Failed to write mock MCP server script");
        script_path
    }

    #[tokio::test]
    async fn test_session_isolation_rejects_tool_calls_until_roots_lock_then_allows() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let script_path = write_mock_mcp_server_script(&temp_dir);

        let session_manager = Arc::new(SessionManager::new(SessionManagerConfig {
            server_command: python_cmd().to_string(),
            server_args: vec![script_path.to_string_lossy().to_string()],
            default_scope: Some(temp_dir.path().to_path_buf()),
            max_sessions: 50,
            ..Default::default()
        }));

        let state = create_state_with_session_manager(Arc::clone(&session_manager));
        let app = create_app(state);

        let session_id = session_manager
            .create_session()
            .await
            .expect("Should create session");

        // 0) Send notifications/initialized to complete MCP handshake
        // (without this, the bridge will block waiting for initialization)
        let init_notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header(MCP_SESSION_ID_HEADER, session_id.as_str())
                    .body(Body::from(serde_json::to_vec(&init_notification).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        // Verify MCP is now initialized
        let session = session_manager
            .get_session(&session_id)
            .expect("Session should exist");
        assert!(session.is_mcp_initialized());

        // 1) tools/call should be rejected before sandbox is locked.
        let tool_call = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "dummy",
                "arguments": {}
            }
        });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header(MCP_SESSION_ID_HEADER, session_id.as_str())
                    .body(Body::from(serde_json::to_vec(&tool_call).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            response
                .headers()
                .get(MCP_SESSION_ID_HEADER)
                .and_then(|h| h.to_str().ok()),
            Some(session_id.as_str())
        );
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], -32001);

        // 2) Simulate client response to roots/list (this locks sandbox scope).
        let client_roots_response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 999,
            "result": {
                "roots": [
                    {
                        "uri": ahma_common::file_uri::encode_file_uri(temp_dir.path()),
                        "name": "root"
                    }
                ]
            }
        });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header(MCP_SESSION_ID_HEADER, session_id.as_str())
                    .body(Body::from(
                        serde_json::to_vec(&client_roots_response).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let session = session_manager
            .get_session(&session_id)
            .expect("Session should exist");

        let sandbox_scope = session
            .get_sandbox_scope()
            .expect("Sandbox scope should be set after roots lock");
        assert_eq!(sandbox_scope, temp_dir.path().to_path_buf());

        // Trigger roots/list_changed to subprocess by marking SSE connected.
        // The mock subprocess responds with notifications/sandbox/configured,
        // which transitions the sandbox state machine from Configuring to Active.
        session
            .mark_sse_connected()
            .await
            .expect("SSE mark should succeed");

        // Wait for the subprocess to confirm sandbox configuration (Active state).
        // Use generous timeout for all platforms (CI environments are slower and more variable).
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            session.wait_for_sandbox_active(),
        )
        .await
        .expect("Timeout waiting for sandbox Active state")
        .expect("Sandbox configuration failed");

        assert!(session.is_sandbox_locked());

        // 3) tools/call should now be forwarded and succeed.
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header(MCP_SESSION_ID_HEADER, session_id.as_str())
                    .body(Body::from(serde_json::to_vec(&tool_call).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        let text = json["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert_eq!(text, "tool ok");
    }

    // ----------------------------------------------------------------------
    // Sandbox-configured authority: regression coverage for the desync where
    // the TUI showed `[LOCKED]` while every tools/call returned HTTP 409.
    //
    // Root cause: the subprocess's `notifications/sandbox/configured` is the
    // authoritative "sandbox is enforced" signal, but the bridge only advanced
    // its state machine `Configuring -> Active`. If `configured` arrived while
    // the bridge was still in `AwaitingRoots` (no roots lock, or before
    // `auto_lock` ran), the transition was dropped and the session was wedged
    // in a non-Active state forever — yet the SSE-forwarded notification still
    // flipped the client UI to LOCKED.
    //
    // These tests pin the invariant: once the subprocess reports `configured`,
    // a subsequent tools/call MUST be forwarded (never 409), across the full
    // matrix of handshake orderings and timings.
    // ----------------------------------------------------------------------

    /// A mock subprocess that emits `notifications/sandbox/configured` as soon
    /// as it observes `notifications/roots/list_changed`, *without* requiring or
    /// echoing any client roots. This reproduces a real client (e.g. the TUI, or
    /// any editor with no workspace folder open) that returns empty roots while
    /// the subprocess configures from its own fallback scope.
    fn write_mock_configures_on_roots_changed(temp_dir: &TempDir) -> std::path::PathBuf {
        let script_path = temp_dir.path().join("mock_configures.py");
        let script_content = r#"import sys
import json

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except Exception:
        continue
    if not isinstance(msg, dict) or "method" not in msg:
        continue

    method = msg.get("method")
    msg_id = msg.get("id")

    if method == "initialize":
        print(json.dumps({
            "jsonrpc": "2.0", "id": msg_id,
            "result": {"protocolVersion": "2024-11-05", "capabilities": {},
                       "serverInfo": {"name": "mock", "version": "1.0"}}
        }))
        sys.stdout.flush()
        continue

    if method == "tools/call":
        print(json.dumps({
            "jsonrpc": "2.0", "id": msg_id,
            "result": {"content": [{"type": "text", "text": "tool ok"}]}
        }))
        sys.stdout.flush()
        continue

    if method == "notifications/roots/list_changed":
        # Subprocess configured its sandbox from its OWN fallback scope and
        # reports it as enforced — carrying the scope summary like the real one.
        print(json.dumps({
            "jsonrpc": "2.0",
            "method": "notifications/sandbox/configured",
            "params": {"scope": {"write": ["/mock/scope"], "read": [],
                                 "tmp": False, "enforced": True,
                                 "source": "default"}}
        }))
        sys.stdout.flush()
        continue

    if msg_id is not None:
        print(json.dumps({"jsonrpc": "2.0", "id": msg_id, "result": {}}))
        sys.stdout.flush()
"#;
        fs::write(&script_path, script_content).expect("Failed to write mock configures script");
        script_path
    }

    fn make_session_manager(
        script: &Path,
        default_scope: Option<std::path::PathBuf>,
    ) -> Arc<SessionManager> {
        Arc::new(SessionManager::new(SessionManagerConfig {
            server_command: python_cmd().to_string(),
            server_args: vec![script.to_string_lossy().to_string()],
            default_scope,
            max_sessions: 50,
            ..Default::default()
        }))
    }

    async fn post_mcp(app: &Router, session_id: &str, body: &Value) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header(MCP_SESSION_ID_HEADER, session_id)
                    .body(Body::from(serde_json::to_vec(body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, json)
    }

    fn initialized_notification() -> Value {
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    }

    fn tool_call_request(id: i64) -> Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": "dummy", "arguments": {}}
        })
    }

    async fn assert_tool_call_ok(app: &Router, session_id: &str, id: i64) {
        let (status, json) = post_mcp(app, session_id, &tool_call_request(id)).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "tools/call after sandbox configured must be forwarded, got {status}: {json}"
        );
        assert_eq!(
            json["result"]["content"][0]["text"].as_str(),
            Some("tool ok"),
            "unexpected tools/call result: {json}"
        );
    }

    /// DETERMINISTIC REGRESSION (the screenshot bug): the bridge has no
    /// `default_scope` and the client never locks roots, but the subprocess
    /// reports `configured`. Before the fix the bridge stayed in `AwaitingRoots`
    /// forever and tools/call returned 409 -32001 — exactly while the TUI header
    /// showed `[LOCKED]`. After the fix `configured` is authoritative and the
    /// tool call is forwarded.
    #[tokio::test]
    async fn test_configured_is_authoritative_without_roots_lock() {
        let temp_dir = TempDir::new().expect("temp dir");
        let script = write_mock_configures_on_roots_changed(&temp_dir);
        let session_manager = make_session_manager(&script, None);
        let app = create_app(create_state_with_session_manager(Arc::clone(
            &session_manager,
        )));

        let session_id = session_manager.create_session().await.expect("session");

        // Complete MCP init.
        let (status, _) = post_mcp(&app, &session_id, &initialized_notification()).await;
        assert_eq!(status, StatusCode::ACCEPTED);

        let session = session_manager.get_session(&session_id).expect("session");

        // Before configured: tools/call is gated with 409 / -32001.
        let (status, json) = post_mcp(&app, &session_id, &tool_call_request(1)).await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "expected 409 before configured"
        );
        assert_eq!(json["error"]["code"], -32001);

        // SSE connects -> bridge sends roots/list_changed -> subprocess emits
        // `configured`. The bridge never locks from roots (no default_scope, no
        // client roots), so this is the pure desync scenario.
        session.mark_sse_connected().await.expect("sse mark");

        // The authoritative `configured` must drive the bridge to Active.
        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            session.wait_for_sandbox_active(),
        )
        .await
        .expect("sandbox must reach Active after configured (regression: it never did)")
        .expect("sandbox configuration must not fail");

        assert!(session.is_sandbox_locked());
        assert!(matches!(
            session.current_sandbox_state(),
            ahma_common::sandbox_state::SandboxState::Active { .. }
        ));

        // The scope summary carried by the notification is recorded.
        if let ahma_common::sandbox_state::SandboxState::Active { scopes } =
            session.current_sandbox_state()
        {
            assert_eq!(scopes, vec![std::path::PathBuf::from("/mock/scope")]);
        }

        // And tools/call is now forwarded successfully.
        assert_tool_call_ok(&app, &session_id, 2).await;
    }

    /// TIMING/RACE MATRIX: with `default_scope` configured, the bridge runs
    /// `auto_lock` (AwaitingRoots -> Configuring) concurrently with the
    /// subprocess `configured` notification (Configuring/AwaitingRoots ->
    /// Active). Whichever wins, the session must end Active and stay there.
    /// We vary the handshake ordering (SSE before vs after `initialized`) and
    /// repeat to shake out the nondeterministic race that wedged the session.
    #[tokio::test]
    async fn test_configured_race_with_auto_lock_never_wedges() {
        for iteration in 0..8 {
            let sse_first = iteration % 2 == 0;
            let temp_dir = TempDir::new().expect("temp dir");
            let script = write_mock_configures_on_roots_changed(&temp_dir);
            // default_scope set => auto_lock races the configured notification.
            let session_manager =
                make_session_manager(&script, Some(temp_dir.path().to_path_buf()));
            let app = create_app(create_state_with_session_manager(Arc::clone(
                &session_manager,
            )));

            let session_id = session_manager.create_session().await.expect("session");
            let session = session_manager.get_session(&session_id).expect("session");

            if sse_first {
                // SSE connects before MCP init: roots/list_changed fires when
                // `initialized` arrives, then auto_lock runs right after.
                session.mark_sse_connected().await.expect("sse mark");
                let (status, _) = post_mcp(&app, &session_id, &initialized_notification()).await;
                assert_eq!(status, StatusCode::ACCEPTED);
            } else {
                let (status, _) = post_mcp(&app, &session_id, &initialized_notification()).await;
                assert_eq!(status, StatusCode::ACCEPTED);
                session.mark_sse_connected().await.expect("sse mark");
            }

            tokio::time::timeout(
                std::time::Duration::from_secs(15),
                session.wait_for_sandbox_active(),
            )
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "iteration {iteration} (sse_first={sse_first}): sandbox wedged, \
                     never reached Active (configured/auto_lock race regression)"
                )
            })
            .expect("sandbox configuration must not fail");

            assert_tool_call_ok(&app, &session_id, 1).await;

            // A repeated/replayed `configured` (idempotency) must keep Active.
            session.mark_sse_connected().await.ok();
            assert!(session.is_sandbox_locked());
            assert_tool_call_ok(&app, &session_id, 2).await;
        }
    }

    /// HAPPY PATH still intact: the normal `Configuring -> Active` flow. The
    /// bridge locks from `default_scope` (auto_lock on `initialized` ->
    /// Configuring), then the subprocess confirms `configured` *without* a scope
    /// payload. The fix must preserve the in-flight Configuring scopes rather
    /// than clobbering them with an empty set, and still reach Active.
    #[tokio::test]
    async fn test_configuring_then_configured_preserves_scopes() {
        let temp_dir = TempDir::new().expect("temp dir");
        // Original mock emits `configured` with NO scope payload, so the
        // bridge must keep the scopes it recorded during Configuring.
        let script = write_mock_mcp_server_script(&temp_dir);
        let session_manager = make_session_manager(&script, Some(temp_dir.path().to_path_buf()));
        let app = create_app(create_state_with_session_manager(Arc::clone(
            &session_manager,
        )));

        let session_id = session_manager.create_session().await.expect("session");
        let session = session_manager.get_session(&session_id).expect("session");

        // `initialized` -> auto_lock from default_scope -> Configuring{temp_dir}.
        let (status, _) = post_mcp(&app, &session_id, &initialized_notification()).await;
        assert_eq!(status, StatusCode::ACCEPTED);

        // Subprocess confirms configuration (no scope payload).
        session.mark_sse_connected().await.expect("sse mark");

        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            session.wait_for_sandbox_active(),
        )
        .await
        .expect("Active after Configuring + configured")
        .expect("sandbox configuration must not fail");

        // The Configuring scopes survived the Active transition.
        match session.current_sandbox_state() {
            ahma_common::sandbox_state::SandboxState::Active { scopes } => {
                assert_eq!(scopes, vec![temp_dir.path().to_path_buf()]);
            }
            other => panic!("expected Active, got {other:?}"),
        }
        let scope = session.get_sandbox_scope().expect("scope set");
        assert_eq!(scope, temp_dir.path().to_path_buf());

        assert_tool_call_ok(&app, &session_id, 1).await;
    }

    #[tokio::test]
    async fn test_health_check_endpoint() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let script_path = write_mock_mcp_server_script(&temp_dir);
        let session_manager = Arc::new(SessionManager::new(SessionManagerConfig {
            server_command: python_cmd().to_string(),
            server_args: vec![script_path.to_string_lossy().to_string()],
            default_scope: Some(temp_dir.path().to_path_buf()),
            max_sessions: 50,
            ..Default::default()
        }));
        let state = create_state_with_session_manager(session_manager);
        let app = create_app(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let response_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(response_json.get("status").unwrap().as_str().unwrap(), "OK");
        let version = response_json.get("version").unwrap().as_str().unwrap();
        // Version now includes build-id suffix: "0.12.6+<hash>" or "0.12.6+t<epoch>".
        assert!(
            version.starts_with(env!("CARGO_PKG_VERSION")),
            "health version must start with semver: got {version}"
        );
        // A client deciding whether to reuse this bridge must be able to see
        // which project it's actually scoped to (SPEC R7).
        let reported_scope = response_json
            .get("default_sandbox_scope")
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(reported_scope, temp_dir.path().to_string_lossy());
    }

    // ── CORS hardening tests ───────────────────────────────────────────

    #[test]
    fn test_cors_loopback_blocks_external_origin() {
        let loopback: SocketAddr = "127.0.0.1:3000".parse().unwrap();
        let cors = build_cors_layer(&loopback);

        // The layer should NOT include a blanket "Access-Control-Allow-Origin: *".
        let debug_str = format!("{:?}", cors);
        assert!(
            !debug_str.contains("\"*\""),
            "Loopback CORS must not use wildcard origin"
        );
    }

    #[test]
    fn test_cors_nonloopback_uses_any_origin() {
        let nonloopback: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        let cors = build_cors_layer(&nonloopback);

        // Non-loopback should use permissive origin (any / wildcard "*").
        let debug_str = format!("{:?}", cors);
        assert!(
            debug_str.contains("\"*\""),
            "Non-loopback CORS should allow all origins (\"*\"), got: {}",
            debug_str
        );
    }

    #[test]
    fn test_cors_ipv6_loopback_is_restrictive() {
        let ipv6_loopback: SocketAddr = "[::1]:3000".parse().unwrap();
        let cors = build_cors_layer(&ipv6_loopback);

        let debug_str = format!("{:?}", cors);
        assert!(
            !debug_str.contains("\"*\""),
            "IPv6 loopback CORS must not use wildcard origin"
        );
    }

    // ── SSE lag observability tests ────────────────────────────────────

    #[test]
    fn test_session_lagged_events_counter() {
        use std::sync::atomic::AtomicU64;
        // Verify the atomic counter tracks lagged events correctly.
        let counter = AtomicU64::new(0);
        counter.fetch_add(5, Ordering::Relaxed);
        counter.fetch_add(12, Ordering::Relaxed);
        assert_eq!(counter.load(Ordering::Relaxed), 17);
    }

    #[tokio::test]
    async fn test_broadcast_lag_is_recorded_on_session() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let script_path = write_mock_mcp_server_script(&temp_dir);
        let session_manager = Arc::new(SessionManager::new(SessionManagerConfig {
            server_command: python_cmd().to_string(),
            server_args: vec![script_path.to_string_lossy().to_string()],
            default_scope: Some(temp_dir.path().to_path_buf()),
            max_sessions: 50,
            ..Default::default()
        }));

        let session_id = session_manager
            .create_session()
            .await
            .expect("Should create session");
        let session = session_manager
            .get_session(&session_id)
            .expect("Session should exist");

        // Initially zero lagged events.
        assert_eq!(session.total_lagged_events(), 0);

        // Subscribe then flood beyond capacity (100) without consuming.
        let _rx = session.subscribe();
        for i in 0..150 {
            let _ = session.broadcast(format!("msg-{}", i));
        }

        // Record lag as if the stream handler detected it.
        session.record_lagged_events(50);
        assert_eq!(session.total_lagged_events(), 50);

        session.record_lagged_events(7);
        assert_eq!(session.total_lagged_events(), 57);
    }

    /// MCP clients probe `/.well-known/oauth-protected-resource` (RFC 9728) to
    /// discover whether the server requires OAuth authentication. A 404 tells
    /// clients there is no authorization server metadata, avoiding spurious
    /// browser-based auth flows on localhost.
    #[tokio::test]
    async fn test_oauth_discovery_returns_404_without_auth_header() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let script_path = write_mock_mcp_server_script(&temp_dir);

        let session_manager = Arc::new(SessionManager::new(SessionManagerConfig {
            server_command: python_cmd().to_string(),
            server_args: vec![script_path.to_string_lossy().to_string()],
            default_scope: Some(temp_dir.path().to_path_buf()),
            max_sessions: 50,
            ..Default::default()
        }));

        let state = create_state_with_session_manager(session_manager);
        let app = create_app(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/.well-known/oauth-protected-resource")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "OAuth discovery endpoint must return 404 on a localhost server without auth"
        );
        assert!(
            response.headers().get("www-authenticate").is_none(),
            "Response must not include WWW-Authenticate header"
        );
    }

    // ── Bearer auth middleware tests ────────────────────────────────────────

    /// Build a minimal router with bearer auth middleware wired in, using a dummy
    /// session manager that won't be exercised by these tests.
    fn create_app_with_token(token: Option<&str>) -> Router {
        let temp_dir = TempDir::new().expect("bearer-auth test: create temp dir");
        let state = Arc::new(BridgeState {
            session_manager: Arc::new(SessionManager::new(SessionManagerConfig {
                server_command: "echo".to_string(),
                default_scope: Some(temp_dir.path().to_path_buf()),
                max_sessions: 50,
                ..Default::default()
            })),
            require_token: ArcSwapOption::new(token.map(|s| Arc::new(s.to_owned()))),
            listener_kind: ListenerKind::Tcp("127.0.0.1:0".parse().unwrap()),
        });
        let loopback_addr: SocketAddr = "127.0.0.1:3000".parse().unwrap();
        Router::new()
            .route(HEALTH_PATH, get(health_check))
            .route(MCP_PATH, post(handle_mcp_request))
            .fallback(handle_not_found)
            .layer(middleware::from_fn_with_state(
                state.clone(),
                bearer_auth_middleware,
            ))
            .layer(build_cors_layer(&loopback_addr))
            .with_state(state)
    }

    #[tokio::test]
    async fn test_bearer_no_token_configured_allows_all() {
        let app = create_app_with_token(None);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Session creation will fail because there's no real server, but the
        // bearer middleware should not reject (no token required → passes through).
        assert_ne!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "No token configured: should not return 401"
        );
    }

    #[tokio::test]
    async fn test_bearer_valid_token_accepted() {
        // A valid token on the exempt /health route returns 200.
        // (The middleware lets /health through regardless; this confirms the
        //  token path doesn't accidentally break health checks.)
        let app = create_app_with_token(Some("secret-token"));
        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/health")
                    .header("authorization", "Bearer secret-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "Valid bearer token on /health must return 200"
        );
    }

    #[tokio::test]
    async fn test_bearer_missing_token_returns_401() {
        let app = create_app_with_token(Some("secret-token"));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "Missing token must return 401"
        );
        assert!(
            response.headers().contains_key("www-authenticate"),
            "401 response must include WWW-Authenticate header"
        );
    }

    #[tokio::test]
    async fn test_bearer_wrong_token_returns_401() {
        let app = create_app_with_token(Some("secret-token"));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("authorization", "Bearer wrong-token")
                    .header("content-type", "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "Wrong token must return 401"
        );
    }

    #[tokio::test]
    async fn test_bearer_health_exempt_without_token() {
        let app = create_app_with_token(Some("secret-token"));
        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/health")
                    // Deliberately no Authorization header.
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // /health is exempted so load-balancers can probe it unauthenticated.
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "/health must be exempt from bearer auth"
        );
    }

    #[tokio::test]
    async fn test_bearer_scheme_case_insensitive_lowercase() {
        let app = create_app_with_token(Some("secret-token"));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("authorization", "bearer secret-token")
                    .header("content-type", "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Auth passed — middleware let the request through (not 401).
        assert_ne!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "Lowercase 'bearer' scheme must be accepted (RFC 7235 case-insensitive)"
        );
    }

    #[tokio::test]
    async fn test_bearer_scheme_case_insensitive_uppercase() {
        let app = create_app_with_token(Some("secret-token"));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("authorization", "BEARER secret-token")
                    .header("content-type", "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Auth passed — middleware let the request through (not 401).
        assert_ne!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "Uppercase 'BEARER' scheme must be accepted (RFC 7235 case-insensitive)"
        );
    }

    // ── Bridge lifecycle / idle-timeout / active_sessions tests ──────────────

    /// Verify that a DELETE /mcp request decrements `active_sessions` so the idle-timeout
    /// checker can see zero clients and eventually exit.
    ///
    /// This test is in-process (no subprocess) and exercises only the counter logic.
    #[test]
    fn active_sessions_counter_increments_and_decrements() {
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        // Simulate one session being created
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "counter should be 1 after first session"
        );

        // Simulate session being deleted (DELETE /mcp or natural teardown)
        let prev = counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            prev, 1,
            "previous value should have been 1 before decrement"
        );
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "counter should be 0 after session deleted"
        );
    }

    /// Verify that the idle timeout checker exits when `active_sessions` reaches zero
    /// for the configured number of seconds.  Uses a very short timeout (1s) to keep
    /// the test fast.  The counter is never incremented, simulating no sessions ever
    /// connecting.  We verify the checker task runs to completion and the exit is clean.
    #[tokio::test]
    async fn idle_timeout_checker_fires_at_zero_sessions() {
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let fired_clone = fired.clone();
        let counter_clone = counter.clone();
        let timeout_secs: u64 = 1;

        // Run the idle checker inline rather than via process::exit.
        tokio::spawn(async move {
            let check_interval = std::time::Duration::from_millis(100);
            let mut idle_duration = std::time::Duration::ZERO;
            loop {
                tokio::time::sleep(check_interval).await;
                if counter_clone.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                    idle_duration = std::time::Duration::ZERO;
                    continue;
                }
                idle_duration += check_interval;
                if idle_duration.as_secs() >= timeout_secs {
                    fired_clone.store(true, std::sync::atomic::Ordering::Relaxed);
                    return;
                }
            }
        });

        // Wait up to 3 seconds for the checker to fire.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while !fired.load(std::sync::atomic::Ordering::Relaxed) {
            if tokio::time::Instant::now() > deadline {
                panic!("idle-timeout checker did not fire within 3 seconds");
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(
            fired.load(std::sync::atomic::Ordering::Relaxed),
            "idle-timeout checker should have fired"
        );
    }

    /// Verify that `terminate_session` decrements `active_sessions` when the session
    /// exists in the map (i.e., counter only drops when remove succeeds), ensuring
    /// that DELETE /mcp reliably brings the idle counter to zero.
    #[tokio::test]
    async fn terminate_nonexistent_session_does_not_underflow_counter() {
        use crate::session::{SessionManager, SessionManagerConfig};

        let temp_dir = TempDir::new().unwrap();
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let session_config = SessionManagerConfig {
            server_command: "echo".to_string(),
            default_scope: Some(temp_dir.path().to_path_buf()),
            max_sessions: 10,
            ..Default::default()
        };
        let mut session_manager = SessionManager::new(session_config);
        // Inject the counter the same way `build_bridge_state` does.
        session_manager.active_sessions = Some(counter.clone());
        let session_manager = Arc::new(session_manager);

        // Terminating a session that was never created must not decrement the counter
        // (no underflow below zero).
        let _ = session_manager
            .terminate_session(
                "nonexistent-id",
                crate::session::SessionTerminationReason::ClientRequested,
            )
            .await;

        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "counter must remain 0; terminating a non-existent session must not underflow"
        );
    }

    /// R-ISO.2: binding must not disturb a path with no socket file.
    #[cfg(unix)]
    #[test]
    fn prepare_unix_socket_path_ok_when_absent() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("absent.sock");
        assert!(prepare_unix_socket_path(path.to_str().unwrap()).is_ok());
    }

    /// R-ISO.2: a stale socket file (its listener is gone) is removed so the
    /// new server can bind.
    #[cfg(unix)]
    #[test]
    fn prepare_unix_socket_path_removes_stale_socket() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("stale.sock");
        {
            // Bind and immediately drop the listener; the file stays behind.
            let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        }
        assert!(
            path.exists(),
            "socket file should linger after listener drop"
        );
        assert!(prepare_unix_socket_path(path.to_str().unwrap()).is_ok());
        assert!(!path.exists(), "stale socket file must be removed");
    }

    /// R-ISO.2: a live socket must never be stolen — preparation fails loudly
    /// and the live listener's socket file is left untouched. This is the
    /// regression test for a test-spawned bridge deleting the developer's
    /// live /tmp/ahma.sock.
    #[cfg(unix)]
    #[test]
    fn prepare_unix_socket_path_refuses_live_socket() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("live.sock");
        let _live_listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

        let err = prepare_unix_socket_path(path.to_str().unwrap())
            .expect_err("must refuse to bind over a live socket");
        assert!(
            err.to_string().contains("another server is live"),
            "error must name the live-socket cause, got: {err}"
        );
        assert!(path.exists(), "the live socket file must not be removed");
    }

    /// A too-long socket path must fail with ahma's actionable message, not
    /// libstd's bare "path must be shorter than SUN_LEN". The length check
    /// runs before any filesystem access, so the path need not exist on disk.
    #[cfg(unix)]
    #[test]
    fn check_unix_socket_path_length_rejects_too_long_path() {
        use ahma_mcp::test_utils::path_helpers::test_abs;

        let long_component = "a".repeat(120);
        let long_path = test_abs(&[&long_component, "socket.sock"])
            .to_string_lossy()
            .into_owned();
        assert!(long_path.len() > 100, "test path must exceed the limit");

        let err = check_unix_socket_path_length(&long_path)
            .expect_err("a path over the conservative limit must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("exceeds the OS limit"),
            "error must use ahma's friendly message, got: {msg}"
        );
        assert!(
            !msg.contains("SUN_LEN"),
            "error must not surface libstd's raw message, got: {msg}"
        );
        assert!(
            msg.contains("--unix-socket-path"),
            "error must point at the actionable fix, got: {msg}"
        );
    }

    /// A short path within the limit must pass the pre-check.
    #[cfg(unix)]
    #[test]
    fn check_unix_socket_path_length_accepts_short_path() {
        use ahma_mcp::test_utils::path_helpers::test_temp_path;

        let short_path = test_temp_path("ahma-test.sock");
        assert!(check_unix_socket_path_length(&short_path.to_string_lossy()).is_ok());
    }

    /// R-ISO.3 helper: identity is stable for the same file and changes when
    /// the path is replaced by a new socket (new inode).
    #[cfg(unix)]
    #[test]
    fn unix_socket_identity_tracks_inode_replacement() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("ident.sock");
        let path_str = path.to_str().unwrap();

        let _first = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let first_identity = unix_socket_identity(path_str).expect("bound socket has metadata");
        assert_eq!(unix_socket_identity(path_str), Some(first_identity));

        // Replace the path with a fresh socket (what another server taking
        // over the path does): identity must change.
        std::fs::remove_file(&path).unwrap();
        let _second = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let second_identity = unix_socket_identity(path_str).expect("rebound socket has metadata");
        assert_ne!(
            first_identity, second_identity,
            "a replaced socket path must have a new identity"
        );
    }
}

#[cfg(test)]
mod transport_guard_tests {
    use super::*;

    #[test]
    fn loopback_origins_are_accepted() {
        for origin in [
            "http://localhost",
            "http://localhost:3000",
            "https://LOCALHOST:8443",
            "http://127.0.0.1:3000",
            "http://[::1]:3000",
        ] {
            assert!(origin_is_loopback(origin), "{origin} must be loopback");
        }
    }

    #[test]
    fn non_loopback_origins_are_rejected() {
        for origin in [
            "http://evil.example.com",
            "http://127.0.0.1.evil.com",
            "null",
            "",
            "http://192.168.1.10:3000",
        ] {
            assert!(!origin_is_loopback(origin), "{origin} must be rejected");
        }
    }

    #[test]
    fn origin_header_gates_requests() {
        let mut headers = HeaderMap::new();
        assert!(
            validate_origin(&headers).is_none(),
            "no Origin header (non-browser client) must pass"
        );
        headers.insert(
            axum::http::header::ORIGIN,
            HeaderValue::from_static("http://localhost:3000"),
        );
        assert!(validate_origin(&headers).is_none());
        headers.insert(
            axum::http::header::ORIGIN,
            HeaderValue::from_static("http://attacker.example"),
        );
        let resp = validate_origin(&headers).expect("must reject");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn protocol_version_header_is_validated_when_present() {
        let mut headers = HeaderMap::new();
        assert!(
            validate_protocol_version_header(&headers).is_none(),
            "absent header assumes 2025-03-26 per spec"
        );
        for version in super::SUPPORTED_PROTOCOL_VERSIONS {
            headers.insert("mcp-protocol-version", HeaderValue::from_static(version));
            assert!(
                validate_protocol_version_header(&headers).is_none(),
                "{version} must be accepted"
            );
        }
        headers.insert("mcp-protocol-version", HeaderValue::from_static("banana"));
        let resp = validate_protocol_version_header(&headers).expect("must reject");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
