//! Finding — or starting — the one daemon (SPEC R-DAEMON.1, R-DAEMON.5).
//!
//! Every entry point that needs ahma's background presence comes through here:
//! the stdio frontend an editor spawns, `ahma tui`, and anything else that
//! wants the MCP endpoint. They all want the same thing: *a daemon is running
//! and it is the right version*.
//!
//! ## Why the version policy is what it is
//!
//! The old rule was "if the running bridge is not my build, restart it". With
//! one bridge per project that was merely rude. With one daemon per user it
//! means a newly-started editor window tears down every other window's session,
//! mid-command, to install a binary only it asked for.
//!
//! So a newer client **drains** instead: the daemon stops accepting new
//! sessions, finishes the ones it has, and exits — at which point the next
//! client starts the new binary. Until then the newcomer proxies to the old
//! daemon, which works, and says so loudly rather than pretending the skew is
//! not there (SPEC R7's rule that ahma never hides its own state).

use anyhow::{Context, Result};
use std::path::Path;

use super::server::{
    check_bridge_running, get_bridge_version, is_test_isolated, parse_version,
    split_version_and_build_id,
};

/// How long to wait for a draining daemon to release the rendezvous before
/// starting the successor anyway.
const DRAIN_HANDOFF_SECS: u64 = 5;

/// What [`ensure_daemon`] found or did.
#[derive(Debug, PartialEq, Eq)]
pub enum EnsureOutcome {
    /// A daemon of this build was already serving.
    Ready,
    /// A daemon is serving, it is a different build, and it could not be
    /// replaced right now — it is either busy or a replacement was already
    /// attempted this lineage. Work continues against it; the caller must
    /// disclose the skew.
    ReadyButStale { daemon: String, ours: String },
    /// No daemon was serving, so this call started one.
    Spawned,
}

/// What the version comparison says to do.
///
/// A pure decision so the policy can be tested exhaustively: the failure modes
/// here are respawn storms and torn-down sessions, neither of which is pleasant
/// to reproduce by hand.
#[derive(Debug, PartialEq, Eq)]
pub enum SkewAction {
    /// Same build: just use it.
    Proxy,
    /// We are newer: ask it to drain, then start the successor.
    Drain,
    /// We are older: re-exec, so the right binary handles this session.
    ReExec,
    /// A skew we must not act on — a replacement was already tried in this
    /// lineage, so trying again is how a respawn storm starts. Proxy and
    /// disclose.
    ProxyStale,
}

/// Decide what to do about the running daemon's version.
///
/// `daemon_version` is the raw `semver+build_id` string from `/health`.
pub fn skew_action(
    client_semver: &str,
    client_build: &str,
    daemon_version: &str,
    already_restarted: bool,
) -> SkewAction {
    let (daemon_semver, daemon_build) = split_version_and_build_id(daemon_version);
    let same_semver = daemon_semver == client_semver;
    let same_build = daemon_build.is_none_or(|b| b == client_build);
    if same_semver && same_build {
        return SkewAction::Proxy;
    }
    // A version string neither side can parse is not evidence the daemon is
    // current, so it counts as older: an unreadable version must not become a
    // reason to keep whatever is running.
    let we_are_newer = match (parse_version(client_semver), parse_version(daemon_semver)) {
        (Some(ours), Some(theirs)) => ours > theirs,
        _ => true,
    } || (same_semver && !same_build);

    if already_restarted {
        // The mismatch survived a replacement, so replacing again is futile and
        // is exactly how several coexisting build-ids turn into a respawn loop.
        return SkewAction::ProxyStale;
    }
    if we_are_newer {
        SkewAction::Drain
    } else {
        SkewAction::ReExec
    }
}

/// Ensure a daemon is serving at `socket_path` / `http_url`, starting one if
/// there is none, and reconcile a version skew.
pub async fn ensure_daemon(
    socket_path: Option<&str>,
    http_url: Option<&str>,
    idle_timeout_secs: Option<u64>,
) -> Result<EnsureOutcome> {
    let client_semver = env!("CARGO_PKG_VERSION");
    let client_build = ahma_common::BUILD_ID;

    // A test-isolated process never judges — much less replaces — a daemon it
    // does not own (SPEC R-ISO.1).
    let running_version = if is_test_isolated() {
        None
    } else {
        get_bridge_version(socket_path, http_url).await
    };

    let Some(daemon_version) = running_version else {
        if check_bridge_running(socket_path, http_url).await {
            // Serving but not answering /health: an older or foreign server.
            return Ok(EnsureOutcome::Ready);
        }
        spawn_daemon(socket_path, http_url, idle_timeout_secs).await?;
        return Ok(EnsureOutcome::Spawned);
    };

    let already_restarted = std::env::var("AHMA_RESTARTED").is_ok();
    match skew_action(
        client_semver,
        client_build,
        &daemon_version,
        already_restarted,
    ) {
        SkewAction::Proxy => Ok(EnsureOutcome::Ready),
        SkewAction::ProxyStale => Ok(EnsureOutcome::ReadyButStale {
            daemon: daemon_version,
            ours: format!("{client_semver}+{client_build}"),
        }),
        SkewAction::ReExec => {
            tracing::info!(
                daemon_version = %daemon_version,
                "this build is older than the running daemon; re-executing"
            );
            super::server::re_exec_current_process()?;
            unreachable!("re_exec_current_process replaces this process image")
        }
        SkewAction::Drain => {
            tracing::info!(
                daemon_version = %daemon_version,
                client_version = client_semver,
                "asking the running daemon to drain so this build can take over"
            );
            let drained = drain_and_wait(socket_path, http_url).await;
            if !drained {
                // It is still serving somebody. Use it and disclose the skew
                // rather than killing sessions that are not ours to end.
                return Ok(EnsureOutcome::ReadyButStale {
                    daemon: daemon_version,
                    ours: format!("{client_semver}+{client_build}"),
                });
            }
            spawn_daemon(socket_path, http_url, idle_timeout_secs).await?;
            Ok(EnsureOutcome::Spawned)
        }
    }
}

/// Ask the daemon to drain, then wait for it to release the rendezvous.
///
/// Returns `false` when it is still there at the deadline — it has live
/// sessions, and taking them away is precisely what draining exists to avoid.
async fn drain_and_wait(socket_path: Option<&str>, http_url: Option<&str>) -> bool {
    super::server::trigger_bridge_drain(socket_path, http_url).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(DRAIN_HANDOFF_SECS);
    while std::time::Instant::now() < deadline {
        if !check_bridge_running(socket_path, http_url).await {
            return true;
        }
        tokio::time::sleep(ahma_common::timeouts::TestTimeouts::poll_interval()).await;
    }
    false
}

/// Start the daemon, detached, and wait for it to answer.
async fn spawn_daemon(
    socket_path: Option<&str>,
    http_url: Option<&str>,
    idle_timeout_secs: Option<u64>,
) -> Result<()> {
    let exe = std::env::current_exe().context("Failed to get current executable path")?;
    let mut cmd = tokio::process::Command::new(&exe);
    cmd.arg("daemon");
    if let Some(path) = socket_path {
        cmd.args(["--unix-socket-path", path]);
        // Hand it the matching hub socket, so a caller that asked for a private
        // MCP endpoint gets a daemon of its own rather than standing down
        // against whoever holds the shared hub.
        cmd.args([
            "--daemon-socket",
            &ahma_common::daemon_hub::hub_socket_beside(path).to_string_lossy(),
        ]);
    }
    if let Some(secs) = idle_timeout_secs {
        cmd.args(["--idle-timeout", &secs.to_string()]);
    }
    // Deliberately detached (SPEC R-PROC.3): the daemon outlives whoever
    // started it, which is the point of there being one.
    cmd.env(
        ahma_common::process_guard::SPAWN_DEPTH_ENV,
        ahma_common::process_guard::child_spawn_depth(),
    );
    // Never inherited: it would make the daemon believe it is a session worker.
    cmd.env_remove("AHMA_SERVER_CHILD");
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        cmd.creation_flags(crate::shell_pool::CREATE_NO_WINDOW);
    }
    cmd.spawn().context("Failed to spawn the ahma daemon")?;

    let timeout =
        ahma_common::timeouts::TestTimeouts::get(ahma_common::timeouts::TimeoutCategory::Quick);
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if check_bridge_running(socket_path, http_url).await {
            tracing::info!("ahma daemon started");
            return Ok(());
        }
        tokio::time::sleep(ahma_common::timeouts::TestTimeouts::poll_interval()).await;
    }
    anyhow::bail!(
        "The ahma daemon did not become reachable within {timeout:?}. \
         Check logs/ahma.log, or run `ahma daemon` in a terminal to see why."
    )
}

/// One sentence a user can act on, for an outcome worth mentioning.
pub fn disclosure(outcome: &EnsureOutcome) -> Option<String> {
    match outcome {
        EnsureOutcome::Ready | EnsureOutcome::Spawned => None,
        EnsureOutcome::ReadyButStale { daemon, ours } => Some(format!(
            "The running ahma daemon is v{daemon} and this build is v{ours}. \
             It is still serving other sessions, so it was not replaced; it will \
             restart itself once they end."
        )),
    }
}

/// True when `path` names a socket nothing is listening on.
pub fn socket_is_stale(path: &Path) -> bool {
    #[cfg(unix)]
    {
        path.exists() && std::os::unix::net::UnixStream::connect(path).is_err()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OURS: &str = "0.20.2";
    const OUR_BUILD: &str = "abc1234";

    #[test]
    fn the_same_build_is_simply_used() {
        assert_eq!(
            skew_action(OURS, OUR_BUILD, "0.20.2+abc1234", false),
            SkewAction::Proxy
        );
        // A daemon that publishes no build id cannot be proven stale by one.
        assert_eq!(
            skew_action(OURS, OUR_BUILD, "0.20.2", false),
            SkewAction::Proxy
        );
    }

    #[test]
    fn a_newer_build_drains_rather_than_restarting() {
        assert_eq!(
            skew_action(OURS, OUR_BUILD, "0.20.1+old1234", false),
            SkewAction::Drain
        );
        // Same version, different build: a developer rebuild. Still a drain,
        // never a restart — other windows are attached to that daemon.
        assert_eq!(
            skew_action(OURS, OUR_BUILD, "0.20.2+def5678", false),
            SkewAction::Drain
        );
    }

    #[test]
    fn an_older_build_re_execs_instead_of_downgrading_the_daemon() {
        assert_eq!(
            skew_action("0.20.1", OUR_BUILD, "0.20.2+newer", false),
            SkewAction::ReExec
        );
    }

    /// A mismatch that survived one replacement will survive the next, and
    /// retrying is how several coexisting build-ids become a respawn loop.
    #[test]
    fn a_skew_that_survived_a_replacement_is_not_retried() {
        assert_eq!(
            skew_action(OURS, OUR_BUILD, "0.20.1+old1234", true),
            SkewAction::ProxyStale
        );
        assert_eq!(
            skew_action("0.20.1", OUR_BUILD, "0.20.2+newer", true),
            SkewAction::ProxyStale
        );
    }

    /// An unreadable version is not evidence that the daemon is current.
    #[test]
    fn an_unparsable_daemon_version_counts_as_older() {
        assert_eq!(
            skew_action(OURS, OUR_BUILD, "not-a-version", false),
            SkewAction::Drain
        );
    }

    #[test]
    fn only_an_unreplaceable_skew_is_disclosed() {
        assert_eq!(disclosure(&EnsureOutcome::Ready), None);
        assert_eq!(disclosure(&EnsureOutcome::Spawned), None);
        let stale = EnsureOutcome::ReadyButStale {
            daemon: "0.20.1+old".into(),
            ours: "0.20.2+new".into(),
        };
        let msg = disclosure(&stale).expect("a skew the user cannot see is a skew they debug");
        assert!(
            msg.contains("0.20.1+old") && msg.contains("0.20.2+new"),
            "{msg}"
        );
        assert!(
            msg.contains("not replaced"),
            "the message must say what did NOT happen to other sessions: {msg}"
        );
    }
}
