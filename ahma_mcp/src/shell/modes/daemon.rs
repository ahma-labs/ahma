//! # The per-user daemon
//!
//! One process per user hosts **both** halves of ahma's background presence
//! (SPEC R-DAEMON.1): the MCP endpoint every editor's `ahma serve stdio`
//! proxies to, and the observability hub every instance reports to and every
//! TUI subscribes to.
//!
//! They used to be two independent singletons with two rendezvous points, two
//! lifetimes and no shared identity — which is why the TUI could end up owning
//! the hub while a detached bridge owned the MCP endpoint, and why quitting a
//! terminal window could take the event stream away from three editors that
//! knew nothing about it.
//!
//! ## What "one process" does and does not mean
//!
//! It does **not** mean one sandbox. Tools still run in one kernel-sandboxed
//! worker subprocess per MCP session, because on Linux a Landlock ruleset
//! restricts the process that applies it, irreversibly: a process cannot hold
//! two workspace scopes (SPEC R5.1). The daemon is a control plane. It executes
//! nothing itself.
//!
//! ## Lifetime
//!
//! The hub socket is the mutex (bind-is-the-mutex); whoever wins it binds the
//! MCP socket too. Idle is judged across *both* halves — no MCP sessions **and**
//! no hub connections — so a TUI watching an idle project keeps the daemon
//! alive while an editor with no TUI attached does too. Exit runs one
//! choreography: stop the sessions, flush history, unlink only the sockets this
//! process bound (SPEC R-ISO.3), and go.

use crate::shell::cli::AppConfig;
use ahma_common::daemon_hub::{HubBindError, HubServer};
use ahma_http_bridge::{BridgeConfig, DaemonExit, ListenerKind, start_bridge};
use anyhow::{Context, Result};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// How often the idle watcher checks whether anything is attached.
const IDLE_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Should the daemon exit now?
///
/// Split out as a pure function because the interesting part is the policy, not
/// the timer: idleness spans both halves of the daemon, and a drain shortens
/// the wait to nothing so a replacement can take over the moment the last
/// session ends.
pub(crate) fn idle_exit_due(
    hub_connections: usize,
    mcp_sessions: usize,
    idle_for: std::time::Duration,
    timeout_secs: u64,
    draining: bool,
) -> bool {
    if hub_connections > 0 || mcp_sessions > 0 {
        return false;
    }
    if draining {
        // A drained daemon has nothing left to finish; waiting out the idle
        // timer would only delay its successor.
        return true;
    }
    // A zero timeout means "stay forever", which is what an operator who runs
    // the daemon deliberately wants.
    timeout_secs > 0 && idle_for.as_secs() >= timeout_secs
}

/// Run the per-user daemon: hub plus MCP endpoint, one runtime, one exit.
pub async fn run_daemon_mode(config: AppConfig) -> Result<()> {
    if let Err(msg) = ahma_common::process_guard::check_spawn_depth() {
        tracing::error!("{msg}");
        return Err(anyhow::anyhow!(msg));
    }

    // ── The rendezvous, and the mutex ────────────────────────────────────────
    //
    // The two sockets are a pair. When an explicit MCP socket is given without
    // an explicit hub socket, the hub goes beside it: left on the shared path,
    // this daemon would lose the bind to whichever one already held it and
    // stand down, leaving nobody serving the endpoint it was asked for.
    let mcp_socket = ahma_common::daemon_hub::mcp_socket_path(
        Some(config.unix_socket_path.as_str()).filter(|p| !p.is_empty()),
    );
    let hub_socket = if config.daemon_socket_explicit {
        ahma_common::daemon_hub::default_socket_path()
    } else {
        ahma_common::daemon_hub::hub_socket_beside(&mcp_socket)
    };
    if let Some(dir) = hub_socket.parent()
        && let Err(e) = ahma_common::daemon_hub::verify_runtime_dir_secure(dir)
    {
        // Refusing is the point: a directory another user can write is a
        // directory in which our sockets can be replaced with theirs.
        return Err(e.context("refusing to start the ahma daemon"));
    }

    let hub = match HubServer::bind_at(hub_socket.clone()).await {
        Ok(server) => server,
        Err(HubBindError::AlreadyRunning) => {
            // The ordinary outcome of losing a startup race: the winner is
            // already serving, and our caller will connect to it.
            tracing::info!("ahma daemon: another daemon already owns the rendezvous; exiting");
            return Ok(());
        }
        Err(HubBindError::Failed(e)) => return Err(e),
    };

    let history_writer = hub
        .attach_history(ahma_common::daemon_history::history_path())
        .await;

    // ── One exit path for both halves ────────────────────────────────────────
    let (stop_tx, mut stop_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let exit = Arc::new(DaemonExit::new(Box::new(move |reason: &str| {
        let _ = stop_tx.send(reason.to_string());
    })));
    hub.set_exit_hook({
        let exit = exit.clone();
        Arc::new(move |reason: &str| exit.request(reason))
    });

    // ── The MCP endpoint ─────────────────────────────────────────────────────
    let active_sessions = Arc::new(AtomicUsize::new(0));
    let bridge = build_bridge_config(&config, &mcp_socket, &active_sessions, &exit)?;

    // An explicit --idle-timeout is a deliberate instruction and outranks the
    // settings value, as every other flag does (SPEC R-CFG1).
    let idle_timeout_secs = config
        .idle_timeout_secs
        .unwrap_or(config.daemon_idle_timeout_secs);

    tracing::info!(
        hub = %hub_socket.display(),
        mcp = %mcp_socket,
        idle_timeout_secs,
        "ahma daemon: serving the hub and the MCP endpoint"
    );

    let hub_connections = hub.connection_count();
    let idle_task = spawn_idle_watcher(
        hub_connections,
        active_sessions.clone(),
        idle_timeout_secs,
        exit.clone(),
    );

    // ── Serve until something asks us to stop ────────────────────────────────
    let reason = tokio::select! {
        result = hub.serve() => {
            match result {
                Ok(()) => "hub accept loop ended".to_string(),
                Err(e) => format!("hub accept loop failed: {e}"),
            }
        }
        result = start_bridge(bridge) => {
            match result {
                Ok(()) => "MCP endpoint ended".to_string(),
                Err(e) => format!("MCP endpoint failed: {e}"),
            }
        }
        stop = stop_rx.recv() => stop.unwrap_or_else(|| "stop requested".to_string()),
        _ = shutdown_signal() => "signal".to_string(),
    };
    idle_task.abort();

    tracing::info!("ahma daemon: stopping ({reason})");
    if let Some(writer) = history_writer {
        // Flush before the sockets go: a record written but not yet on disk is
        // exactly the last thing that happened, which is what someone opening a
        // TUI afterwards is most likely to be looking for.
        writer.shutdown().await;
    }
    remove_own_socket(&hub_socket);
    remove_own_socket(std::path::Path::new(&mcp_socket));
    Ok(())
}

/// Build the MCP endpoint's configuration.
///
/// Deliberately **no** `default_sandbox_scope`: a scope set here would apply to
/// every session from every client (SPEC R5.1, R10.3). Each session locks its
/// own, from its own client's `roots/list`.
fn build_bridge_config(
    config: &AppConfig,
    mcp_socket: &str,
    active_sessions: &Arc<AtomicUsize>,
    exit: &Arc<DaemonExit>,
) -> Result<BridgeConfig> {
    let server_command = std::env::current_exe()
        .context("Failed to get current executable path")?
        .to_string_lossy()
        .to_string();

    Ok(BridgeConfig {
        bind_addr: "127.0.0.1:0".parse().expect("a literal loopback address"),
        server_command,
        server_args: super::build_stdio_server_args(config, "--tools", false),
        enable_colored_output: true,
        default_sandbox_scope: None,
        handshake_timeout_secs: config.handshake_timeout_secs,
        request_timeout_secs: ahma_http_bridge::session::DEFAULT_REQUEST_TIMEOUT_SECS,
        tool_call_timeout_secs: ahma_http_bridge::session::DEFAULT_TOOL_CALL_TIMEOUT_SECS,
        // QUIC is UDP-based and has no meaning over a Unix socket.
        enable_quic: false,
        disable_http1_1: false,
        listener_kind: listener_for(mcp_socket),
        require_token: None,
        require_token_path: None,
        rate_limit_rps: 0,
        rate_limit_burst: 10,
        active_sessions: Some(active_sessions.clone()),
        // The daemon owns the idle policy: the bridge's own timer counts only
        // MCP sessions and would exit while a TUI was still watching.
        idle_timeout_secs: None,
        max_sessions: config.max_sessions,
        peer_factory: None,
        bound_port_tx: None,
        exit: Some(exit.clone()),
    })
}

#[cfg(unix)]
fn listener_for(mcp_socket: &str) -> ListenerKind {
    ListenerKind::Unix(mcp_socket.to_string())
}

/// Windows has no Unix sockets: the MCP endpoint binds an ephemeral loopback
/// port, published in the runtime directory's endpoint descriptor
/// (SPEC R-DAEMON.2).
#[cfg(not(unix))]
fn listener_for(_mcp_socket: &str) -> ListenerKind {
    ListenerKind::Tcp("127.0.0.1:0".parse().expect("a literal loopback address"))
}

/// Watch both halves and ask the daemon to stop once neither has anything
/// attached for the configured window.
fn spawn_idle_watcher(
    hub_connections: Arc<AtomicUsize>,
    active_sessions: Arc<AtomicUsize>,
    timeout_secs: u64,
    exit: Arc<DaemonExit>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut idle_for = std::time::Duration::ZERO;
        loop {
            tokio::time::sleep(IDLE_CHECK_INTERVAL).await;
            let hub_now = hub_connections.load(Ordering::Relaxed);
            let sessions_now = active_sessions.load(Ordering::SeqCst);
            if hub_now > 0 || sessions_now > 0 {
                idle_for = std::time::Duration::ZERO;
                continue;
            }
            idle_for += IDLE_CHECK_INTERVAL;
            if idle_exit_due(
                hub_now,
                sessions_now,
                idle_for,
                timeout_secs,
                exit.is_draining(),
            ) {
                exit.request("idle");
                return;
            }
        }
    })
}

/// Remove a socket file this process bound, and only if it is still the one we
/// bound (SPEC R-ISO.3): if another daemon has since replaced the path, this
/// would orphan *its* live socket.
fn remove_own_socket(path: &std::path::Path) {
    #[cfg(unix)]
    ahma_common::fs_lock::remove_stale_socket(path);
    #[cfg(not(unix))]
    let _ = path;
}

/// Resolve when the process is asked to stop by the operating system.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => return std::future::pending().await,
        };
        let mut int = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(_) => return std::future::pending().await,
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Idleness spans both halves. Counting only MCP sessions would exit while
    /// a TUI sat watching an idle project; counting only hub connections would
    /// exit while an editor was mid-build with no TUI attached.
    #[test]
    fn idle_exit_needs_both_halves_empty_for_the_full_window() {
        assert!(!idle_exit_due(1, 0, Duration::from_secs(600), 60, false));
        assert!(!idle_exit_due(0, 1, Duration::from_secs(600), 60, false));
        assert!(!idle_exit_due(0, 0, Duration::from_secs(59), 60, false));
        assert!(idle_exit_due(0, 0, Duration::from_secs(60), 60, false));
    }

    /// A drained daemon has nothing left to finish, so it goes as soon as the
    /// last session ends rather than making its successor wait out the timer.
    #[test]
    fn draining_shortens_the_wait_to_nothing() {
        assert!(idle_exit_due(0, 0, Duration::ZERO, 60, true));
        assert!(
            !idle_exit_due(0, 1, Duration::ZERO, 60, true),
            "but a live session still holds a draining daemon open"
        );
    }

    /// `0` is an operator saying "stay": a daemon started deliberately in a
    /// terminal should not disappear because nobody happened to be attached.
    #[test]
    fn a_zero_timeout_never_expires() {
        assert!(!idle_exit_due(0, 0, Duration::from_secs(86_400), 0, false));
    }
}
