//! The per-user hub, end to end (SPEC R-HUB.1, R-HUB.3).
//!
//! E2E rather than in-process by necessity: what is under test is that *one
//! process* serves both halves on one socket, that a second one recognises the
//! first and stands down, and that the process actually exits when nothing is
//! attached. None of those are observable without a real process.

use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use ahma_mcp::test_utils::cli::{build_binary_cached, test_command};
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// The socket for one test, isolated from the developer's live hub (R-ISO.1).
struct Rendezvous {
    dir: tempfile::TempDir,
    hub: PathBuf,
}

impl Rendezvous {
    fn log_path(&self) -> PathBuf {
        self.dir.path().join("hub.log")
    }

    /// Whatever the hub managed to say before it gave up, for a failure
    /// message that names the cause instead of the symptom.
    fn log(&self) -> String {
        std::fs::read_to_string(self.log_path()).unwrap_or_default()
    }
}

impl Rendezvous {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        // A runtime directory is owner-only, and the hub refuses to use one
        // that is not (SPEC R-HUB.2). `tempdir()` inherits the umask, which
        // is typically 022, so tighten it the way `runtime_dir()` does.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
                .expect("chmod 700 the test runtime dir");
        }
        let hub = dir.path().join("hub.sock");
        Self { dir, hub }
    }
}

fn spawn_hub(binary: &Path, r: &Rendezvous, idle_secs: u64) -> std::process::Child {
    let mut cmd = test_command(binary);
    cmd.args(["hub", "--unix-socket-path", &r.hub.to_string_lossy()]);
    cmd.args(["--idle-timeout", &idle_secs.to_string()]);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    // Keep the hub's own diagnosis: a test that only reports "the socket
    // never appeared" makes every failure a fresh investigation.
    cmd.arg("--log-to-stderr");
    let log = std::fs::File::create(r.log_path()).expect("create hub log");
    cmd.stderr(Stdio::from(log));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Die with the test rather than outliving it as an orphan.
        cmd.process_group(0);
    }
    cmd.spawn().expect("spawn ahma hub")
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

/// One process serves both halves — the MCP endpoint and the event stream —
/// on one owner-only socket (SPEC R-HUB.2). Two singletons with two
/// lifetimes, and then one process with two sockets, is what this replaces.
///
/// On every OS: it is an `AF_UNIX` socket on Windows too. The mode check
/// alone is Unix-only, because Windows has no mode bits.
#[tokio::test]
async fn the_hub_serves_both_halves_on_one_socket() {
    let binary = build_binary_cached("ahma_bin", "ahma");
    let r = Rendezvous::new();
    let mut child = spawn_hub(&binary, &r, 3);

    if !wait_for(&r.hub, true).await {
        let log = r.log();
        kill(&mut child);
        panic!("hub did not bind its socket; it said:\n{log}");
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&r.hub).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o600,
            "{} must be owner-only: anything that can connect runs commands as this user",
            r.hub.display()
        );
    }

    // The event stream: a one-shot query returns an (empty) instance list.
    let listed = ahma_common::hub::list_instances_at(&r.hub).await;
    assert!(listed.is_ok(), "the event stream must answer: {listed:?}");

    // The MCP endpoint's `/health`, on the same socket.
    let health = ahma_common::doctor::probe_hub(&r.hub).await;
    assert!(
        matches!(
            health,
            ahma_common::doctor::HubStatus::Running { version: Some(_) }
        ),
        "/health must answer on the same socket: {health:?}"
    );

    let sockets: Vec<_> = std::fs::read_dir(r.dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".sock"))
        .collect();
    assert_eq!(sockets, ["hub.sock"], "one socket, nothing beside it");

    kill(&mut child);
}

/// A second hub recognises the first and stands down. Exactly one per user
/// is the whole point, and a losing race is an ordinary outcome, not a crash.
#[tokio::test]
async fn a_second_hub_stands_down() {
    let binary = build_binary_cached("ahma_bin", "ahma");
    let r = Rendezvous::new();
    let mut first = spawn_hub(&binary, &r, 30);
    if !wait_for(&r.hub, true).await {
        let log = r.log();
        kill(&mut first);
        panic!("first hub must bind; it said:\n{log}");
    }

    let mut second = spawn_hub(&binary, &r, 30);
    let status = tokio::task::spawn_blocking(move || second.wait())
        .await
        .expect("join")
        .expect("second hub exits");
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

/// With nothing attached the hub goes, and takes its socket with it — so a
/// later client sees a clean absence rather than a stale file.
#[tokio::test]
async fn the_hub_exits_when_nothing_is_attached() {
    let binary = build_binary_cached("ahma_bin", "ahma");
    let r = Rendezvous::new();
    let child = spawn_hub(&binary, &r, 2);
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

    assert!(started, "hub must start; it said:\n{}", r.log());
    assert!(exited, "an idle hub must exit on its own");
    assert!(
        wait_for(&r.hub, false).await,
        "and unlink the socket it bound"
    );
}

/// A hub whose socket is removed gives way (SPEC R-HUB.3). No client can
/// find it by that path any more — `$XDG_RUNTIME_DIR` cleared at logout does
/// this — and a hub that stayed would sit beside the one the next client
/// starts. It must go on its own, and must not take anything with it.
#[tokio::test]
async fn a_hub_whose_socket_is_removed_gives_way() {
    let binary = build_binary_cached("ahma_bin", "ahma");
    let r = Rendezvous::new();
    // Long enough that only the lost socket can explain an exit.
    let child = spawn_hub(&binary, &r, 3600);
    let started = wait_for(&r.hub, true).await;
    if started {
        std::fs::remove_file(&r.hub).expect("remove the hub's socket");
    }

    let exited = tokio::task::spawn_blocking(move || {
        let mut child = child;
        let deadline = std::time::Instant::now() + TestTimeouts::get(TimeoutCategory::ProcessSpawn);
        while std::time::Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) => std::thread::sleep(TestTimeouts::poll_interval()),
                Err(_) => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        false
    })
    .await
    .expect("join");

    assert!(started, "hub must start; it said:\n{}", r.log());
    assert!(
        exited,
        "a hub nobody can reach must exit on its own; it said:\n{}",
        r.log()
    );
    assert!(
        r.log().contains("removed or replaced"),
        "and say why: {}",
        r.log()
    );
}

/// A draining hub starts its successor and hands over to it (SPEC R-HUB.5):
/// with nothing in flight the old hub goes at once, and the successor it
/// pre-spawned — waiting on the lock — is serving on the same socket.
#[tokio::test]
async fn a_draining_hub_hands_over_to_its_successor() {
    let binary = build_binary_cached("ahma_bin", "ahma");
    let r = Rendezvous::new();
    let child = spawn_hub(&binary, &r, 30);
    let started = wait_for(&r.hub, true).await;
    let socket = r.hub.to_string_lossy().into_owned();
    let drain_accepted =
        started && ahma_mcp::shell::modes::server::trigger_bridge_drain(&socket).await;

    let exited = tokio::task::spawn_blocking(move || {
        let mut child = child;
        let deadline = std::time::Instant::now() + TestTimeouts::get(TimeoutCategory::ProcessSpawn);
        while std::time::Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) => std::thread::sleep(TestTimeouts::poll_interval()),
                Err(_) => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        false
    })
    .await
    .expect("join");

    assert!(started, "hub must start; it said:\n{}", r.log());
    assert!(drain_accepted, "the hub must accept a drain request");
    assert!(
        exited,
        "with nothing in flight a draining hub goes; it said:\n{}",
        r.log()
    );
    assert!(
        r.log().contains("successor started"),
        "and it started its successor first: {}",
        r.log()
    );

    // The successor, not a client, now serves the socket.
    let deadline = std::time::Instant::now() + TestTimeouts::get(TimeoutCategory::ProcessSpawn);
    let mut serving = false;
    while std::time::Instant::now() < deadline {
        if matches!(
            ahma_common::doctor::probe_hub(&r.hub).await,
            ahma_common::doctor::HubStatus::Running { .. }
        ) {
            serving = true;
            break;
        }
        tokio::time::sleep(TestTimeouts::poll_interval()).await;
    }

    // The successor is detached from this test, so stop it by asking.
    if let Ok(mut stream) = ahma_common::hub::connect_to_hub_at(&r.hub).await {
        let _ =
            ahma_common::hub::send_msg(&mut stream, &ahma_common::hub::ClientMsg::Shutdown).await;
    }
    assert!(serving, "the successor must take over the socket");
    assert!(
        wait_for(&r.hub, false).await,
        "and the successor goes when asked"
    );
}

/// Put a copy of `binary` at `dest`, with a modification time `ahead` of now,
/// the way an install writes a new file over the path.
fn install_copy(binary: &Path, dest: &Path, ahead: std::time::Duration) {
    std::fs::copy(binary, dest).expect("copy the ahma binary");
    // A copy may keep the source's timestamps (macOS clones do): set one
    // so the file is unmistakably newer, as a real install's would be.
    std::fs::File::options()
        .write(true)
        .open(dest)
        .and_then(|f| f.set_modified(std::time::SystemTime::now() + ahead))
        .expect("stamp the installed copy");
}

/// A hub notices a new install written over its own executable and hands
/// over to it (SPEC R-HUB.5). Nothing tells it: `cargo install`, brew and
/// the install scripts just write the file, which is why the hub looks.
#[tokio::test]
async fn a_hub_hands_over_when_its_executable_is_replaced() {
    let binary = build_binary_cached("ahma_bin", "ahma");
    let r = Rendezvous::new();
    let installed = r
        .dir
        .path()
        .join(format!("ahma{}", std::env::consts::EXE_SUFFIX));
    install_copy(&binary, &installed, std::time::Duration::ZERO);
    let child = spawn_hub(&installed, &r, 30);
    let started = wait_for(&r.hub, true).await;

    if started {
        // How an installer replaces a running binary on every OS, Windows
        // included: move the running file aside, write the new one.
        let aside = installed.with_extension("old");
        std::fs::rename(&installed, &aside).expect("move the running binary aside");
        install_copy(&binary, &installed, std::time::Duration::from_secs(60));
    }

    let exited = tokio::task::spawn_blocking(move || {
        let mut child = child;
        let deadline = std::time::Instant::now() + TestTimeouts::get(TimeoutCategory::ProcessSpawn);
        while std::time::Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) => std::thread::sleep(TestTimeouts::poll_interval()),
                Err(_) => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        false
    })
    .await
    .expect("join");

    let deadline = std::time::Instant::now() + TestTimeouts::get(TimeoutCategory::ProcessSpawn);
    let mut serving = false;
    while exited && std::time::Instant::now() < deadline {
        if matches!(
            ahma_common::doctor::probe_hub(&r.hub).await,
            ahma_common::doctor::HubStatus::Running { .. }
        ) {
            serving = true;
            break;
        }
        tokio::time::sleep(TestTimeouts::poll_interval()).await;
    }
    if let Ok(mut stream) = ahma_common::hub::connect_to_hub_at(&r.hub).await {
        let _ =
            ahma_common::hub::send_msg(&mut stream, &ahma_common::hub::ClientMsg::Shutdown).await;
    }

    assert!(started, "hub must start; it said:\n{}", r.log());
    assert!(
        exited,
        "a hub whose binary was replaced hands over; it said:\n{}",
        r.log()
    );
    assert!(
        r.log().contains("was replaced by a new install"),
        "and says why: {}",
        r.log()
    );
    assert!(serving, "the new install serves the socket");
    assert!(wait_for(&r.hub, false).await, "and goes when asked");
}
