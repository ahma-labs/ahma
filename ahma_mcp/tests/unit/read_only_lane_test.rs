//! The read-only lane is the kernel's word, not ahma's (SPEC R2.7.4).
//!
//! A command in the read-only lane skips the workspace write queue — `git
//! status` answers during a ten-minute build. That is only safe if the command
//! genuinely cannot write the workspace, so the lane is spawned with a sandbox
//! profile granting the workspace read and execute access only. These tests
//! assert the kernel refuses the write, not that a classifier said "read".

use ahma_mcp::sandbox::{Sandbox, SandboxMode};
use tempfile::TempDir;

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn strict_sandbox(scope: &TempDir) -> Sandbox {
    let sandbox = Sandbox::new(
        vec![scope.path().to_path_buf()],
        SandboxMode::Strict,
        false,
        false,
        false,
    )
    .expect("strict sandbox");
    sandbox.set_roots_received(true);
    let _ = sandbox.commit_existing_scopes();
    sandbox
}

/// Test mode has no kernel enforcement, so it has no read-only lane either.
#[test]
fn a_test_mode_sandbox_offers_no_read_only_lane() {
    let scope = TempDir::new().unwrap();
    let sandbox = Sandbox::new(
        vec![scope.path().to_path_buf()],
        SandboxMode::Test,
        false,
        false,
        false,
    )
    .unwrap();
    assert!(!sandbox.can_enforce_read_only());
    assert!(
        sandbox
            .create_read_only_command("ls", &[], scope.path())
            .is_err(),
        "an unenforceable read-only command must be refused, never built unenforced"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_read_only_command_reads_the_workspace_but_cannot_write_it() {
    let scope = TempDir::new().unwrap();
    std::fs::write(scope.path().join("a.txt"), "hello-read-lane").unwrap();
    let sandbox = strict_sandbox(&scope);
    if !sandbox.can_enforce_read_only() {
        // Landlock is unavailable on this kernel: the lane correctly does not
        // exist (every command is exclusive), and there is nothing to enforce.
        eprintln!("Landlock unavailable — read-only lane disabled, as required");
        return;
    }

    let read = sandbox
        .create_read_only_command("cat", &["a.txt".to_string()], scope.path())
        .unwrap()
        .output()
        .await
        .unwrap();
    assert!(read.status.success(), "reading must work: {read:?}");
    assert_eq!(String::from_utf8_lossy(&read.stdout), "hello-read-lane");

    let write = sandbox
        .create_read_only_command(
            "sh",
            &["-c".to_string(), "echo x > b.txt".to_string()],
            scope.path(),
        )
        .unwrap()
        .output()
        .await
        .unwrap();
    assert!(
        !write.status.success(),
        "the kernel must refuse the write: {write:?}"
    );
    assert!(
        !scope.path().join("b.txt").exists(),
        "nothing may be written"
    );

    // The same write through the ordinary (exclusive-lane) spawn succeeds, so
    // the refusal above is the read-only profile, not the scope.
    let ok = sandbox
        .create_command(
            "sh",
            &["-c".to_string(), "echo x > c.txt".to_string()],
            scope.path(),
        )
        .unwrap()
        .output()
        .await
        .unwrap();
    assert!(ok.status.success(), "{ok:?}");
    assert!(scope.path().join("c.txt").exists());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn git_status_runs_in_the_read_only_lane() {
    let scope = TempDir::new().unwrap();
    let sandbox = strict_sandbox(&scope);
    if !sandbox.can_enforce_read_only() || which_git().is_none() {
        eprintln!("Landlock or git unavailable — nothing to assert");
        return;
    }
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .env("HOME", scope.path())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(scope.path())
        .status()
        .unwrap();
    assert!(init.success());
    std::fs::write(scope.path().join("f.txt"), "x").unwrap();

    // Keep git away from the host's own configuration: on Linux the sandbox
    // confines reads to the workspace, so `~/.gitconfig` is unreadable there
    // (a separate, pre-existing limit this test is not about).
    let isolate = |mut cmd: tokio::process::Command| {
        cmd.env("HOME", scope.path())
            .env("GIT_CONFIG_NOSYSTEM", "1");
        cmd
    };
    // The ordinary lane runs git too — it used to die opening /dev/null.
    let normal = isolate(
        sandbox
            .create_command(
                "git",
                &["status".to_string(), "--porcelain".to_string()],
                scope.path(),
            )
            .unwrap(),
    )
    .output()
    .await
    .unwrap();
    assert!(normal.status.success(), "{normal:?}");
    let out = isolate(
        sandbox
            .create_read_only_command(
                "git",
                &["status".to_string(), "--porcelain".to_string()],
                scope.path(),
            )
            .unwrap(),
    )
    .output()
    .await
    .unwrap();
    assert!(
        out.status.success(),
        "git status must work without writing the index (GIT_OPTIONAL_LOCKS=0): {out:?}"
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("f.txt"));
}

#[cfg(target_os = "linux")]
fn which_git() -> Option<()> {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| ())
}

/// On macOS the read-only lane is a Seatbelt profile with no write rule for the
/// workspace, while the ordinary profile keeps one.
#[cfg(target_os = "macos")]
#[test]
fn the_read_only_seatbelt_profile_grants_no_workspace_write() {
    let scope = TempDir::new().unwrap();
    let sandbox = strict_sandbox(&scope);
    let canonical = dunce::canonicalize(scope.path()).unwrap();
    let write_rule = format!("(allow file-write* (subpath \"{}\"))", canonical.display());
    let normal = sandbox.generate_seatbelt_profile_test(&canonical);
    assert!(normal.contains(&write_rule), "{normal}");
    let read_only = sandbox.generate_read_only_seatbelt_profile_test(&canonical);
    assert!(!read_only.contains(&write_rule), "{read_only}");
    assert!(
        read_only.contains(&format!(
            "(allow file-read* (subpath \"{}\"))",
            canonical.display()
        )),
        "{read_only}"
    );
}
