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
    },
    OpFinished {
        id: String,
        /// "Completed", "Failed", "Cancelled", "TimedOut"
        status: String,
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
}

// ─────────────────────────────────────────────────────────────────────────────
// Socket path
// ─────────────────────────────────────────────────────────────────────────────

/// Return the platform-default socket path for the hub daemon.
///
/// Override with `AHMA_DAEMON_SOCK` (set to a path on Unix or `host:port` on
/// Windows — though Windows currently always uses `127.0.0.1:7395`).
pub fn default_socket_path() -> PathBuf {
    if let Ok(v) = std::env::var("AHMA_DAEMON_SOCK") {
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
}

impl DaemonHub {
    fn new() -> (Self, broadcast::Receiver<DaemonMsg>) {
        let (tx, rx) = broadcast::channel(512);
        (
            Self {
                instances: Arc::new(Mutex::new(std::collections::HashMap::new())),
                broadcast: tx,
                connection_count: Arc::new(AtomicUsize::new(0)),
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
    let socket_path = default_socket_path();

    // ── Bind (the mutex): try, handle EADDRINUSE ──────────────────────────────
    #[cfg(unix)]
    let listener = bind_unix(&socket_path).await?;

    #[cfg(not(unix))]
    let listener = bind_tcp().await?;

    info!(
        "ahma daemon: listening on {}",
        socket_path.display()
    );

    let (hub, _) = DaemonHub::new();
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
            hub.instances
                .lock()
                .await
                .insert(id.clone(), info.clone());
            let _ = hub
                .broadcast
                .send(DaemonMsg::InstanceRegistered { instance: info });
            info!("daemon: instance registered id={id} pid={pid}");

            // Read events until EOF or Unregister.
            loop {
                match recv_msg::<_, ClientMsg>(&mut reader).await {
                    Ok(ClientMsg::Event { payload }) => {
                        let _ = hub.broadcast.send(DaemonMsg::Event {
                            instance_id: id.clone(),
                            payload,
                        });
                    }
                    Ok(ClientMsg::Unregister) | Err(_) => break,
                    Ok(_) => {} // ignore unexpected messages
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
