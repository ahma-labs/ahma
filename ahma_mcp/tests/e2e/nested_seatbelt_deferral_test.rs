//! SPEC R7.6 — an ahma that is itself inside a macOS Seatbelt profile cannot
//! apply its own: the kernel refuses a nested profile whenever the outer one
//! denies anything at all. ahma must then defer to the outer sandbox, say so,
//! and still run the command — never fail with the child's opaque
//! `sandbox-exec: sandbox_apply: Operation not permitted`.
//!
//! This is the dogfooding failure of 2026-09: `cargo nextest run` executed
//! *through* ahma's `run_terminal_command` failed every in-process test that
//! spawns a command (`async_first_contract_e2e_test` first), because those
//! tests build a `Sandbox` directly and never pass through the server's
//! startup probe, which was the only place nesting was handled.
//!
//! ## How the nesting is reproduced
//!
//! The test process re-executes itself under `sandbox-exec` with a profile
//! that allows everything except one path that does not exist — the smallest
//! profile the kernel treats as "denies something" — and runs the inner half
//! there. Re-exec rather than `sandbox_init` in-process because confinement is
//! irrevocable and under `cargo test` (one process per binary) it would leak
//! into every other test in the binary. `cargo nextest` runs each test in its
//! own process either way.
//!
//! Self-skips when `sandbox-exec` cannot run at all. When the test runner is
//! *already* confined — this suite run through ahma, which is exactly the
//! scenario — the outer half is redundant and the inner assertions run
//! directly.

#![cfg(target_os = "macos")]

use ahma_common::timeouts::TestTimeouts;
use ahma_mcp::sandbox::{HostSandbox, OUTER_SANDBOX_PID_ENV, process_is_seatbelt_confined};
use ahma_mcp::test_utils::in_process::create_in_process_mcp_with_scope;
use ahma_mcp::utils::logging::init_test_logging;
use anyhow::{Context, Result};
use rmcp::model::CallToolRequestParams;
use serde_json::json;
use std::process::Command;

/// Set on the re-executed child so it runs the inner half.
const INNER_MARKER: &str = "AHMA_NESTED_SEATBELT_DEFERRAL_INNER";

/// The full libtest path of the test, as `--exact` needs it.
const TEST_PATH: &str = "nested_seatbelt_deferral_test::a_command_still_runs_when_ahma_cannot_nest_its_seatbelt_profile";

/// Allows everything, denies one nonexistent path: measured on macOS 26, that
/// single `deny` is enough for the kernel to refuse a nested profile.
const OUTER_PROFILE: &str =
    r#"(version 1)(allow default)(deny file-write* (subpath "/nonexistent-ahma-nesting-probe"))"#;

fn sandbox_exec_usable() -> bool {
    Command::new("sandbox-exec")
        .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The inner half: runs confined. Builds an in-process server exactly the way
/// the unit suites do, runs a command, and checks both the outcome and the
/// disclosure.
async fn inner() -> Result<()> {
    init_test_logging();
    assert!(
        process_is_seatbelt_confined(),
        "the inner half must run inside a Seatbelt profile; `sandbox_check` says it does not"
    );

    let temp = tempfile::tempdir()?;
    let ahma_dir = temp.path().join(".ahma");
    tokio::fs::create_dir_all(&ahma_dir).await?;
    let mcp = create_in_process_mcp_with_scope(&ahma_dir, vec![temp.path().to_path_buf()]).await?;

    // The sandbox knows, before any spawn, that it cannot nest — and names
    // the outer ahma that spawned it (SPEC R7.1) when one stamped us. A
    // confined runner that was not started by ahma (e.g. a developer wrapping
    // `cargo nextest` in `sandbox-exec` by hand) still defers, unnamed.
    let sandbox = mcp.service.adapter.sandbox();
    let outer = sandbox.deferred_to_outer_sandbox();
    let stamped_by_ahma = std::env::var_os(OUTER_SANDBOX_PID_ENV).is_some();
    if stamped_by_ahma {
        assert_eq!(
            outer,
            Some(HostSandbox::Ahma),
            "a confined process whose nesting is refused must defer, and name the outer ahma"
        );
    } else {
        assert!(
            outer.is_some(),
            "a confined process whose nesting is refused must defer even when the outer \
             sandbox cannot be named"
        );
    }
    assert!(
        !sandbox.is_enforced(),
        "a deferring sandbox must not claim to be enforcing"
    );
    let scope = sandbox.scope_json(ahma_mcp::sandbox::ScopeSource::Explicit);
    assert_eq!(scope["active"], "deferred_to_host", "{scope}");
    let disclosure = scope["active_disclosure"].as_str().unwrap_or_default();
    assert!(
        disclosure.contains("DEFERRING"),
        "the disclosure must say ahma is deferring: {disclosure}"
    );
    if stamped_by_ahma {
        assert!(
            disclosure.contains("an outer ahma") && disclosure.contains("run_terminal_command"),
            "the disclosure must name the outer ahma and the path that produced the nesting: \
             {disclosure}"
        );
    }

    // And the command runs — inside the outer kernel boundary, not refused by
    // an inner profile the kernel will not apply.
    let params = CallToolRequestParams::new("run_terminal_command")
        .with_arguments(json!({ "command": "true" }).as_object().cloned().unwrap());
    let result = tokio::time::timeout(TestTimeouts::scale_secs(30), mcp.client.call_tool(params))
        .await
        .context("run_terminal_command must answer")??;
    let text: String = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect();
    assert!(
        text.contains("exit 0"),
        "the command must run and report its outcome, got: {text:?}"
    );
    assert!(
        !text.contains("sandbox_apply") && !text.contains("Operation not permitted"),
        "the kernel's refusal to nest must never surface as the command's failure: {text:?}"
    );

    let _ = mcp.client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn a_command_still_runs_when_ahma_cannot_nest_its_seatbelt_profile() -> Result<()> {
    if std::env::var_os(INNER_MARKER).is_some() {
        return inner().await;
    }
    if process_is_seatbelt_confined() {
        // The runner is already the scenario (this suite run through ahma's
        // `run_terminal_command`); re-executing would only nest deeper, and
        // `sandbox-exec` is refused here anyway. The outer ahma stamped us.
        eprintln!("test runner is already Seatbelt-confined: running the inner half directly");
        return inner().await;
    }
    if !sandbox_exec_usable() {
        eprintln!("SKIPPED: sandbox-exec cannot run here, so nesting cannot be reproduced");
        return Ok(());
    }

    let exe = std::env::current_exe().context("current_exe")?;
    let output = Command::new("sandbox-exec")
        .args(["-p", OUTER_PROFILE])
        .arg(&exe)
        .args(["--exact", TEST_PATH, "--nocapture", "--test-threads=1"])
        .env(INNER_MARKER, "1")
        // What ahma's own `run_terminal_command` stamps on the commands it
        // sandboxes — here, standing in for the outer ahma.
        .env(OUTER_SANDBOX_PID_ENV, std::process::id().to_string())
        .output()
        .context("re-exec under sandbox-exec")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the confined inner half failed (exit {:?})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        output.status.code()
    );
    assert!(
        stdout.contains("test result: ok. 1 passed"),
        "the inner half must have actually run and passed:\n{stdout}\n{stderr}"
    );
    Ok(())
}
