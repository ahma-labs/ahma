//! A file can be moved or linked to another directory inside the scope.
//!
//! Landlock's first ABI refuses every rename or link that changes a file's
//! directory with `EXDEV` ("Invalid cross-device link"), even when both
//! directories are writable. `mv` hides it by falling back to copy-and-delete,
//! but any program calling `rename(2)` across directories failed: the suite's
//! own trash tests failed this way when run through ahma on Linux. The
//! ruleset grants the "refer" right (ABI 2) on writable areas so they do not.
#![cfg(target_os = "linux")]

use ahma_mcp::sandbox::{Sandbox, SandboxMode};
use tempfile::TempDir;

#[tokio::test]
async fn a_file_can_be_linked_into_another_directory_of_the_scope() {
    let scope = TempDir::new().unwrap();
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
    if !sandbox.can_enforce_read_only() {
        eprintln!("Landlock unavailable on this kernel: nothing to enforce");
        return;
    }
    // `ln` has no copy fallback, so it fails exactly where rename(2) does.
    let out = sandbox
        .create_command(
            "sh",
            &[
                "-c".to_string(),
                "mkdir sub && echo x > a && ln a sub/b && echo linked".to_string(),
            ],
            scope.path(),
        )
        .expect("build command")
        .output()
        .await
        .expect("run command");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.contains("linked"),
        "a link inside the scope was refused: {stderr}"
    );
    assert!(scope.path().join("sub/b").exists());
}
