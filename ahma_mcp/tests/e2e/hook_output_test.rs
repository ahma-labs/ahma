//! What `ahma hooks run-shell` hands the harness — stdout, stderr and exit
//! status — for a success, a failure, a time limit and a refusal the
//! command's own output names (SPEC R-SETUP.2, R-PERM.3). Pinned so the
//! path a hooked command runs on can change without the harness noticing.
#![cfg(unix)]

use base64::Engine as _;
use std::path::Path;
use std::process::Command;

/// What the harness reads back from one hooked call.
#[derive(Debug, PartialEq, Eq)]
struct Answer {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

fn hooked(home: &Path, ws: &Path, command: &str, timeout_ms: Option<u64>) -> Answer {
    let bin = ahma_mcp::test_utils::cli::build_binary_cached("ahma_bin", "ahma");
    let payload = serde_json::json!({
        "cwd": ws,
        "command": command,
        "session_id": "pin-session",
        "intent": { "timeout_ms": timeout_ms },
    });
    let out = Command::new(bin)
        .args(["hooks", "run-shell", "--payload-base64"])
        .arg(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string()))
        .env("AHMA_TEST_HOME", home)
        // What a harness sees: no backtrace after the error line, whatever
        // this test run's own `RUST_BACKTRACE` is (CI sets it).
        .env_remove("RUST_BACKTRACE")
        .env_remove("RUST_LIB_BACKTRACE")
        .current_dir(ws)
        .output()
        .expect("run ahma hooks run-shell");
    Answer {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        // A nested run says once that the outer sandbox is the control
        // (SPEC R7.6); that line depends on where the test runs.
        stderr: String::from_utf8_lossy(&out.stderr)
            .lines()
            .filter(|l| !l.contains("outer sandbox") && !l.contains("another sandbox"))
            .map(|l| format!("{l}\n"))
            .collect(),
        code: out.status.code(),
    }
}

fn workspace() -> (tempfile::TempDir, tempfile::TempDir, std::path::PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(ws.path().join(".git")).unwrap();
    let path = dunce::canonicalize(ws.path()).unwrap();
    (home, ws, path)
}

#[test]
fn a_hooked_success_is_its_output_on_stdout() {
    let (home, _ws, ws) = workspace();
    let answer = hooked(
        home.path(),
        &ws,
        "printf 'out\\n'; printf 'err\\n' >&2; printf 'last'",
        None,
    );
    assert_eq!(
        answer,
        Answer {
            stdout: "out\nerr\nlast\n".into(),
            stderr: String::new(),
            code: Some(0),
        }
    );
}

#[test]
fn a_hooked_failure_names_its_exit_code_and_output() {
    let (home, _ws, ws) = workspace();
    let answer = hooked(home.path(), &ws, "printf 'out\\n'; exit 3", None);
    assert_eq!(
        answer,
        Answer {
            stdout: String::new(),
            stderr: "Error: Command failed with exit code 3: stderr: , stdout: out\n\n".into(),
            code: Some(1),
        }
    );
}

#[test]
fn a_hooked_command_stopped_at_its_limit_says_who_and_what_it_printed() {
    let (home, _ws, ws) = workspace();
    let answer = hooked(
        home.path(),
        &ws,
        "printf 'before the limit\\n'; sleep 30",
        Some(1000),
    );
    assert_eq!(
        answer,
        Answer {
            stdout: String::new(),
            stderr: "Error: ended by ahma after 1 s (the harness's own timeout for this call). \
                     Ask for a longer `timeout` for this call, or run it in the background.\n\
                     \n\
                     Output before it was stopped:\n\
                     before the limit\n\n"
                .into(),
            code: Some(1),
        }
    );
}

#[test]
fn a_hooked_refusal_its_output_names_is_a_grant_to_ask_for() {
    let (home, _ws, ws) = workspace();
    let outside = ahma_mcp::test_utils::path_helpers::test_out_of_scope_path();
    let line = format!(
        "printf 'touch: {}: Operation not permitted\\n' >&2; exit 1",
        outside.display()
    );
    let answer = hooked(home.path(), &ws, &line, None);
    assert_eq!(answer.stdout, "");
    assert_eq!(answer.code, Some(1));
    let first = format!(
        "Error: Command failed with exit code 1: stderr: , stdout: touch: {}: Operation not \
         permitted\n",
        outside.display()
    );
    assert!(
        answer.stderr.starts_with(&first),
        "{:?}\nwanted it to start with {first:?}",
        answer.stderr
    );
    assert!(
        answer.stderr.contains("/nonexistent_scope"),
        "names the grant: {}",
        answer.stderr
    );
}

/// SPEC R2.1: a SIGTERM to the hook — the harness giving up on the call —
/// takes the hooked command's whole process tree with it, a grandchild
/// included, and the hook dies of that signal.
#[test]
fn a_signalled_hook_takes_its_command_tree_with_it() {
    use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
    use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
    let (home, _ws, ws) = workspace();
    let pid_file = ws.join("sleep.pid");
    let bin = ahma_mcp::test_utils::cli::build_binary_cached("ahma_bin", "ahma");
    let mut hook = Command::new(bin)
        .args(["hooks", "run-shell", "--cwd"])
        .arg(&ws)
        .args(["--session-id", "pin-session", "--command"])
        .arg(format!(
            "sleep 300 & echo $! > '{}'; wait",
            pid_file.display()
        ))
        .env("AHMA_TEST_HOME", home.path())
        .current_dir(&ws)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // Its own group, so the cleanup below can take everything with it.
        .process_group(0)
        .spawn()
        .expect("spawn ahma hooks run-shell");
    let group = hook.id() as i32;
    // Whatever happens below, nothing this test started outlives it: the
    // hook's group, and the group ahma starts the command in.
    struct KillGroups(Vec<i32>);
    impl Drop for KillGroups {
        fn drop(&mut self) {
            for group in &self.0 {
                // SAFETY: signals process groups this test caused to exist.
                unsafe { libc::kill(-group, libc::SIGKILL) };
            }
        }
    }
    let mut cleanup = KillGroups(vec![group]);

    let deadline = std::time::Instant::now() + TestTimeouts::get(TimeoutCategory::ToolCall);
    let sleep_pid: i32 = loop {
        if let Some(pid) = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            break pid;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the command never started"
        );
        std::thread::sleep(TestTimeouts::poll_interval());
    };
    // SAFETY: reads the group of a process this test caused to exist.
    let command_group = unsafe { libc::getpgid(sleep_pid) };
    if command_group > 0 {
        cleanup.0.push(command_group);
    }
    // SAFETY: signal 0 only asks whether the pid exists.
    assert_eq!(unsafe { libc::kill(sleep_pid, 0) }, 0, "the sleep runs");

    // SAFETY: signals the hook process this test spawned.
    unsafe { libc::kill(hook.id() as i32, libc::SIGTERM) };
    let status = hook.wait().expect("the hook ends");
    assert_eq!(status.signal(), Some(libc::SIGTERM), "{status:?}");

    let deadline = std::time::Instant::now() + TestTimeouts::get(TimeoutCategory::Cleanup);
    // SAFETY: as above.
    while unsafe { libc::kill(sleep_pid, 0) } == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the grandchild `sleep` {sleep_pid} outlived the hook"
        );
        std::thread::sleep(TestTimeouts::poll_interval());
    }
}
