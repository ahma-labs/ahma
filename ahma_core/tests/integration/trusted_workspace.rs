//! Trusted folders (SPEC R-PERM.1.3): one "trust this folder?" answer replaces
//! the per-tool prompts for everything that runs inside the folder's sandbox —
//! and for nothing that reaches past it.
//!
//! Uses the same `AHMA_TEST_HOME` seam as `approvals_persistence` (one writer of
//! the env var per test process; nextest runs each test in its own process).

use ahma_core::approvals::{
    covered_by_trust, forget_tool_approvals, is_tool_allowed, is_workspace_trusted,
    remember_tool_approval, trust_workspace, untrust_workspace,
};
use tempfile::TempDir;

#[tokio::test]
async fn trusting_a_folder_allows_sandboxed_tools_but_not_boundary_crossing_ones() {
    let home = TempDir::new().unwrap();
    // SAFETY: set once, before any approvals call, in a process running only
    // this test.
    unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };

    let ws_dir = TempDir::new().unwrap();
    let other_dir = TempDir::new().unwrap();
    let ws = ws_dir.path();

    // Untrusted: nothing is allowed without asking.
    assert!(!is_workspace_trusted(ws));
    assert!(!is_tool_allowed(ws, "write_file").await);

    trust_workspace(ws).expect("trusting a folder persists");
    assert!(is_workspace_trusted(ws));

    // Inside the sandbox: covered.
    for tool in [
        "write_file",
        "run_terminal_command",
        "read_file",
        "cargo_build",
    ] {
        assert!(covered_by_trust(tool), "{tool}");
        assert!(
            is_tool_allowed(ws, tool).await,
            "{tool} runs inside the sandbox"
        );
    }
    // Past the boundary: still asks, even in a trusted folder.
    for tool in [
        "sandbox_grant",
        "logs_approve",
        "fetch_webpage",
        "github::create_issue",
    ] {
        assert!(!covered_by_trust(tool), "{tool}");
        assert!(!is_tool_allowed(ws, tool).await, "{tool} must still ask");
    }

    // Trust is per folder.
    assert!(!is_workspace_trusted(other_dir.path()));
    assert!(!is_tool_allowed(other_dir.path(), "write_file").await);

    // Withdrawing trust and forgetting grants are separate, and each says
    // whether it changed anything.
    remember_tool_approval(ws, "fetch_webpage").unwrap();
    assert_eq!(forget_tool_approvals(ws).unwrap(), 1);
    assert!(is_workspace_trusted(ws), "forgetting grants keeps trust");
    assert!(untrust_workspace(ws).unwrap());
    assert!(!untrust_workspace(ws).unwrap(), "already untrusted");
    assert!(!is_tool_allowed(ws, "write_file").await);
}
