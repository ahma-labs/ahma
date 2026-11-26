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
use rmcp::model::CallToolResult;
use serde_json::json;
use std::fs;
#[cfg(target_os = "linux")]
use std::sync::OnceLock;
use tempfile::TempDir;

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

/// True when the platform's OS-level sandbox can actually be enforced for a
/// spawned server.
///
/// In a nested sandbox (Docker, Cursor, an outer `sandbox-exec`, CI running
/// inside a sandbox) `sandbox_apply` is denied, so the test harness runs the
/// server WITHOUT OS enforcement (see `ClientBuilder::run_command`, which sets
/// `force_no_sandbox` when `is_nested_sandbox_environment()`). Tests that depend
/// on *kernel-level* blocking (a `>` shell redirect that application-level
/// `validate_path` cannot see) therefore cannot hold in that environment. They
/// skip here rather than report a false failure — the assertion still runs and
/// must pass on real CI hosts where the OS sandbox engages.
fn os_sandbox_enforced() -> bool {
    ahma_mcp::sandbox::check_sandbox_prerequisites().is_ok()
        && ahma_mcp::sandbox::test_sandbox_exec_available().is_ok()
}

macro_rules! skip_if_os_sandbox_unenforced {
    () => {
        if !os_sandbox_enforced() {
            eprintln!(
                "Skipping test: OS-level sandbox cannot be enforced in this environment \
                 (nested sandbox); kernel enforcement is required for this assertion."
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
            "working_directory": "; cat /etc/passwd #",
            "execution_mode": "Synchronous"
        }))
        .unwrap(),
    );
    let result = mcp.client.call_tool(params).await;
    // The metacharacters must be treated as a literal (nonexistent) directory
    // name, never shell-interpreted. Whether the call errors (invalid working
    // dir) or runs with the path taken literally, /etc/passwd must NOT be read.
    let leaked = match &result {
        Ok(r) => {
            let text = result_text(r);
            text.contains("root:") || text.contains("/bin/bash\n") || text.contains(":0:0:")
        }
        Err(_) => false,
    };
    assert!(
        !leaked,
        "shell metacharacters in working_directory must not execute a command injection: {result:?}"
    );
}

/// End-to-end: a tool subprocess must NOT inherit secret-bearing environment
/// variables from the server, so a prompt-injected `env`/`printenv` cannot leak
/// the server's credentials. Complements the `base_command` unit test by
/// asserting a *real spawned process* does not see the secret, while a
/// non-secret var is still inherited (the scrub is selective).
#[cfg(unix)]
#[tokio::test]
async fn red_team_secret_env_scrubbed_from_subprocess() {
    init_test_logging();
    // Set the secret BEFORE spawning the server so the server inherits it (and
    // must then scrub it from the tool child). SAFETY: nextest runs each test in
    // its own process; these names are unique.
    unsafe {
        std::env::set_var("AHMA_TEST_FAKE_API_KEY", "supersecretvalue123");
        std::env::set_var("AHMA_TEST_KEEPME", "keepvalue456");
    }

    let temp_dir = TempDir::new().unwrap();
    let tools_dir = get_workspace_tools_dir();
    // ClientBuilder spawns a real server subprocess; env scrubbing happens in
    // `base_command` regardless of whether the OS sandbox engages, so this runs
    // in a nested-sandbox environment too (the harness auto-disables the OS
    // sandbox there). `env` dumps the child's environment.
    let client = ClientBuilder::new()
        .tools_dir(&tools_dir)
        .working_dir(temp_dir.path())
        .no_sandbox(false)
        .build()
        .await
        .unwrap();

    let params = CallToolRequestParams::new("run_terminal_command").with_arguments(
        serde_json::from_value(json!({
            "command": "env",
            "execution_mode": "Synchronous"
        }))
        .unwrap(),
    );
    let result = client.call_tool(params).await;
    let text = result.as_ref().map(result_text).unwrap_or_default();
    client.cancel().await.unwrap();

    unsafe {
        std::env::remove_var("AHMA_TEST_FAKE_API_KEY");
        std::env::remove_var("AHMA_TEST_KEEPME");
    }

    assert!(
        text.contains("keepvalue456"),
        "sanity: `env` ran and inherited the non-secret var; got: {text}"
    );
    assert!(
        !text.contains("supersecretvalue123"),
        "SECURITY: a secret-named env var must be scrubbed from the tool subprocess; got: {text}"
    );
}

// =============================================================================
// RED TEAM TEST 4: No-Temp-Files Mode Tests
// =============================================================================

/// Test that no_temp_files mode is properly set on Sandbox
#[test]
fn red_team_no_temp_files_flag_setting() {
    let sandbox = Sandbox::new(
        vec![],
        SandboxMode::Strict,
        ahma_mcp::sandbox::SandboxOptions {
            no_temp_files: true,
            ..ahma_mcp::sandbox::SandboxOptions::default()
        },
    )
    .unwrap();
    assert!(
        sandbox.is_no_temp_files(),
        "no_temp_files should be enabled"
    );

    let sandbox_default = Sandbox::new(vec![], SandboxMode::Strict, Default::default()).unwrap();
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
#[ignore = "creates its fixtures in the current directory (inside the repository), since a /tmp fixture would be covered by the temp rule; run by build.yml's dedicated Landlock red-team step"]
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
/// - The out-of-scope symlink target is approved as a `log-target` grant in the
///   permission ledger (`~/.ahma/settings.toml` under a temp `AHMA_TEST_HOME`,
///   inherited by the spawned server) so livelog's `is_target_allowed` check
///   admits it. Without this the symlink would be silently blocked by
///   `resolve_log_symlink` and never added to Landlock read_scopes.
#[tokio::test]
#[cfg(target_os = "linux")]
#[ignore = "creates its fixtures in the current directory (inside the repository), since a /tmp fixture would be covered by the temp rule; run by build.yml's dedicated Landlock red-team step"]
async fn red_team_livelog_symlink_read_allowed() {
    init_test_logging();
    skip_if_landlock_unavailable!();

    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(windows)]
    use std::os::windows::fs::symlink_dir as symlink;

    // Use a non-tmp scope so --disable-temp-files does not reject the working dir.
    let scope_dir = create_non_tmp_scope_dir(); // sandbox scope
    let log_dir = scope_dir.path().join(".ahma").join("logs");
    std::fs::create_dir_all(&log_dir).unwrap();

    let outside_dir = create_non_tmp_tempdir();
    let outside_target = outside_dir.path().join("secret.log");
    std::fs::write(&outside_target, "livelog secret content").unwrap();

    let outside_forbidden = outside_dir.path().join("forbidden.log");
    std::fs::write(&outside_forbidden, "forbidden content").unwrap();

    // Approve the outside target so livelog's is_target_allowed() admits it to
    // the Landlock read_scopes. Approvals live in the permission ledger, outside
    // every sandbox scope; redirect the home to a temp location for isolation
    // (nextest = process-per-test). The spawned server inherits AHMA_TEST_HOME,
    // so it reads the same ledger. Without this, the out-of-scope symlink target
    // would be silently blocked by resolve_log_symlink and never added to
    // read_scopes.
    let home_dir = create_non_tmp_tempdir();
    unsafe { std::env::set_var("AHMA_TEST_HOME", home_dir.path()) };
    let outside_target = dunce::canonicalize(&outside_target).unwrap();
    ahma_mcp::sandbox::add_log_exception(scope_dir.path(), &outside_target).unwrap();

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
                // The inline operation result: identity line, then stdout.
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
    // started successfully), so we cannot use result3.is_err() here — we check the
    // command-level failure, and that the file on disk is untouched. (The result's
    // identity line echoes the command, `echo hax > …`, so "hax" in the text proves
    // nothing either way; the file's content is the real evidence.)
    if let Ok(result) = &result3 {
        assert!(
            blocked_shell_result_indicates_failure(result),
            "SECURITY: Livelog target should be strictly read-only; the write must fail. Output: {}",
            result_text(result)
        );
    }
    assert_eq!(
        std::fs::read_to_string(&outside_target).unwrap(),
        "livelog secret content",
        "SECURITY: Livelog target should be strictly read-only; the write must not land"
    );

    client.cancel().await.unwrap();
}

// =============================================================================
// RED TEAM TEST 9: Spawn-Time Landlock (worker-thread regression)
// =============================================================================

/// SPEC R-HUB.13.3: on Linux the hub's token is the boundary, because Landlock
/// at ahma's ABI cannot refuse a `connect(2)` to the hub's socket. So what a
/// command in ahma's sandbox must not be able to do is *read* what holds the
/// token: the runtime directory is in no ruleset, and the process that holds a
/// copy in memory (a worker, a hook) cannot be read through `/proc` — Landlock
/// refuses a restricted process `ptrace_may_access` to one outside its domain.
/// Each is checked against an unrestricted control, so a refusal means the
/// sandbox, not a typo in the path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(target_os = "linux")]
async fn red_team_a_sandboxed_command_cannot_read_the_hubs_token_or_its_holder() {
    init_test_logging();
    skip_if_landlock_unavailable!();

    let scope_dir = create_non_tmp_scope_dir();
    // The token as the hub keeps it: beside its socket, in the runtime directory.
    let runtime = ahma_common::hub::runtime_dir().expect("a runtime directory");
    let token = ahma_common::hub::HubToken::generate();
    let token_file = runtime.join("redteam-hub.token");
    ahma_common::hub::publish_token(&token_file, &token)
        .await
        .unwrap();
    // A secret a process holds in memory, as a worker holds the token it
    // preloaded: a separate process outside the sandbox, started with it in its
    // environment. (`/proc/<pid>/environ` shows the environment as of `exec`, so
    // a `set_var` in this process would never appear there.)
    const MARKER: &str = "AHMA_REDTEAM_HOLDER_SECRET=hunter2-held-in-memory";
    let mut holder = tokio::process::Command::new("sleep")
        .arg("600")
        .env("AHMA_REDTEAM_HOLDER_SECRET", "hunter2-held-in-memory")
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .expect("start the holder process");
    let environ = format!("/proc/{}/environ", holder.id().expect("holder pid"));

    // Controls: this unrestricted process reads both.
    assert_eq!(
        std::fs::read_to_string(&token_file).unwrap().trim(),
        token.as_str()
    );
    assert!(
        std::fs::read(&environ)
            .unwrap()
            .windows(MARKER.len())
            .any(|w| w == MARKER.as_bytes()),
        "the control read of {environ} must see the marker"
    );

    let sandbox = Sandbox::new(
        vec![scope_dir.path().to_path_buf()],
        SandboxMode::Strict,
        ahma_mcp::sandbox::SandboxOptions {
            no_temp_files: true,
            ..ahma_mcp::sandbox::SandboxOptions::default()
        },
    )
    .unwrap();
    let scope = scope_dir.path().to_path_buf();
    for (what, path) in [
        ("the hub's token file", token_file.display().to_string()),
        ("the holder's /proc environment", environ),
    ] {
        let out = sandbox
            .create_shell_command("bash", &format!("cat {path}"), &scope)
            .unwrap()
            .output()
            .await
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            !stdout.contains(token.as_str()) && !stdout.contains("hunter2-held-in-memory"),
            "SECURITY: a sandboxed command read {what}: {stdout}"
        );
        assert!(
            !out.status.success(),
            "SECURITY: reading {what} must fail under Landlock, got exit 0"
        );
    }
    let _ = std::fs::remove_file(token_file);
    ahma_mcp::shell_pool::kill_process_tree(&mut holder).await;
}

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
        ahma_mcp::sandbox::SandboxOptions {
            no_temp_files: true,
            ..ahma_mcp::sandbox::SandboxOptions::default()
        },
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
/// shell. Linux (Landlock) and macOS (Seatbelt) run and enforce it.
///
/// **Windows status: implemented, executed, not yet shown to work.** AppContainer
/// spawn isolation exists (`sandbox/windows.rs`: per-session container SID, scope
/// DACL grants, `STARTUPINFOEX` + `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`
/// spawn through the `ahma.exe` launcher), and no `windows-latest` run has shown
/// the in-scope write succeeding. Why is not known: the shell used to test it,
/// Windows PowerShell 5.1, could not load its own modules inside the container,
/// so the earlier "an in-scope write is denied" was never established. So the
/// path is switched off
/// (`windows::appcontainer_spawn_enabled` is `false`) and Windows currently has
/// **no** OS-enforced path boundary at all, which is why this escape is not
/// blocked there.
///
/// An earlier version of this comment said the code had "never been executed"
/// and that "only the second half is outstanding". Both were true when written
/// and stopped being true when CI ran it; the correction matters because the old
/// wording implies the mechanism probably works and is merely unverified.
///
/// The `ignore` comes off when a `windows-latest` run shows the boundary holding
/// in both directions — see `appcontainer_dacl_diagnostics`, which measures it
/// without depending on a shell.
#[tokio::test]
#[cfg_attr(
    target_os = "windows",
    ignore = "AppContainer spawn isolation (SPEC R6.3.3) is implemented but not yet shown to \
              work on windows-latest CI and is switched off, so Windows has no path boundary \
              to enforce this; remove once a CI run shows the boundary holding both ways"
)]
async fn red_team_command_write_escape_blocked() {
    init_test_logging();
    // The escape is a `>` shell redirect that only the kernel sandbox can block;
    // skip when OS enforcement is unavailable (nested sandbox), since the harness
    // then runs the server unsandboxed and the write cannot be blocked.
    skip_if_os_sandbox_unenforced!();

    // Scope and target outside temp: under `--disable-temp-files` a scope in
    // temp is refused before the command runs, which would pass this test for
    // the wrong reason. The control write inside the scope proves it ran.
    let (scope_dir, outside_dir) = scope_and_outside_dirs("write");
    let control_file = scope_dir.path().join("control.txt");
    let outside_file = outside_dir.path().join("pwned.txt");

    let tools_dir = get_workspace_tools_dir();
    let client = ClientBuilder::new()
        .tools_dir(&tools_dir)
        .working_dir(scope_dir.path())
        .no_sandbox(false)
        .arg("--disable-temp-files")
        .build()
        .await
        .unwrap();

    // Attempt to write to a file outside the sandbox using absolute path
    let params = CallToolRequestParams::new("run_terminal_command").with_arguments(
        serde_json::from_value(json!({
            "command": format!(
                "echo ran > '{}'; echo hacked > '{}'",
                control_file.display(),
                outside_file.display()
            ),
            "execution_mode": "Synchronous"
        }))
        .unwrap(),
    );

    // The command might "succeed" (exit code 0) if the shell handles the error gracefully,
    // or fail (exit code 1). Key check is: file MUST NOT exist.
    let result = client.call_tool(params).await;

    assert!(
        control_file.exists(),
        "the control failed: the command did not write inside its scope, so it never ran \
         and this test proves nothing. Result: {result:?}"
    );
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

// NOTE: a former `red_team_command_read_escape_blocked_linux` that targeted
// `/etc/shadow` was removed: `/etc/shadow` is unreadable to non-root regardless
// of the sandbox (confounding the assertion), and it asserted nothing. The
// definitive read-escape coverage is `_custom` below, which uses a
// world-readable file placed outside the sandbox scope.

/// Refined Linux read test with verified outside file
#[tokio::test]
#[cfg(target_os = "linux")]
async fn red_team_command_read_escape_blocked_linux_custom() {
    use std::io::Write;

    init_test_logging();
    // Kernel-level read blocking is exactly what this test asserts; without
    // Landlock the spawned child runs with application-level checks only and
    // the out-of-scope read genuinely succeeds.
    skip_if_landlock_unavailable!();
    // Outside temp, with a control read inside the scope: see
    // `red_team_command_write_escape_blocked` for why.
    let (scope_dir, outside_dir) = scope_and_outside_dirs("read");
    let control_file = scope_dir.path().join("control.txt");
    fs::write(&control_file, "control-visible\n").unwrap();
    let outside_file = outside_dir.path().join("secret.txt");
    {
        let mut f = fs::File::create(&outside_file).unwrap();
        writeln!(f, "secret content").unwrap();
    }

    let tools_dir = get_workspace_tools_dir();
    let client = ClientBuilder::new()
        .tools_dir(&tools_dir)
        .working_dir(scope_dir.path())
        .no_sandbox(false)
        .arg("--disable-temp-files")
        .build()
        .await
        .unwrap();

    // Attempt to read the outside file
    let params = CallToolRequestParams::new("run_terminal_command").with_arguments(
        serde_json::from_value(json!({
            "command": format!(
                "cat '{}'; cat '{}'",
                control_file.display(),
                outside_file.display()
            ),
            "execution_mode": "Synchronous"
        }))
        .unwrap(),
    );

    let result = client.call_tool(params).await;

    // The control must have been read, or the command never ran; the secret
    // must not have been, and the command must report the refusal.
    let tools_res = result.expect("the command must run, or the control proves nothing");
    let text = result_text(&tools_res);
    assert!(
        text.contains("control-visible"),
        "the control failed: the command did not read inside its scope. Result: {text}"
    );
    assert!(
        !text.contains("secret content"),
        "SECURITY: a sandboxed command read a file outside its scope on Linux. Result: {text}"
    );
    {
        assert!(
            blocked_shell_result_indicates_failure(&tools_res),
            "SECURITY: Should not be able to read file outside sandbox on Linux. Result: {}",
            result_text(&tools_res)
        );
    }

    client.cancel().await.unwrap();
}

// =============================================================================
// RED TEAM: truncate(2) outside the scope (SPEC R-HARDEN.1)
// =============================================================================

/// `truncate(2)` is a write that never opens the file. Landlock gates it only
/// from ABI 3 (Linux 6.2), as its own access right, so a ruleset that handles
/// ABI 2 leaves it open: a sandboxed command could zero any file the user can
/// write, outside the scope. Seatbelt counts it as `file-write-data`. The
/// command uses perl's `truncate`, a direct truncate(2) by path, because
/// macOS has no `truncate` binary and a shell redirect opens the file (which
/// ABI 1 already denies), so it would not test this at all.
#[tokio::test]
#[cfg_attr(
    target_os = "windows",
    ignore = "AppContainer spawn isolation (SPEC R6.3.3) is switched off, so Windows has no \
              path boundary to enforce this"
)]
async fn red_team_truncate_outside_scope_blocked() {
    init_test_logging();
    skip_if_os_sandbox_unenforced!();
    #[cfg(target_os = "linux")]
    if !landlock_abi_at_least(3) {
        eprintln!(
            "Skipping: this kernel's Landlock predates ABI 3 (Linux 6.2), which is the first \
             that can deny truncate(2); ahma discloses the gap there (SPEC R-HARDEN.1)"
        );
        return;
    }

    // Both directories outside temp, so `--disable-temp-files` cannot refuse
    // the scope itself before the command runs.
    let (scope_dir, outside_dir) = scope_and_outside_dirs("truncate");
    // The control: a file in the scope the same command must truncate, which
    // proves the command ran under the sandbox rather than not at all.
    let inside_file = scope_dir.path().join("mine.txt");
    let outside_file = outside_dir.path().join("precious.txt");
    fs::write(&inside_file, "mine to truncate\n").unwrap();
    fs::write(&outside_file, "do not lose me\n").unwrap();

    let tools_dir = get_workspace_tools_dir();
    let client = ClientBuilder::new()
        .tools_dir(&tools_dir)
        .working_dir(scope_dir.path())
        .no_sandbox(false)
        .arg("--disable-temp-files")
        .build()
        .await
        .unwrap();

    let params = CallToolRequestParams::new("run_terminal_command").with_arguments(
        serde_json::from_value(json!({
            "command": format!(
                "perl -e 'truncate($ARGV[0], 0) or die \"inside: $!\\n\"; truncate($ARGV[1], 0) or die \"outside: $!\\n\"' '{}' '{}'",
                inside_file.display(),
                outside_file.display()
            ),
            "execution_mode": "Synchronous"
        }))
        .unwrap(),
    );
    let result = client.call_tool(params).await;

    assert_eq!(
        fs::metadata(&inside_file).unwrap().len(),
        0,
        "the control failed: the command did not truncate the file inside its scope, so it \
         never ran and this test proves nothing. Result: {result:?}"
    );
    assert_eq!(
        fs::read_to_string(&outside_file).unwrap(),
        "do not lose me\n",
        "SECURITY: a sandboxed command truncated a file outside its scope: {}",
        outside_file.display()
    );

    client.cancel().await.unwrap();
}

/// The Landlock ABI this kernel offers, from `landlock_create_ruleset` with
/// `LANDLOCK_CREATE_RULESET_VERSION`.
#[cfg(target_os = "linux")]
fn landlock_abi_at_least(min: i64) -> bool {
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    // SAFETY: a version query passes no attributes and creates nothing.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    abi >= min
}

/// A scope directory and a sibling outside it, both outside temp, for tests
/// that run with `--disable-temp-files` (which refuses a scope in temp before
/// any command runs). Created in the test's working directory, removed on drop.
fn scope_and_outside_dirs(what: &str) -> (TempDir, TempDir) {
    let base = std::env::current_dir().expect("current directory");
    let make = |kind: &str| {
        tempfile::Builder::new()
            .prefix(&format!("ahma-redteam-{what}-{kind}-"))
            .tempdir_in(&base)
            .unwrap()
    };
    (make("scope"), make("outside"))
}

// =============================================================================
// RED TEAM: kernel attack surface behind a seccomp baseline (SPEC R-HARDEN.3)
// =============================================================================

/// A kernel privilege escalation escapes any Landlock sandbox, so commands run
/// with the system calls a build or test never needs, and that 2026's kernel
/// exploits reached, refused: `AF_ALG` sockets (CVE-2026-31431 "Copy Fail"),
/// `io_uring`, `keyctl`. Each is refused with the error a program treats as
/// "not available" (`EAFNOSUPPORT`, `ENOSYS`), so the ones that probe for
/// them fall back instead of failing. A Unix socket, which builds use, still
/// opens: the control that proves the command ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(target_os = "linux")]
async fn red_team_seccomp_refuses_kernel_attack_surface() {
    init_test_logging();
    skip_if_landlock_unavailable!();
    let (scope_dir, _outside) = scope_and_outside_dirs("seccomp");
    let sandbox = Sandbox::new(
        vec![scope_dir.path().to_path_buf()],
        SandboxMode::Strict,
        ahma_mcp::sandbox::SandboxOptions::default(),
    )
    .unwrap();
    let keyctl = if std::env::consts::ARCH == "aarch64" {
        219
    } else {
        250
    };
    let probe = format!(
        r#"perl -e '
            socket(my $u, 1, 1, 0) ? print "unix: opened\n" : print "unix: refused $!\n";
            socket(my $a, 38, 5, 0) ? print "alg: opened\n" : print "alg: refused $!\n";
            my $params = "\0" x 120;
            my $r = syscall(425, 1, $params); print $r < 0 ? "uring: refused $!\n" : "uring: opened\n";
            my $k = syscall({keyctl}, 0, -3, 0); print $k < 0 ? "keyctl: refused $!\n" : "keyctl: allowed\n";
        '"#
    );
    let out = sandbox
        .create_shell_command("bash", &probe, scope_dir.path())
        .unwrap()
        .output()
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("unix: opened"),
        "the control must run: {text}"
    );
    // The exact errno is the filter's answer, so a refusal for some other
    // reason (a kernel built without io_uring, say) cannot pass for it.
    for refused in [
        "alg: refused Address family not supported",
        "uring: refused Function not implemented",
        "keyctl: refused Function not implemented",
    ] {
        assert!(
            text.contains(refused),
            "SECURITY: expected {refused}: {text}"
        );
    }
}

/// Landlock's network rules (ABI 4) arrived in Linux 6.7; before that a
/// restricted command's TCP connects are not kernel-filtered at all.
#[cfg(target_os = "linux")]
fn kernel_filters_tcp_connect() -> bool {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let mut parts = release
        .trim()
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|p| p.parse::<u32>().ok());
    matches!((parts.next(), parts.next()), (Some(major), Some(minor)) if (major, minor) >= (6, 7))
}

/// SPEC R-WEB.16.13 on Linux: under `--restrict-network` a command may
/// connect to the proxy's port and nothing else, so a local service (a test
/// database) is refused until `[network] local_ports` lists its port. The
/// listener is this test's own; with restriction off the command reaches it,
/// which proves the refusal is the kernel rule's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(target_os = "linux")]
async fn red_team_restricted_network_opens_a_local_port_only_when_listed() {
    init_test_logging();
    skip_if_landlock_unavailable!();
    if !kernel_filters_tcp_connect() {
        eprintln!("Skipping test: Landlock filters TCP connects from Linux 6.7");
        return;
    }
    let (scope_dir, _outside) = scope_and_outside_dirs("local-ports");
    let sandbox = Sandbox::new(
        vec![scope_dir.path().to_path_buf()],
        SandboxMode::Strict,
        ahma_mcp::sandbox::SandboxOptions::default(),
    )
    .unwrap();
    let service = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a local service");
    let port = service.local_addr().unwrap().port();
    let probe = format!("exec 3<>/dev/tcp/127.0.0.1/{port} && echo connected || echo refused");
    let run = |sandbox: &Sandbox| {
        let cmd = sandbox.create_shell_command("bash", &probe, scope_dir.path());
        async move {
            let out = cmd.unwrap().output().await.unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
    };

    assert_eq!(run(&sandbox).await, "connected", "control: restriction off");

    // Any port that is not the service's stands in for the proxy.
    let proxy_port = if port == 1 { 2 } else { 1 };
    sandbox.set_egress_proxy_addr(Some(std::net::SocketAddr::from((
        [127, 0, 0, 1],
        proxy_port,
    ))));
    assert_eq!(
        run(&sandbox).await,
        "refused",
        "SECURITY: under restriction an unlisted port must be refused"
    );

    sandbox.set_local_tcp_ports(&[port]);
    assert_eq!(
        run(&sandbox).await,
        "connected",
        "a port in [network] local_ports must be reachable"
    );
}
