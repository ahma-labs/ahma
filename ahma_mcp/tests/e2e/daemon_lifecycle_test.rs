//! The per-user daemon, end to end (SPEC R-DAEMON.1, R-DAEMON.3).
//!
//! E2E rather than in-process by necessity: what is under test is that *one
//! process* serves both rendezvous points, that a second one recognises the
//! first and stands down, and that the process actually exits when nothing is
//! attached. None of those are observable without a real process.

use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use ahma_mcp::test_utils::cli::{build_binary_cached, test_command};
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Sockets for one test, isolated from the developer's live daemon (R-ISO.1).
struct Rendezvous {
    dir: tempfile::TempDir,
    hub: PathBuf,
    mcp: PathBuf,
}

impl Rendezvous {
    fn log_path(&self) -> PathBuf {
        self.dir.path().join("daemon.log")
    }

    /// Whatever the daemon managed to say before it gave up, for a failure
    /// message that names the cause instead of the symptom.
    fn log(&self) -> String {
        std::fs::read_to_string(self.log_path()).unwrap_or_default()
    }
}

impl Rendezvous {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        // A runtime directory is owner-only, and the daemon refuses to use one
        // that is not (SPEC R-DAEMON.2). `tempdir()` inherits the umask, which
        // is typically 022, so tighten it the way `runtime_dir()` does.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
                .expect("chmod 700 the test runtime dir");
        }
        let hub = dir.path().join("daemon.sock");
        let mcp = dir.path().join("mcp.sock");
        Self { dir, hub, mcp }
    }
}

fn spawn_daemon(binary: &Path, r: &Rendezvous, idle_secs: u64) -> std::process::Child {
    let mut cmd = test_command(binary);
    cmd.args([
        "daemon",
        "--daemon-socket",
        &r.hub.to_string_lossy(),
        "--unix-socket-path",
        &r.mcp.to_string_lossy(),
    ]);
    cmd.args(["--idle-timeout", &idle_secs.to_string()]);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    // Keep the daemon's own diagnosis: a test that only reports "the socket
    // never appeared" makes every failure a fresh investigation.
    cmd.arg("--log-to-stderr");
    let log = std::fs::File::create(r.log_path()).expect("create daemon log");
    cmd.stderr(Stdio::from(log));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Die with the test rather than outliving it as an orphan.
        cmd.process_group(0);
    }
    cmd.spawn().expect("spawn ahma daemon")
}

async fn wait_for(path: &Path, exists: bool) -> bool {
    let deadline = std::time::Instant::now() + TestTimeouts::get(TimeoutCategory::ProcessSpawn);
    while std::time::Instant::now() < deadline {
        if path.exists() == exists {
            return true;
        }
        tokio::time::sleep(TestTimeouts::poll_interval()).await;
    }
    false
}

fn kill(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// One process binds *both* rendezvous points, and both are owner-only. Two
/// singletons with two lifetimes is what this replaces.
#[cfg(unix)]
#[tokio::test]
async fn the_daemon_serves_both_rendezvous_points() {
    let binary = build_binary_cached("ahma_bin", "ahma");
    let r = Rendezvous::new();
    let mut child = spawn_daemon(&binary, &r, 3);

    let hub_up = wait_for(&r.hub, true).await;
    let mcp_up = wait_for(&r.mcp, true).await;
    if !hub_up || !mcp_up {
        let log = r.log();
        kill(&mut child);
        panic!(
            "daemon did not bind both sockets (hub={hub_up}, mcp={mcp_up}); daemon said:\n{log}"
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [&r.hub, &r.mcp] {
            let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode,
                0o600,
                "{} must be owner-only: anything that can connect runs commands as this user",
                path.display()
            );
        }
    }

    // The hub answers: a one-shot query returns an (empty) instance list.
    let listed = ahma_common::daemon_hub::list_instances_at(&r.hub).await;
    assert!(
        listed.is_ok(),
        "the hub must answer on its socket: {listed:?}"
    );

    kill(&mut child);
}

/// A second daemon recognises the first and stands down. Exactly one per user
/// is the whole point, and a losing race is an ordinary outcome, not a crash.
#[cfg(unix)]
#[tokio::test]
async fn a_second_daemon_stands_down() {
    let binary = build_binary_cached("ahma_bin", "ahma");
    let r = Rendezvous::new();
    let mut first = spawn_daemon(&binary, &r, 30);
    if !wait_for(&r.hub, true).await {
        let log = r.log();
        kill(&mut first);
        panic!("first daemon must bind; it said:\n{log}");
    }

    let mut second = spawn_daemon(&binary, &r, 30);
    let status = tokio::task::spawn_blocking(move || second.wait())
        .await
        .expect("join")
        .expect("second daemon exits");
    assert!(
        status.success(),
        "losing the race is an ordinary outcome, not a failure: {status:?}"
    );
    assert!(
        r.hub.exists(),
        "the loser must not remove the winner's socket"
    );

    kill(&mut first);
}

/// With nothing attached the daemon goes, and takes its sockets with it — so a
/// later client sees a clean absence rather than a stale file.
#[cfg(unix)]
#[tokio::test]
async fn the_daemon_exits_when_nothing_is_attached() {
    let binary = build_binary_cached("ahma_bin", "ahma");
    let r = Rendezvous::new();
    let child = spawn_daemon(&binary, &r, 2);
    let started = wait_for(&r.hub, true).await;

    // One owner for the child on every path, so it is always reaped: the
    // waiter kills it if it overstays, and returns whether it left on its own.
    let exited = tokio::task::spawn_blocking(move || {
        let mut child = child;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
                Err(_) => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        false
    })
    .await
    .expect("join");

    assert!(started, "daemon must start; it said:\n{}", r.log());
    assert!(exited, "an idle daemon must exit on its own");
    assert!(
        wait_for(&r.hub, false).await,
        "and unlink the hub socket it bound"
    );
    assert!(wait_for(&r.mcp, false).await, "and the MCP socket it bound");
}
