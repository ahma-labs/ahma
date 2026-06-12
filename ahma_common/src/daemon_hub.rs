//! Ahma Hub Daemon – lightweight IPC hub for multi-instance aggregation.
//!
//! The hub daemon is a tiny user-space process that:
//! * Accepts connections from all running ahma instances (stdio, http, unix).
//! * Accepts subscription connections from TUI clients.
//! * Fans out operation events to all subscribers in real time.
//!
//! ## Transport
//!
//! | Platform        | Transport                                |
//! |-----------------|------------------------------------------|
//! | Unix / macOS    | Unix domain socket (`~/.ahma/daemon.sock`) |
//! | Windows         | TCP loopback `127.0.0.1:7395`            |
//!
//! Override the path/address with the `AHMA_DAEMON_SOCK` environment variable.
//!
//! ## Protocol
//!
//! All messages are newline-delimited JSON (NDJ).  Each line is one serialised
//! [`ClientMsg`] (instance → daemon or subscriber → daemon) or [`DaemonMsg`]
//! (daemon → subscriber).
//!
//! ## Startup / race-condition handling
//!
//! The bind-is-the-mutex approach avoids lock files entirely:
//!
//! **Instance side** (`ensure_daemon_running`):
//! 1. Try `connect()` → success → done, use existing daemon.
//! 2. Spawn `ahma daemon` as a detached child.
//! 3. Poll `connect()` every 50 ms × 20 attempts (~1 s).
//!
//! **Daemon startup** (`run_daemon`):
//! 1. Try `bind()` → `Ok` → start serving (won the race).
//! 2. `EADDRINUSE` → try `connect()` → `Ok` → `exit(0)` (another daemon won).
//! 3. `EADDRINUSE` + `ECONNREFUSED` → unlink stale socket file (ENOENT benign)
//!    → go back to step 1.
//!
//! **Idle exit**: the daemon resets a 60-second timer on every new connection.
//! When the timer fires and the active connection count is zero it unlinks the
//! socket file *first* (so late arrivals get ENOENT, not ECONNREFUSED) and then
//! exits cleanly.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex, broadcast},
};
use tracing::{debug, info, warn};

// ─── Windows constant ────────────────────────────────────────────────────────

/// TCP port used on Windows (unix sockets not supported there).
pub const WINDOWS_DAEMON_PORT: u16 = 7395;

/// Interval at which the hub sends liveness pings to connected instances.
const PING_INTERVAL: Duration = Duration::from_secs(30);

// ─────────────────────────────────────────────────────────────────────────────
// Protocol types
// ─────────────────────────────────────────────────────────────────────────────

/// Metadata about a registered ahma instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceInfo {
    /// Random UUID assigned by the daemon at registration time.
    pub id: String,
    pub pid: u32,
    /// "stdio", "http", or "unix"
    pub mode: String,
    /// Sandbox scope / workspace root, e.g. `/home/user/project`.
    pub scope: String,
    /// Human-readable label, e.g. `"VS Code"` or `"Cursor"`.
    pub label: String,
}

/// An operation event forwarded from an instance to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum DaemonEvent {
    OpStarted {
        id: String,
        tool_name: String,
        description: String,
        scope: String,
    },
    OpFinished {
        id: String,
        /// "Completed", "Failed", "Cancelled", "TimedOut"
        status: String,
        result_summary: Option<String>,
        duration_ms: u64,
    },
    /// A single line of live output from a running operation.
    /// Streamed as the child process produces it, so subscribers (TUI) can
    /// render output in real time instead of waiting for completion.
    OpOutput {
        id: String,
        line: String,
        is_stderr: bool,
    },
    LogLine {
        level: String,
        message: String,
    },
}

/// Message from any client (instance or TUI subscriber) to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMsg {
    /// An ahma instance announcing itself.  Sent once immediately after connecting.
    Register {
        pid: u32,
        mode: String,
        scope: String,
        label: String,
    },
    /// An operation event from a registered instance.
    Event { payload: DaemonEvent },
    /// A TUI/subscriber requesting the live event stream.
    Subscribe,
    /// A TUI/subscriber requesting a one-shot snapshot of current instances.
    ListInstances,
    /// An instance gracefully unregistering (optional — EOF works too).
    Unregister,
    /// Liveness response to a hub [`DaemonMsg::Ping`].
    Pong { seq: u32 },
    /// Ask the daemon to shut down and exit immediately.
    Shutdown,
}

/// Message from the daemon to a subscriber (TUI).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonMsg {
    /// Sent once in response to `Subscribe` or `ListInstances`.
    InstanceList { instances: Vec<InstanceInfo> },
    /// Broadcast whenever a new instance registers.
    InstanceRegistered { instance: InstanceInfo },
    /// Broadcast whenever an instance disconnects or sends `Unregister`.
    InstanceUnregistered { id: String },
    /// An event forwarded from a registered instance.
    Event {
        instance_id: String,
        payload: DaemonEvent,
    },
    /// Liveness probe sent from hub to a connected instance.
    /// The instance should respond with a matching [`ClientMsg::Pong`].
    Ping { seq: u32 },
}

// ─────────────────────────────────────────────────────────────────────────────
// Socket path
// ─────────────────────────────────────────────────────────────────────────────

/// Process-wide socket path override set from the `--daemon-socket` CLI flag.
static SOCKET_PATH_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Set the daemon socket path from the `--daemon-socket` CLI flag.
/// Call once, early in startup. Takes precedence over `AHMA_DAEMON_SOCK`.
pub fn set_socket_path_override(path: PathBuf) {
    let _ = SOCKET_PATH_OVERRIDE.set(path);
}

/// Return the platform-default socket path for the hub daemon.
///
/// Resolution order: `--daemon-socket` flag, then the deprecated
/// `AHMA_DAEMON_SOCK` environment variable, then the platform default.
pub fn default_socket_path() -> PathBuf {
    if let Some(p) = SOCKET_PATH_OVERRIDE.get() {
        return p.clone();
    }
    if let Ok(v) = std::env::var("AHMA_DAEMON_SOCK") {
        warn!(
            "Deprecated: AHMA_DAEMON_SOCK environment variable is set. Use the --daemon-socket flag instead."
        );
        return PathBuf::from(v);
    }

    #[cfg(unix)]
    {
        // Prefer XDG_RUNTIME_DIR on Linux (per-user, tmpfs, auto-cleaned).
        if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
            let dir = PathBuf::from(xdg).join("ahma");
            let _ = std::fs::create_dir_all(&dir);
            return dir.join("daemon.sock");
        }
        // Fall back to ~/.ahma/daemon.sock (macOS + Linux without XDG).
        if let Some(home) = dirs::home_dir() {
            let dir = home.join(".ahma");
            let _ = std::fs::create_dir_all(&dir);
            return dir.join("daemon.sock");
        }
        PathBuf::from("/tmp/ahma-daemon.sock")
    }

    #[cfg(not(unix))]
    {
        // On Windows the path is unused; callers use the TCP address.
        PathBuf::from("unused-on-windows")
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Client-side transport abstraction
// ─────────────────────────────────────────────────────────────────────────────

/// Platform stream type returned by [`connect_to_daemon`].
///
/// On Unix this is a `UnixStream`; on Windows a `TcpStream`.
/// Both implement `AsyncRead + AsyncWrite + Unpin + Send`.
#[cfg(unix)]
pub type DaemonStream = tokio::net::UnixStream;

#[cfg(not(unix))]
pub type DaemonStream = tokio::net::TcpStream;

/// Try to connect to the hub daemon.
///
/// Returns `Ok(stream)` on success, or an error if the daemon is not running.
pub async fn connect_to_daemon() -> Result<DaemonStream> {
    #[cfg(unix)]
    {
        let path = default_socket_path();
        Ok(tokio::net::UnixStream::connect(&path).await?)
    }
    #[cfg(not(unix))]
    {
        Ok(tokio::net::TcpStream::connect(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            WINDOWS_DAEMON_PORT,
        )))
        .await?)
    }
}

/// Returns `true` if a daemon is currently accepting connections.
async fn try_connect() -> bool {
    connect_to_daemon().await.is_ok()
}

/// Ensure a hub daemon is running, starting one if necessary.
///
/// * Fast path: daemon already up — returns immediately.
/// * Slow path: spawns `ahma daemon` as a detached child, then polls until
///   it accepts connections (up to ~1 second / 20 attempts at 50 ms).
///
/// Returns `Ok(())` when a daemon is reachable, or an error if it could not
/// be started.
pub async fn ensure_daemon_running() -> Result<()> {
    if try_connect().await {
        return Ok(());
    }

    // Spawn the daemon as a detached child — use the current executable so
    // this works regardless of PATH.
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ahma"));
    let mut cmd = tokio::process::Command::new(&exe);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    // Detach from the current process group so the daemon outlives us on Unix.
    // process_group(0) calls setpgid(0,0) in the child — creates a new process
    // group so the daemon is not killed when the spawning terminal/IDE exits.
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    match cmd.spawn() {
        Ok(_child) => {
            debug!("daemon_hub: spawned ahma daemon from {:?}", exe);
        }
        Err(e) => {
            bail!("Failed to spawn ahma daemon: {e}");
        }
    }

    // Poll until connected (max ~1 s).
    for attempt in 1..=20u32 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if try_connect().await {
            debug!("daemon_hub: connected after {}ms", attempt * 50);
            return Ok(());
        }
    }

    bail!(
        "ahma daemon did not become ready within 1 s. \
         Try starting it manually with `ahma daemon`."
    )
}

/// Stop the running hub daemon immediately.
pub async fn stop_daemon() -> Result<()> {
    if let Ok(mut stream) = connect_to_daemon().await {
        send_msg(&mut stream, &ClientMsg::Shutdown).await?;
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Framing helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Serialise `msg` as JSON and write it as a single line to `writer`.
pub async fn send_msg<W, M>(writer: &mut W, msg: &M) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
    M: Serialize,
{
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    Ok(())
}

/// Read one newline-delimited JSON message from a buffered reader.
pub async fn recv_msg<R, M>(reader: &mut BufReader<R>) -> Result<M>
where
    R: tokio::io::AsyncRead + Unpin,
    M: for<'de> Deserialize<'de>,
{
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        bail!("daemon connection closed (EOF)");
    }
    Ok(serde_json::from_str(line.trim())?)
}

// ─────────────────────────────────────────────────────────────────────────────
// Server-side: run_daemon
// ─────────────────────────────────────────────────────────────────────────────

/// Internal shared state for the running daemon.
struct DaemonHub {
    instances: Arc<Mutex<std::collections::HashMap<String, InstanceInfo>>>,
    broadcast: broadcast::Sender<DaemonMsg>,
    connection_count: Arc<AtomicUsize>,
    socket_path: Option<PathBuf>,
}

impl DaemonHub {
    fn new(socket_path: Option<PathBuf>) -> (Self, broadcast::Receiver<DaemonMsg>) {
        let (tx, rx) = broadcast::channel(512);
        (
            Self {
                instances: Arc::new(Mutex::new(std::collections::HashMap::new())),
                broadcast: tx,
                connection_count: Arc::new(AtomicUsize::new(0)),
                socket_path,
            },
            rx,
        )
    }
}

/// Start the hub daemon.
///
/// Binds a unix socket (macOS / Linux) or TCP socket (Windows), accepts
/// connections from instances and TUI subscribers, and fans out events.
///
/// Exits automatically when no connections have been active for 60 seconds.
///
/// This function is called by `ahma daemon` via the CLI dispatch.
pub async fn run_daemon() -> Result<()> {
    run_daemon_at(default_socket_path()).await
}

/// Like [`run_daemon`] but binds at `socket_path` instead of the default.
///
/// Exposed for testing — callers can pass a temp-directory path to avoid
/// colliding with a real daemon running on the default socket.
pub async fn run_daemon_at(socket_path: PathBuf) -> Result<()> {
    // ── Bind (the mutex): try, handle EADDRINUSE ──────────────────────────────
    #[cfg(unix)]
    let listener = bind_unix(&socket_path).await?;

    #[cfg(not(unix))]
    let listener = bind_tcp().await?;

    info!("ahma daemon: listening on {}", socket_path.display());

    let (hub, _) = DaemonHub::new(Some(socket_path.clone()));
    let hub = Arc::new(hub);

    // ── Idle-exit watcher ─────────────────────────────────────────────────────
    let idle_count = hub.connection_count.clone();
    #[cfg(unix)]
    let idle_socket_path = socket_path.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if idle_count.load(Ordering::Relaxed) == 0 {
                // Wait 60 s with count still at 0.
                tokio::time::sleep(Duration::from_secs(60)).await;
                if idle_count.load(Ordering::Relaxed) == 0 {
                    info!("ahma daemon: idle timeout, exiting");
                    // Unlink the socket file FIRST so late arrivals get ENOENT
                    // (clean start) instead of ECONNREFUSED (ambiguous stale).
                    #[cfg(unix)]
                    let _ = std::fs::remove_file(&idle_socket_path);
                    std::process::exit(0);
                }
            }
        }
    });

    // ── Accept loop ───────────────────────────────────────────────────────────
    accept_loop(listener, hub).await
}

// ─────────────────────────────────────────────────────────────────────────────
// Embedded hub — TUI-owned server whose lifecycle matches the TUI process
// ─────────────────────────────────────────────────────────────────────────────

/// Handle for a hub server running inside the TUI process.
///
/// The server binds the same Unix socket (macOS/Linux) or TCP loopback port
/// (Windows) as the standalone `ahma daemon`.  When this handle is dropped the
/// accept-loop task is aborted and the socket file is removed, so no IPC
/// resources are left behind after the TUI exits — even on a panic.
///
/// Subscribers (ahma instances) see an EOF when the socket is removed and
/// reconnect cleanly when a new TUI starts.
pub struct EmbeddedHub {
    broadcast: broadcast::Sender<DaemonMsg>,
    abort: tokio::task::AbortHandle,
    #[cfg(unix)]
    socket_path: PathBuf,
}

impl EmbeddedHub {
    /// Subscribe to the event stream **directly** through an in-process
    /// channel, with no socket round-trip.
    pub fn subscribe(&self) -> broadcast::Receiver<DaemonMsg> {
        self.broadcast.subscribe()
    }
}

impl Drop for EmbeddedHub {
    fn drop(&mut self) {
        // Cancel the accept-loop task first so no new connections arrive
        // while the socket file is being removed.
        self.abort.abort();
        // Best-effort removal — non-fatal if already gone.
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Attempt to start a hub server embedded in the calling process.
///
/// Returns `Ok(Some(hub))` when the server is bound and running.
/// Returns `Ok(None)` when another server (a running TUI or standalone
/// `ahma daemon`) already owns the socket — the caller should fall back to
/// [`spawn_daemon_source`] (subscriber mode).
/// Returns `Err` only for unexpected OS errors (e.g. permission denied).
pub async fn try_start_hub_server() -> Result<Option<EmbeddedHub>> {
    try_start_hub_server_at(default_socket_path()).await
}

/// Like [`try_start_hub_server`] but binds at `socket_path` instead of the
/// platform default.  Exposed for testing.
pub async fn try_start_hub_server_at(socket_path: PathBuf) -> Result<Option<EmbeddedHub>> {
    #[cfg(unix)]
    let listener = match try_bind_unix(&socket_path).await? {
        Some(l) => l,
        None => return Ok(None),
    };

    #[cfg(not(unix))]
    let listener = match try_bind_tcp().await? {
        Some(l) => l,
        None => return Ok(None),
    };

    info!(
        "ahma hub: embedded server started on {}",
        socket_path.display()
    );

    let (hub, _) = DaemonHub::new(Some(socket_path.clone()));
    let hub = Arc::new(hub);
    let broadcast = hub.broadcast.clone();

    let task = tokio::spawn(accept_loop(listener, hub));
    let abort = task.abort_handle();

    Ok(Some(EmbeddedHub {
        broadcast,
        abort,
        #[cfg(unix)]
        socket_path,
    }))
}

// ── Unix bind/accept ──────────────────────────────────────────────────────────

#[cfg(unix)]
async fn bind_unix(path: &std::path::Path) -> Result<tokio::net::UnixListener> {
    use tokio::net::UnixListener;
    loop {
        match UnixListener::bind(path) {
            Ok(l) => return Ok(l),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                // Check if there is actually a live daemon.
                match tokio::net::UnixStream::connect(path).await {
                    Ok(_) => {
                        // Another daemon won the race. Exit gracefully.
                        info!("ahma daemon: another instance is already running, exiting");
                        std::process::exit(0);
                    }
                    Err(_) => {
                        // Stale socket file — unlink and retry.
                        debug!("ahma daemon: removing stale socket at {}", path.display());
                        let _ = std::fs::remove_file(path);
                        // Small delay before retry to avoid tight loop on weird FS.
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
            Err(e) => bail!("ahma daemon: failed to bind unix socket: {e}"),
        }
    }
}

/// Try to bind the Unix socket for the embedded hub.
/// Returns `None` when another server is already running (caller should subscribe instead).
#[cfg(unix)]
async fn try_bind_unix(path: &std::path::Path) -> Result<Option<tokio::net::UnixListener>> {
    use tokio::net::UnixListener;
    loop {
        match UnixListener::bind(path) {
            Ok(l) => return Ok(Some(l)),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                match tokio::net::UnixStream::connect(path).await {
                    Ok(_) => {
                        // Another server owns the socket — caller should subscribe.
                        debug!(
                            "ahma hub: another server already running at {}",
                            path.display()
                        );
                        return Ok(None);
                    }
                    Err(_) => {
                        // Stale socket file — remove and retry.
                        debug!("ahma hub: removing stale socket at {}", path.display());
                        let _ = std::fs::remove_file(path);
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(unix)]
async fn accept_loop(listener: tokio::net::UnixListener, hub: Arc<DaemonHub>) -> Result<()> {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                hub.connection_count.fetch_add(1, Ordering::Relaxed);
                let hub2 = hub.clone();
                tokio::spawn(async move {
                    handle_connection(stream, hub2).await;
                });
            }
            Err(e) => warn!("ahma daemon: accept error: {e}"),
        }
    }
}

// ── Windows bind/accept ───────────────────────────────────────────────────────

#[cfg(not(unix))]
async fn bind_tcp() -> Result<tokio::net::TcpListener> {
    use tokio::net::TcpListener;
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], WINDOWS_DAEMON_PORT));
    match TcpListener::bind(addr).await {
        Ok(l) => Ok(l),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // Try connecting to confirm a live daemon.
            match tokio::net::TcpStream::connect(addr).await {
                Ok(_) => {
                    info!("ahma daemon: another instance is already running, exiting");
                    std::process::exit(0);
                }
                Err(_) => {
                    bail!(
                        "ahma daemon: port {} is in use by another process",
                        WINDOWS_DAEMON_PORT
                    );
                }
            }
        }
        Err(e) => bail!("ahma daemon: failed to bind TCP socket: {e}"),
    }
}

/// Try to bind the TCP loopback port for the embedded hub on Windows.
/// Returns `None` when another server is already running (caller should subscribe instead).
#[cfg(not(unix))]
async fn try_bind_tcp() -> Result<Option<tokio::net::TcpListener>> {
    use tokio::net::TcpListener;
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], WINDOWS_DAEMON_PORT));
    match TcpListener::bind(addr).await {
        Ok(l) => Ok(Some(l)),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            match tokio::net::TcpStream::connect(addr).await {
                Ok(_) => {
                    // Another server owns the port — caller should subscribe.
                    debug!(
                        "ahma hub: another server already running on port {}",
                        WINDOWS_DAEMON_PORT
                    );
                    Ok(None)
                }
                Err(_) => Err(anyhow::anyhow!(
                    "port {} is in use by another process",
                    WINDOWS_DAEMON_PORT
                )),
            }
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(not(unix))]
async fn accept_loop(listener: tokio::net::TcpListener, hub: Arc<DaemonHub>) -> Result<()> {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                hub.connection_count.fetch_add(1, Ordering::Relaxed);
                let hub2 = hub.clone();
                tokio::spawn(async move {
                    handle_connection(stream, hub2).await;
                });
            }
            Err(e) => warn!("ahma daemon: accept error: {e}"),
        }
    }
}

// ── Per-connection handler (generic over stream type) ─────────────────────────

async fn handle_connection<S>(stream: S, hub: Arc<DaemonHub>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (read_half, write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let mut writer = write_half;

    // Read the first message to classify the connection.
    let first = match recv_msg::<_, ClientMsg>(&mut reader).await {
        Ok(m) => m,
        Err(e) => {
            debug!("daemon: connection closed before first message: {e}");
            hub.connection_count.fetch_sub(1, Ordering::Relaxed);
            return;
        }
    };

    match first {
        ClientMsg::Register {
            pid,
            mode,
            scope,
            label,
        } => {
            let id = uuid_v4();
            let info = InstanceInfo {
                id: id.clone(),
                pid,
                mode,
                scope,
                label,
            };
            hub.instances.lock().await.insert(id.clone(), info.clone());
            let _ = hub
                .broadcast
                .send(DaemonMsg::InstanceRegistered { instance: info });
            info!("daemon: instance registered id={id} pid={pid}");

            // Exchange events and liveness pings until the instance disconnects.
            let mut ping_interval = tokio::time::interval_at(
                tokio::time::Instant::now() + PING_INTERVAL,
                PING_INTERVAL,
            );
            ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut ping_seq: u32 = 0;

            loop {
                tokio::select! {
                    biased;

                    msg = recv_msg::<_, ClientMsg>(&mut reader) => {
                        match msg {
                            Ok(ClientMsg::Event { payload }) => {
                                let _ = hub.broadcast.send(DaemonMsg::Event {
                                    instance_id: id.clone(),
                                    payload,
                                });
                            }
                            Ok(ClientMsg::Pong { .. }) => {
                                // Liveness confirmed — nothing else to do for now.
                                debug!("daemon: pong received from id={id}");
                            }
                            Ok(ClientMsg::Unregister) | Err(_) => break,
                            Ok(_) => {} // ignore unexpected messages
                        }
                    }

                    _ = ping_interval.tick() => {
                        ping_seq = ping_seq.wrapping_add(1);
                        debug!("daemon: sending ping to id={id} seq={ping_seq}");
                        if send_msg(&mut writer, &DaemonMsg::Ping { seq: ping_seq })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }

            hub.instances.lock().await.remove(&id);
            let _ = hub
                .broadcast
                .send(DaemonMsg::InstanceUnregistered { id: id.clone() });
            info!("daemon: instance unregistered id={id}");
        }

        ClientMsg::Subscribe => {
            // Send current instance list, then stream events.
            let instances: Vec<InstanceInfo> =
                hub.instances.lock().await.values().cloned().collect();
            if let Err(e) = send_msg(&mut writer, &DaemonMsg::InstanceList { instances }).await {
                debug!("daemon: subscriber write failed: {e}");
                hub.connection_count.fetch_sub(1, Ordering::Relaxed);
                return;
            }

            let mut rx = hub.broadcast.subscribe();
            loop {
                match rx.recv().await {
                    Ok(msg) => {
                        if let Err(e) = send_msg(&mut writer, &msg).await {
                            debug!("daemon: subscriber write failed: {e}");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("daemon: subscriber lagged by {n} messages");
                        // Continue — lagging is non-fatal.
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }

        ClientMsg::ListInstances => {
            let instances: Vec<InstanceInfo> =
                hub.instances.lock().await.values().cloned().collect();
            let _ = send_msg(&mut writer, &DaemonMsg::InstanceList { instances }).await;
            // One-shot query — connection closes after response.
        }

        ClientMsg::Shutdown => {
            info!("daemon: shutdown requested, exiting");
            if let Some(ref path) = hub.socket_path {
                #[cfg(unix)]
                let _ = std::fs::remove_file(path);
            }
            std::process::exit(0);
        }

        _ => {
            debug!("daemon: unexpected first message, closing connection");
        }
    }

    hub.connection_count.fetch_sub(1, Ordering::Relaxed);
}

// ─── Tiny UUID v4 without the uuid crate ──────────────────────────────────────

fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Use process ID + timestamp + a counter for sufficient uniqueness in a
    // local IPC context.  We don't need cryptographic randomness here.
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let pid = std::process::id();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{pid:08x}-{ts:08x}-{seq:08x}")
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    // ── NDJ framing ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn ndj_roundtrip_client_msg_register() {
        let msg = ClientMsg::Register {
            pid: 42,
            mode: "stdio".to_string(),
            scope: "/test".to_string(),
            label: "TestLabel".to_string(),
        };
        let mut buf = Vec::<u8>::new();
        send_msg(&mut buf, &msg).await.unwrap();
        assert!(buf.ends_with(b"\n"), "NDJ line must end with newline");
        assert_eq!(
            buf.iter().filter(|&&b| b == b'\n').count(),
            1,
            "exactly one newline"
        );

        let mut reader = BufReader::new(&buf[..]);
        let decoded: ClientMsg = recv_msg(&mut reader).await.unwrap();
        match decoded {
            ClientMsg::Register {
                pid,
                mode,
                scope,
                label,
            } => {
                assert_eq!(pid, 42);
                assert_eq!(mode, "stdio");
                assert_eq!(scope, "/test");
                assert_eq!(label, "TestLabel");
            }
            other => panic!("expected Register, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ndj_roundtrip_daemon_msg_instance_list() {
        let msg = DaemonMsg::InstanceList {
            instances: vec![InstanceInfo {
                id: "abc123".to_string(),
                pid: 99,
                mode: "http".to_string(),
                scope: "/project".to_string(),
                label: "Cursor".to_string(),
            }],
        };
        let mut buf = Vec::<u8>::new();
        send_msg(&mut buf, &msg).await.unwrap();

        let mut reader = BufReader::new(&buf[..]);
        let decoded: DaemonMsg = recv_msg(&mut reader).await.unwrap();
        match decoded {
            DaemonMsg::InstanceList { instances } => {
                assert_eq!(instances.len(), 1);
                assert_eq!(instances[0].id, "abc123");
                assert_eq!(instances[0].label, "Cursor");
            }
            other => panic!("expected InstanceList, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ndj_roundtrip_daemon_event_op_started() {
        let msg = DaemonMsg::Event {
            instance_id: "inst-1".to_string(),
            payload: DaemonEvent::OpStarted {
                id: "op-1".to_string(),
                tool_name: "cargo_build".to_string(),
                description: "Build workspace".to_string(),
                scope: "/test/scope".to_string(),
            },
        };
        let mut buf = Vec::<u8>::new();
        send_msg(&mut buf, &msg).await.unwrap();

        let mut reader = BufReader::new(&buf[..]);
        let decoded: DaemonMsg = recv_msg(&mut reader).await.unwrap();
        match decoded {
            DaemonMsg::Event {
                instance_id,
                payload: DaemonEvent::OpStarted { id, tool_name, .. },
            } => {
                assert_eq!(instance_id, "inst-1");
                assert_eq!(id, "op-1");
                assert_eq!(tool_name, "cargo_build");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn ndj_roundtrip_daemon_event_op_finished() {
        let msg = DaemonMsg::Event {
            instance_id: "inst-2".to_string(),
            payload: DaemonEvent::OpFinished {
                id: "op-2".to_string(),
                status: "Completed".to_string(),
                result_summary: Some("success".to_string()),
                duration_ms: 1500,
            },
        };
        let mut buf = Vec::<u8>::new();
        send_msg(&mut buf, &msg).await.unwrap();

        let mut reader = BufReader::new(&buf[..]);
        let decoded: DaemonMsg = recv_msg(&mut reader).await.unwrap();
        match decoded {
            DaemonMsg::Event {
                payload: DaemonEvent::OpFinished { id, status, .. },
                ..
            } => {
                assert_eq!(id, "op-2");
                assert_eq!(status, "Completed");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn ndj_roundtrip_subscribe_and_unregister() {
        // Verify unit variants round-trip correctly.
        for msg in [
            ClientMsg::Subscribe,
            ClientMsg::ListInstances,
            ClientMsg::Unregister,
        ] {
            let mut buf = Vec::<u8>::new();
            send_msg(&mut buf, &msg).await.unwrap();
            assert!(buf.ends_with(b"\n"));
        }
    }

    #[tokio::test]
    async fn ndj_eof_returns_error() {
        let empty: &[u8] = b"";
        let mut reader = BufReader::new(empty);
        let result: Result<ClientMsg> = recv_msg(&mut reader).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("EOF") || msg.contains("closed"),
            "expected EOF error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn ndj_malformed_json_returns_error() {
        let bad = b"not-valid-json\n";
        let mut reader = BufReader::new(bad.as_slice());
        let result: Result<ClientMsg> = recv_msg(&mut reader).await;
        assert!(result.is_err());
    }

    // ── uuid_v4 ───────────────────────────────────────────────────────────────

    #[test]
    fn uuid_v4_produces_unique_values() {
        let ids: Vec<String> = (0..20).map(|_| uuid_v4()).collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "all UUIDs should be unique");
    }

    #[test]
    fn uuid_v4_contains_dashes() {
        let id = uuid_v4();
        assert!(id.contains('-'), "UUID should contain dashes: {id}");
    }

    // ── In-process daemon integration (Unix only) ─────────────────────────────
    //
    // We spin up `run_daemon_at()` in a background tokio task pointing to a
    // temp socket, then exercise the register → subscribe → event → unregister
    // flow end-to-end.  All spawned tasks are automatically cancelled when the
    // test runtime drops.

    /// Wait until a Unix socket file exists and accepts a connection.
    #[cfg(unix)]
    async fn wait_for_daemon(sock: &std::path::Path) {
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            if tokio::net::UnixStream::connect(sock).await.is_ok() {
                return;
            }
        }
        panic!("daemon did not start within 1 s on {}", sock.display());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn daemon_subscribe_register_event_unregister_flow() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("daemon.sock");
        let sock2 = sock.clone();
        tokio::spawn(async move {
            let _ = run_daemon_at(sock2).await;
        });
        wait_for_daemon(&sock).await;

        // ── Subscriber connects ────────────────────────────────────────────
        let sub = tokio::net::UnixStream::connect(&sock)
            .await
            .expect("connect subscriber");
        let (sr, sw) = tokio::io::split(sub);
        let mut sub_reader = BufReader::new(sr);
        let mut sub_writer = sw;
        send_msg(&mut sub_writer, &ClientMsg::Subscribe)
            .await
            .unwrap();

        // Initial InstanceList should be empty.
        let first: DaemonMsg = recv_msg(&mut sub_reader).await.unwrap();
        let DaemonMsg::InstanceList { instances } = first else {
            panic!("expected InstanceList, got {first:?}");
        };
        assert!(instances.is_empty(), "no instances registered yet");

        // ── Instance registers ─────────────────────────────────────────────
        let inst = tokio::net::UnixStream::connect(&sock)
            .await
            .expect("connect instance");
        let (_, mut iw) = tokio::io::split(inst);
        send_msg(
            &mut iw,
            &ClientMsg::Register {
                pid: std::process::id(),
                mode: "stdio".to_string(),
                scope: "/test/scope".to_string(),
                label: "TestInstance".to_string(),
            },
        )
        .await
        .unwrap();

        // Subscriber receives InstanceRegistered.
        let reg: DaemonMsg = recv_msg(&mut sub_reader).await.unwrap();
        let DaemonMsg::InstanceRegistered { instance } = reg else {
            panic!("expected InstanceRegistered, got {reg:?}");
        };
        assert_eq!(instance.label, "TestInstance");
        assert_eq!(instance.mode, "stdio");
        let instance_id = instance.id.clone();

        // ── OpStarted event ────────────────────────────────────────────────
        send_msg(
            &mut iw,
            &ClientMsg::Event {
                payload: DaemonEvent::OpStarted {
                    id: "op-001".to_string(),
                    tool_name: "cargo_test".to_string(),
                    description: "Run tests".to_string(),
                    scope: "/test/scope".to_string(),
                },
            },
        )
        .await
        .unwrap();

        let ev: DaemonMsg = recv_msg(&mut sub_reader).await.unwrap();
        match ev {
            DaemonMsg::Event {
                instance_id: iid,
                payload: DaemonEvent::OpStarted { id, tool_name, .. },
            } => {
                assert_eq!(iid, instance_id);
                assert_eq!(id, "op-001");
                assert_eq!(tool_name, "cargo_test");
            }
            other => panic!("expected Event::OpStarted, got {other:?}"),
        }

        // ── OpFinished event ───────────────────────────────────────────────
        send_msg(
            &mut iw,
            &ClientMsg::Event {
                payload: DaemonEvent::OpFinished {
                    id: "op-001".to_string(),
                    status: "Completed".to_string(),
                    result_summary: Some("success".to_string()),
                    duration_ms: 1200,
                },
            },
        )
        .await
        .unwrap();

        let fin: DaemonMsg = recv_msg(&mut sub_reader).await.unwrap();
        match fin {
            DaemonMsg::Event {
                payload: DaemonEvent::OpFinished { id, status, .. },
                ..
            } => {
                assert_eq!(id, "op-001");
                assert_eq!(status, "Completed");
            }
            other => panic!("expected Event::OpFinished, got {other:?}"),
        }

        // ── Unregister ─────────────────────────────────────────────────────
        send_msg(&mut iw, &ClientMsg::Unregister).await.unwrap();

        let unreg: DaemonMsg = recv_msg(&mut sub_reader).await.unwrap();
        match unreg {
            DaemonMsg::InstanceUnregistered { id } => assert_eq!(id, instance_id),
            other => panic!("expected InstanceUnregistered, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn daemon_list_instances_query() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("list.sock");
        let sock2 = sock.clone();
        tokio::spawn(async move {
            let _ = run_daemon_at(sock2).await;
        });
        wait_for_daemon(&sock).await;

        // Register one instance.
        let inst = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let (_, mut iw) = tokio::io::split(inst);
        send_msg(
            &mut iw,
            &ClientMsg::Register {
                pid: 1234,
                mode: "http".to_string(),
                scope: "/project".to_string(),
                label: "HttpBridge".to_string(),
            },
        )
        .await
        .unwrap();

        // Give the daemon time to process the registration.
        tokio::time::sleep(Duration::from_millis(30)).await;

        // One-shot ListInstances query.
        let q = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let (qr, mut qw) = tokio::io::split(q);
        let mut qrdr = BufReader::new(qr);
        send_msg(&mut qw, &ClientMsg::ListInstances).await.unwrap();

        let resp: DaemonMsg = recv_msg(&mut qrdr).await.unwrap();
        match resp {
            DaemonMsg::InstanceList { instances } => {
                assert_eq!(instances.len(), 1, "expected 1 registered instance");
                assert_eq!(instances[0].label, "HttpBridge");
                assert_eq!(instances[0].mode, "http");
            }
            other => panic!("expected InstanceList, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn daemon_stale_socket_cleanup() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("stale.sock");

        // Create a stale socket file: bind a listener then immediately drop it.
        // The file remains but nothing is listening.
        {
            let _listener = tokio::net::UnixListener::bind(&sock).unwrap();
        }
        assert!(
            sock.exists(),
            "stale socket file should exist before daemon starts"
        );

        // The daemon should detect ECONNREFUSED on the stale socket,
        // remove the file, and bind successfully.
        let sock2 = sock.clone();
        tokio::spawn(async move {
            let _ = run_daemon_at(sock2).await;
        });
        wait_for_daemon(&sock).await;

        // Verify a fresh connection works after cleanup.
        let conn = tokio::net::UnixStream::connect(&sock).await;
        assert!(
            conn.is_ok(),
            "daemon should be running after stale socket cleanup"
        );
    }
}
