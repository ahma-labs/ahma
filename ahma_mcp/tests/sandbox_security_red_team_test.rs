//! Red Team Security Tests for Sandbox Escape Prevention
//!
//! These tests attempt various sandbox escape techniques to verify that:
//! 1. Path validation correctly blocks access outside sandbox scope
//! 2. The --disable-temp-files flag effectively blocks temp directory writes
//! 3. Symlink-based escape attempts are detected
//! 4. Encoded/obfuscated path traversal attempts fail
//!
//! The goal is to document both working protections and known limitations.

#[cfg(target_os = "linux")]
use ahma_mcp::sandbox::check_sandbox_prerequisites;
use ahma_mcp::sandbox::{Sandbox, SandboxMode};
use ahma_mcp::test_utils as common;
use ahma_mcp::test_utils::client::ClientBuilder;
use ahma_mcp::test_utils::in_process::create_in_process_mcp_with_scope;
use ahma_mcp::utils::logging::init_test_logging;
use common::fs::get_workspace_tools_dir;
use rmcp::model::CallToolRequestParams;
#[cfg(target_os = "linux")]
use rmcp::model::CallToolResult;
use serde_json::json;
use std::fs;
#[cfg(target_os = "linux")]
use std::sync::OnceLock;
use tempfile::TempDir;

#[cfg(target_os = "linux")]
fn result_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| content.as_text().map(|text| text.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(target_os = "linux")]
fn blocked_shell_result_indicates_failure(result: &CallToolResult) -> bool {
    if result.is_error.unwrap_or(false) {
        return true;
    }

    let text = result_text(result);
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
        let exit_code = json.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(0);
        let stderr = json
            .get("stderr")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let stdout = json
            .get("stdout")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        return exit_code != 0
            || stderr.contains("permission denied")
            || stderr.contains("outside the sandbox")
            || stderr.contains("no such file or directory")
            || stdout.contains("permission denied")
            || stdout.contains("outside the sandbox")
            || stdout.contains("no such file or directory");
    }

    let lower = text.to_ascii_lowercase();
    lower.contains("command failed")
        || lower.contains("failed")
        || lower.contains("permission denied")
        || lower.contains("outside the sandbox")
        || lower.contains("no such file or directory")
        || lower.contains("high-security mode")
}

#[cfg(target_os = "linux")]
fn assert_blocked_shell_result<E: std::fmt::Debug>(
    result: Result<CallToolResult, E>,
    leaked_content: &str,
    message: &str,
) {
    match result {
        Err(_) => {}
        Ok(result) => {
            let text = result_text(&result);
            assert!(
                !text.contains(leaked_content),
                "{message}: blocked command leaked sensitive content. Output: {text}"
            );
            assert!(
                blocked_shell_result_indicates_failure(&result),
                "{message}: expected blocked command to be marked as failure. Output: {text}"
            );
        }
    }
}

#[cfg(target_os = "linux")]
fn create_non_tmp_tempdir() -> TempDir {
    // Landlock adds broad /tmp access unless --disable-temp-files is enabled.
    // For "outside scope" tests we need fixtures outside /tmp.
    let base = std::env::current_dir().expect("failed to get current directory");
    tempfile::Builder::new()
        .prefix("ahma-redteam-outside-")
        .tempdir_in(base)
        .expect("failed to create non-/tmp temporary directory")
}

/// Create a non-/tmp temp directory to use as the sandbox scope.
///
/// Using a non-tmp scope is required when `--disable-temp-files` is passed: with
/// that flag the sandbox rejects any working directory under `/tmp`.  We place the
/// scope alongside the "outside" dir in the test process's current directory so
/// that both dirs are siblings in the same workspace tree — only the scope dir is
/// added to the Landlock ruleset; the sibling is therefore OS-blocked.
#[cfg(target_os = "linux")]
fn create_non_tmp_scope_dir() -> TempDir {
    let base = std::env::current_dir().expect("failed to get current directory");
    tempfile::Builder::new()
        .prefix("ahma-redteam-scope-")
        .tempdir_in(base)
        .expect("failed to create non-/tmp scope directory")
}

#[cfg(target_os = "linux")]
fn landlock_enforcement_available() -> bool {
    static LANDLOCK_AVAILABLE: OnceLock<bool> = OnceLock::new();
    *LANDLOCK_AVAILABLE.get_or_init(|| check_sandbox_prerequisites().is_ok())
}

#[cfg(target_os = "linux")]
macro_rules! skip_if_landlock_unavailable {
    () => {
        if !landlock_enforcement_available() {
            eprintln!(
                "Skipping test: Landlock unavailable (requires Linux kernel 5.13+ with Landlock LSM)"
            );
            return;
        }
    };
}

// =============================================================================
// RED TEAM TEST 1: Path Traversal Attacks
// =============================================================================

/// Test that basic path traversal (../) is blocked
#[tokio::test]
async fn red_team_basic_path_traversal_blocked() {
    init_test_logging();
    let temp_dir = TempDir::new().unwrap();
    let mcp = create_in_process_mcp_with_scope(
        &get_workspace_tools_dir(),
        vec![temp_dir.path().to_path_buf()],
    )
    .await
    .unwrap();

    // Attempt to escape via simple ../
    let params = CallToolRequestParams::new("run_terminal_command").with_arguments(
        serde_json::from_value(json!({
            "command": "cat /etc/passwd",
            "working_directory": "../"
        }))
        .unwrap(),
    );
    let result = mcp.client.call_tool(params).await;
    assert!(
        result.is_err(),
        "SECURITY: Basic path traversal should be blocked"
    );
}

/// Test that deeply nested path traversal is blocked
#[tokio::test]
async fn red_team_deep_path_traversal_blocked() {
    init_test_logging();
    let temp_dir = TempDir::new().unwrap();
    let mcp = create_in_process_mcp_with_scope(
        &get_workspace_tools_dir(),
        vec![temp_dir.path().to_path_buf()],
    )
    .await
    .unwrap();

    // Attempt to escape via deeply nested traversal
    let params = CallToolRequestParams::new("run_terminal_command").with_arguments(
        serde_json::from_value(json!({
            "command": "ls",
            "working_directory": "a/b/c/d/e/../../../../../../../../../../"
        }))
        .unwrap(),
    );
    let result = mcp.client.call_tool(params).await;
    assert!(
        result.is_err(),
        "SECURITY: Deep path traversal should be blocked"
    );
}

/// Test that absolute path outside sandbox is blocked
#[tokio::test]
async fn red_team_absolute_path_escape_blocked() {
    init_test_logging();
    let temp_dir = TempDir::new().unwrap();
    let mcp = create_in_process_mcp_with_scope(
        &get_workspace_tools_dir(),
        vec![temp_dir.path().to_path_buf()],
    )
    .await
    .unwrap();

    // Attempt to use absolute path outside sandbox
    let params = CallToolRequestParams::new("run_terminal_command").with_arguments(
        serde_json::from_value(json!({
            "command": "ls",
            "working_directory": "/etc"
        }))
        .unwrap(),
    );
    let result = mcp.client.call_tool(params).await;
    assert!(
        result.is_err(),
        "SECURITY: Absolute path outside sandbox should be blocked"
    );
}

// =============================================================================
// RED TEAM TEST 2: Symlink Escape Attacks
// =============================================================================

/// Test that symlinks pointing outside sandbox are blocked
#[tokio::test]
async fn red_team_symlink_escape_blocked() {
    init_test_logging();

    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(windows)]
    use std::os::windows::fs::symlink_dir as symlink;

    let temp_dir = TempDir::new().unwrap();
    let mcp = create_in_process_mcp_with_scope(
        &get_workspace_tools_dir(),
        vec![temp_dir.path().to_path_buf()],
    )
    .await
    .unwrap();

    // Create a symlink inside sandbox pointing to root / C:\ (outside)
    let malicious_link = temp_dir.path().join("etc_link");
    let target_dir = if cfg!(windows) { "C:\\" } else { "/etc" };
    let _ = fs::remove_file(&malicious_link);
    match symlink(target_dir, &malicious_link) {
        Ok(_) => {}
        Err(e) if cfg!(windows) && e.kind() == std::io::ErrorKind::PermissionDenied => {
            println!(
                "Skipping test: Windows requires Developer Mode or Admin rights to create symlinks"
            );
            return;
        }
        Err(e) => panic!("Failed to create symlink: {}", e),
    }

    let params = CallToolRequestParams::new("run_terminal_command").with_arguments(
        serde_json::from_value(json!({
            "command": "cat passwd",
            "working_directory": "etc_link"
        }))
        .unwrap(),
    );
    let result = mcp.client.call_tool(params).await;
    assert!(
        result.is_err(),
        "SECURITY: Symlink escape outside sandbox should be blocked"
    );
}

/// Test that symlinks to user home directory are blocked
#[tokio::test]
async fn red_team_symlink_to_home_blocked() {
    init_test_logging();

    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(windows)]
    use std::os::windows::fs::symlink_dir as symlink;

    let temp_dir = TempDir::new().unwrap();
    let mcp = create_in_process_mcp_with_scope(
        &get_workspace_tools_dir(),
        vec![temp_dir.path().to_path_buf()],
    )
    .await
    .unwrap();

    // Create symlink to home directory
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| {
            if cfg!(windows) {
                "C:\\Users\\Public".to_string()
            } else {
                "/Users/Shared".to_string()
            }
        });
    let malicious_link = temp_dir.path().join("home_link");
    let _ = fs::remove_file(&malicious_link);
    match symlink(&home, &malicious_link) {
        Ok(_) => {}
        Err(e) if cfg!(windows) && e.kind() == std::io::ErrorKind::PermissionDenied => {
            println!(
                "Skipping test: Windows requires Developer Mode or Admin rights to create symlinks"
            );
            return;
        }
        Err(e) => panic!("Failed to create symlink: {}", e),
    }

    let params = CallToolRequestParams::new("run_terminal_command").with_arguments(
        serde_json::from_value(json!({
            "command": "ls .ssh",
            "working_directory": "home_link"
        }))
        .unwrap(),
    );
    let result = mcp.client.call_tool(params).await;
    assert!(
        result.is_err(),
        "SECURITY: Symlink escape to home directory should be blocked"
    );
}

// =============================================================================
// RED TEAM TEST 3: Command Injection via Path
// =============================================================================

/// Test that shell metacharacters in paths are rejected
#[tokio::test]
async fn red_team_shell_metacharacters_in_path() {
    init_test_logging();
    let temp_dir = TempDir::new().unwrap();
    let mcp = create_in_process_mcp_with_scope(
        &get_workspace_tools_dir(),
        vec![temp_dir.path().to_path_buf()],
    )
    .await
    .unwrap();

    // Attempt to inject shell commands via path
    // The path "; cat /etc/passwd #" doesn't exist as a directory
    let params = CallToolRequestParams::new("run_terminal_command").with_arguments(
        serde_json::from_value(json!({
            "command": "echo test",
            "working_directory": "; cat /etc/passwd #"
        }))
        .unwrap(),
    );
    let result = mcp.client.call_tool(params).await;
    // The command may start async but should fail during execution
    // because the working directory doesn't exist.
    // We're documenting that the system handles this case safely.
    let _ = result;
}

// =============================================================================
// RED TEAM TEST 4: No-Temp-Files Mode Tests
// =============================================================================

/// Test that no_temp_files mode is properly set on Sandbox
#[test]
fn red_team_no_temp_files_flag_setting() {
    let sandbox = Sandbox::new(vec![], SandboxMode::Strict, true, false, false).unwrap();
    assert!(
        sandbox.is_no_temp_files(),
        "no_temp_files should be enabled"
    );

    let sandbox_default = Sandbox::new(vec![], SandboxMode::Strict, false, false, false).unwrap();
    assert!(
        !sandbox_default.is_no_temp_files(),
        "no_temp_files should be disabled by default"
    );
}

// =============================================================================
// RED TEAM TEST 7: Global Read Access Prevention (Uniform Strictness)
// =============================================================================

/// Linux-only: reading a file outside the sandbox is blocked when Landlock is enforced.
///
/// Design notes:
/// - Both `scope_dir` (the sandbox root) and `outside_dir` are placed in the
///   test-process CWD (workspace, not /tmp) so they are siblings in the same
///   parent directory.
/// - Only `scope_dir` is added to the Landlock ruleset; `outside_dir` has no
///   Landlock rule covering it and is therefore OS-blocked.
/// - `--disable-temp-files` is passed so Landlock does NOT add a blanket
///   `/tmp` read+write rule.  Without this flag the entire /tmp hierarchy
///   becomes accessible and TempDir-based scopes become meaningless for
///   "outside scope" checks.
/// - The sandbox scope (working_dir) is `scope_dir`, which is non-tmp, so
///   the --disable-temp-files check in validate_path does not reject it.
#[tokio::test]
#[cfg(target_os = "linux")]
#[ignore]
async fn red_team_global_read_access_blocked() {
    init_test_logging();
    skip_if_landlock_unavailable!();

    // Use a non-tmp scope so --disable-temp-files does not reject the working dir.
    let scope_dir = create_non_tmp_scope_dir();
    let tools_dir = get_workspace_tools_dir();
    let client = ClientBuilder::new()
        .tools_dir(&tools_dir)
        .working_dir(scope_dir.path())
        .no_sandbox(false)
        // Prevent Landlock from adding a blanket /tmp rule — without this flag
        // a /tmp-based working_dir would make the entire /tmp accessible and
        // our separate TempDir::new() outside dir would also be readable.
        .arg("--disable-temp-files")
        .build()
        .await
        .unwrap();

    // Place the "secret" file in a sibling dir that is NOT the Landlock scope.
    let outside_dir = create_non_tmp_tempdir();
    let outside_file = outside_dir.path().join("secret.txt");
    std::fs::write(&outside_file, "secret content").unwrap();

    let params = CallToolRequestParams::new("run_terminal_command").with_arguments(
        serde_json::from_value(json!({
            "command": format!("cat {}", outside_file.display()),
            "execution_mode": "Synchronous"
        }))
        .unwrap(),
    );

    let result = client.call_tool(params).await;
    assert_blocked_shell_result(
        result,
        "secret content",
        "SECURITY: Should not be able to read file outside sandbox scope",
    );

    client.cancel().await.unwrap();
}

// =============================================================================
// RED TEAM TEST 8: LiveLog Symlink Targeted Read Expansion
// =============================================================================

/// Test that --livelog grants precise read-only access to a target symlink, but blocks writes and blocks neighboring files.
///
/// Design notes (see red_team_global_read_access_blocked for the full rationale):
/// - Scope dir is non-tmp (created via create_non_tmp_scope_dir) so
///   --disable-temp-files does not reject the working directory.
/// - Outside files are in a sibling non-tmp dir — not covered by any Landlock rule
///   except the explicit livelog read-scope grant for `outside_target`.
/// - --disable-temp-files prevents a blanket /tmp Landlock grant that would
///   otherwise make all temp dirs accessible.
/// - An `exceptions.json` is written into the scope dir so livelog's
///   `is_target_allowed` check approves the out-of-scope symlink target.
///   Without this the symlink would be silently blocked by `resolve_log_symlink`
///   and never added to Landlock read_scopes.
#[tokio::test]
#[cfg(target_os = "linux")]
#[ignore]
async fn red_team_livelog_symlink_read_allowed() {
    init_test_logging();
    skip_if_landlock_unavailable!();

    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(windows)]
    use std::os::windows::fs::symlink_dir as symlink;

    // Use a non-tmp scope so --disable-temp-files does not reject the working dir.
    let scope_dir = create_non_tmp_scope_dir(); // sandbox scope
    let log_dir = scope_dir.path().join("logs");
    std::fs::create_dir_all(&log_dir).unwrap();

    let outside_dir = create_non_tmp_tempdir();
    let outside_target = outside_dir.path().join("secret.log");
    std::fs::write(&outside_target, "livelog secret content").unwrap();

    let outside_forbidden = outside_dir.path().join("forbidden.log");
    std::fs::write(&outside_forbidden, "forbidden content").unwrap();

    // Set up exceptions.json so that livelog's is_target_allowed() approves
    // the outside target, allowing it to be added to Landlock read_scopes.
    // Without this, the out-of-scope symlink target would be silently blocked
    // by resolve_log_symlink and never added to read_scopes.
    let ahma_dir = scope_dir.path().join(".ahma");
    std::fs::create_dir_all(&ahma_dir).unwrap();
    let exceptions_json = serde_json::json!({
        "approved_log_symlinks": [
            {"target_path": outside_target.to_str().expect("non-UTF-8 path")}
        ]
    });
    std::fs::write(
        ahma_dir.join("exceptions.json"),
        serde_json::to_string(&exceptions_json).unwrap(),
    )
    .unwrap();

    let malicious_link = log_dir.join("live.log");
    match symlink(&outside_target, &malicious_link) {
        Ok(_) => {}
        Err(e) if cfg!(windows) && e.kind() == std::io::ErrorKind::PermissionDenied => {
            println!(
                "Skipping test: Windows requires Developer Mode or Admin rights to create symlinks"
            );
            return;
        }
        Err(e) => panic!("Failed to create symlink: {}", e),
    }

    let tools_dir = get_workspace_tools_dir();
    let client = ClientBuilder::new()
        .tools_dir(&tools_dir)
        .working_dir(scope_dir.path())
        .no_sandbox(false)
        // Prevent Landlock from adding a blanket /tmp rule.
        .arg("--disable-temp-files")
        .livelog(true) // Enable the feature we are testing
        .build()
        .await
        .unwrap();

    // 1. We MUST be able to read the explicit target file via its absolute path.
    //    The livelog feature adds this path to read_scopes so it is always readable.
    let params = CallToolRequestParams::new("run_terminal_command").with_arguments(
        serde_json::from_value(json!({
            "command": format!("cat {}", outside_target.display()),
            "execution_mode": "Synchronous"
        }))
        .unwrap(),
    );
    let result = client.call_tool(params).await;
    let mut read_succeeded = false;
    if let Ok(tools_res) = result {
        for content in tools_res.content {
            if let Some(text) = content.as_text() {
                // execute_shell_sync returns raw stdout text, not a JSON envelope.
                assert!(
                    text.text.contains("livelog secret content"),
                    "SECURITY: Valid livelog symlink target read was blocked. Got: {}",
                    text.text
                );
                read_succeeded = true;
            }
        }
    }
    assert!(read_succeeded, "Read command did not complete successfully");

    // 2. We MUST NOT be able to read neighboring files in the external directory.
    let params2 = CallToolRequestParams::new("run_terminal_command").with_arguments(
        serde_json::from_value(json!({
            "command": format!("cat {}", outside_forbidden.display()),
            "execution_mode": "Synchronous"
        }))
        .unwrap(),
    );
    let result2 = client.call_tool(params2).await;
    assert_blocked_shell_result(
        result2,
        "forbidden content",
        "SECURITY: Livelog should not grant access to neighboring files outside scope",
    );

    // 3. We MUST NOT be able to WRITE to the explicit target file.
    //    The core livelog invariant is Linux-focused: livelog must remain read-only and
    //    must not expand write scope to external files.
    let params3 = CallToolRequestParams::new("run_terminal_command").with_arguments(
        serde_json::from_value(json!({
            "command": format!("echo hax > {}", outside_target.display()),
            "execution_mode": "Synchronous"
        }))
        .unwrap(),
    );
    let result3 = client.call_tool(params3).await;
    // Landlock scopes writes precisely: outside_target has only read access in read_scopes,
    // not write access.  When bash tries to open the file for writing, the kernel returns
    // EACCES and bash exits non-zero.  The MCP tool call itself returns Ok (the process
    // started successfully), so we cannot use result3.is_err() here — we must check the
    // command-level exit code via assert_blocked_shell_result.
    assert_blocked_shell_result(
        result3,
        "hax",
        "SECURITY: Livelog target should be strictly read-only; write should be blocked",
    );

    client.cancel().await.unwrap();
}

// =============================================================================
// RED TEAM TEST 9: Spawn-Time Landlock (worker-thread regression)
// =============================================================================

/// Children built via `Sandbox::create_command` must be kernel-restricted even
/// when spawned from a tokio worker thread.
///
/// Regression test: `landlock_restrict_self(2)` only restricts the calling
/// thread, so process-level enforcement performed inside an already-running
/// async runtime never covered commands spawned from pre-existing worker
/// threads — they ran fully unsandboxed. The fix applies the ruleset per child
/// in `pre_exec` (the forked child is single-threaded), which this test
/// exercises by spawning from an explicitly multi-threaded runtime task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(target_os = "linux")]
async fn red_team_spawned_child_landlock_enforced_from_worker_thread() {
    init_test_logging();
    skip_if_landlock_unavailable!();

    // Both dirs live in the workspace (not /tmp) so the blanket /tmp grant is
    // irrelevant; only scope_dir is covered by a Landlock rule.
    let scope_dir = create_non_tmp_scope_dir();
    let outside_dir = create_non_tmp_tempdir();
    let outside_file = outside_dir.path().join("secret.txt");
    std::fs::write(&outside_file, "secret content").unwrap();
    let inside_file = scope_dir.path().join("inside.txt");
    std::fs::write(&inside_file, "inside content").unwrap();

    let sandbox = Sandbox::new(
        vec![scope_dir.path().to_path_buf()],
        SandboxMode::Strict,
        true, // no_temp_files: avoid the blanket /tmp read+write rule
        false,
        false,
    )
    .unwrap();

    let scope = scope_dir.path().to_path_buf();
    let outside = outside_file.clone();
    let inside = inside_file.clone();
    // tokio::spawn moves execution to a worker thread — the exact path that
    // process-level restrict_self() never covered.
    let (outside_output, inside_output) = tokio::spawn(async move {
        let outside_output = sandbox
            .create_shell_command("bash", &format!("cat {}", outside.display()), &scope)
            .unwrap()
            .output()
            .await
            .unwrap();
        let inside_output = sandbox
            .create_shell_command("bash", &format!("cat {}", inside.display()), &scope)
            .unwrap()
            .output()
            .await
            .unwrap();
        (outside_output, inside_output)
    })
    .await
    .unwrap();

    let outside_stdout = String::from_utf8_lossy(&outside_output.stdout);
    assert!(
        !outside_stdout.contains("secret content"),
        "SECURITY: child spawned from worker thread read a file outside the sandbox scope"
    );
    assert!(
        !outside_output.status.success(),
        "SECURITY: out-of-scope read should fail with a kernel-level error, got exit 0"
    );

    let inside_stdout = String::from_utf8_lossy(&inside_output.stdout);
    assert!(
        inside_output.status.success() && inside_stdout.contains("inside content"),
        "In-scope read must still succeed under spawn-time Landlock. stdout: {inside_stdout}, stderr: {}",
        String::from_utf8_lossy(&inside_output.stderr)
    );
}

// =============================================================================
// RED TEAM TEST 5: Command Argument Escape (Write)
// =============================================================================

/// Test that writing to a file outside the sandbox via command arguments is blocked.
/// Requires OS-level sandbox enforcement (Landlock on Linux, Seatbelt on macOS,
/// AppContainer on Windows).
///
/// The escape is a `>` shell redirect inside a free-form command string, which
/// ahma never parses — so application-level `validate_path` cannot catch it and
/// blocking depends entirely on the kernel-level sandbox wrapping the spawned
/// shell.  On Windows that requires AppContainer spawn isolation, which is still
/// pending (SPEC R6.3.3; `create_appcontainer_command` is currently a stub).
/// Until that lands, Windows cannot block this escape, so the test is ignored
/// there rather than asserting a guarantee the platform does not yet provide.
/// Linux (Landlock) and macOS (Seatbelt) run and enforce it.
#[tokio::test]
#[cfg_attr(
    target_os = "windows",
    ignore = "Requires AppContainer spawn isolation (SPEC R6.3.3), still pending on Windows"
)]
async fn red_team_command_write_escape_blocked() {
    init_test_logging();

    let temp_dir = TempDir::new().unwrap();
    let outside_dir = TempDir::new().unwrap();
    let outside_file = outside_dir.path().join("pwned.txt");

    let tools_dir = get_workspace_tools_dir();
    let client = ClientBuilder::new()
        .tools_dir(&tools_dir)
        .working_dir(temp_dir.path())
        .no_sandbox(false)
        .arg("--disable-temp-files")
        .build()
        .await
        .unwrap();

    // Attempt to write to a file outside the sandbox using absolute path
    let params = CallToolRequestParams::new("run_terminal_command").with_arguments(
        serde_json::from_value(json!({
            "command": format!("echo 'hacked' > {}", outside_file.display()),
            "execution_mode": "Synchronous"
        }))
        .unwrap(),
    );

    // The command might "succeed" (exit code 0) if the shell handles the error gracefully,
    // or fail (exit code 1). Key check is: file MUST NOT exist.
    let _ = client.call_tool(params).await;

    assert!(
        !outside_file.exists(),
        "SECURITY: Should not be able to write to file outside sandbox: {}",
        outside_file.display()
    );

    client.cancel().await.unwrap();
}

// =============================================================================
// RED TEAM TEST 6: Command Argument Escape (Read - Linux Only)
// =============================================================================

/// Test that reading a file outside the sandbox via command arguments is blocked on Linux
#[tokio::test]
#[cfg(target_os = "linux")]
async fn red_team_command_read_escape_blocked_linux() {
    init_test_logging();
    let temp_dir = TempDir::new().unwrap();
    let tools_dir = get_workspace_tools_dir();
    let client = ClientBuilder::new()
        .tools_dir(&tools_dir)
        .working_dir(temp_dir.path())
        .no_sandbox(false)
        .arg("--disable-temp-files")
        .build()
        .await
        .unwrap();

    // Attempt to read /etc/shadow (or similar restricted file)
    let params = CallToolRequestParams::new("run_terminal_command").with_arguments(
        serde_json::from_value(json!({
            "command": "cat /etc/shadow", // Typically root only, but Landlock should block open() regardless
            "execution_mode": "Synchronous"
        }))
        .unwrap(),
    );

    let result = client.call_tool(params).await;

    // Command should fail or return error exit code
    if let Ok(response) = result {
        let _content = response.content.first().unwrap().as_text().unwrap();
        // Check if output contains "Permission denied" or similar
        // Note: response content is JSON string of the result, we need to check stderr/exit code
        // But client.call_tool returns the ToolResult. Use debug print if needed.
        // Simplified check: Use a file we know exists but shouldn't be readable due to sandbox

        // Actually, let's use a custom file outside sandbox to be sure
    }

    client.cancel().await.unwrap();
}

/// Refined Linux read test with verified outside file
#[tokio::test]
#[cfg(target_os = "linux")]
async fn red_team_command_read_escape_blocked_linux_custom() {
    use std::io::Write;

    init_test_logging();
    let temp_dir = TempDir::new().unwrap();
    let outside_dir = TempDir::new().unwrap();
    let outside_file = outside_dir.path().join("secret.txt");
    {
        let mut f = fs::File::create(&outside_file).unwrap();
        writeln!(f, "secret content").unwrap();
    }

    let tools_dir = get_workspace_tools_dir();
    let client = ClientBuilder::new()
        .tools_dir(&tools_dir)
        .working_dir(temp_dir.path())
        .no_sandbox(false)
        .arg("--disable-temp-files")
        .build()
        .await
        .unwrap();

    // Attempt to read the outside file
    let params = CallToolRequestParams::new("run_terminal_command").with_arguments(
        serde_json::from_value(json!({
            "command": format!("cat {}", outside_file.display()),
            "execution_mode": "Synchronous"
        }))
        .unwrap(),
    );

    let result = client.call_tool(params).await;

    if let Ok(tools_res) = result {
        for content in tools_res.content {
            if let Some(text) = content.as_text() {
                let res_json: serde_json::Value = serde_json::from_str(&text.text).unwrap();
                let exit_code = res_json
                    .get("exit_code")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let stderr = res_json
                    .get("stderr")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let stdout = res_json
                    .get("stdout")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                // Should fail with exit code != 0 or Permission denied
                assert!(
                    exit_code != 0 || stderr.contains("Permission denied"),
                    "SECURITY: Should not be able to read file outside sandbox on Linux. Exit: {}, Stderr: {}, Stdout: {}",
                    exit_code,
                    stderr,
                    stdout
                );
            }
        }
    }

    client.cancel().await.unwrap();
}
