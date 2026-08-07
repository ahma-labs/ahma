//! Integration tests for session sandbox scope handling.
//!
//! These tests reproduce the issue where the HTTP server's sandbox scope
//! is locked to the server's working directory instead of respecting
//! the client's workspace roots provided via the MCP protocol.
//!
//! Issue: When running `./scripts/ahma-http-server.sh` from `/Users/paul/github/ahma_mcp`,
//! connecting from VS Code with workspace `/Users/paul/github/nb_lifeline3/android_lifeline`
//! results in: "Path is outside the sandbox root"

use ahma_http_bridge::session::{McpRoot, SessionManager, SessionManagerConfig};
use ahma_http_bridge::{
    DEFAULT_HANDSHAKE_TIMEOUT_SECS, DEFAULT_REQUEST_TIMEOUT_SECS, DEFAULT_TOOL_CALL_TIMEOUT_SECS,
};
use std::path::PathBuf;
use std::sync::Arc;

/// Helper to create a SessionManager with test configuration
fn create_test_session_manager(default_scope: Option<PathBuf>) -> Arc<SessionManager> {
    let config = SessionManagerConfig {
        server_command: "echo".to_string(), // Use echo as a safe subprocess
        server_args: vec!["test".to_string()],
        default_scope,
        enable_colored_output: false,
        handshake_timeout_secs: DEFAULT_HANDSHAKE_TIMEOUT_SECS,
        request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
        tool_call_timeout_secs: DEFAULT_TOOL_CALL_TIMEOUT_SECS,
        max_sessions: 100,
        peer_factory: None,
    };
    Arc::new(SessionManager::new(config))
}

/// Convert a `Path` to a `file://` URI that is valid on both Unix and Windows.
///
/// Windows: `C:\foo\bar` → `"file:///C:/foo/bar"`
/// Unix:    `/tmp/bar`   → `"file:///tmp/bar"`
fn path_to_file_uri(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    #[cfg(windows)]
    {
        let forward = s.replace('\\', "/");
        format!("file:///{}", forward)
    }
    #[cfg(not(windows))]
    {
        format!("file://{}", s)
    }
}

/// Test that verifies sandbox scope mismatch scenario.
///
/// This reproduces the bug where:
/// 1. Server starts with sandbox scope = /path/to/ahma_mcp
/// 2. Client connects with workspace = /path/to/other_project
/// 3. Client tries to access file in their workspace
/// 4. Server rejects because file is outside server's sandbox
///
/// The fix: Session sandbox scope should be set from client's roots/list response,
/// not from server's startup directory.
#[tokio::test]
async fn test_sandbox_scope_should_use_client_roots_not_server_cwd() {
    // Server started with a temp dir as default scope
    let server_default_scope = std::env::temp_dir().join("ahma_mcp");

    // Client's workspace is a different project
    let client_workspace = std::env::temp_dir().join("android_lifeline");

    let session_manager = create_test_session_manager(Some(server_default_scope.clone()));

    // Create a session
    let session_id = session_manager
        .create_session()
        .await
        .expect("Should create session");

    // Client provides their workspace root via roots/list response
    let client_roots = vec![McpRoot {
        uri: path_to_file_uri(&client_workspace),
        name: Some("android_lifeline".to_string()),
    }];

    // Lock sandbox to client's roots (this is what should happen)
    session_manager
        .lock_sandbox(&session_id, &client_roots)
        .await
        .expect("Should lock sandbox");

    // Get the session and verify sandbox scope
    let session = session_manager
        .get_session(&session_id)
        .expect("Session should exist");

    let sandbox_scope = session
        .get_sandbox_scope()
        .await
        .expect("Sandbox scope should be set");

    // CRITICAL: Sandbox scope should be client's workspace, NOT server's CWD
    assert_eq!(
        sandbox_scope, client_workspace,
        "Sandbox scope should be client's workspace ({:?}), not server's CWD ({:?})",
        client_workspace, server_default_scope
    );
}

/// Test that empty roots are rejected (security feature).
///
/// Empty roots should NOT fall back to a default scope because this could
/// lead to over-permissive behavior. The client must provide at least one
/// valid file:// URI.
#[tokio::test]
async fn test_sandbox_scope_rejects_empty_roots() {
    let session_manager = create_test_session_manager(None);

    let session_id = session_manager
        .create_session()
        .await
        .expect("Should create session");

    // Client provides empty roots (no workspace folders)
    let empty_roots: Vec<McpRoot> = vec![];

    let result = session_manager
        .lock_sandbox(&session_id, &empty_roots)
        .await;

    // Empty roots should be rejected
    assert!(
        result.is_err(),
        "Empty roots should be rejected, not fall back to default scope"
    );

    let session = session_manager
        .get_session(&session_id)
        .expect("Session should exist");

    // Sandbox should not be locked after rejection
    assert!(
        !session.is_sandbox_locked(),
        "Sandbox should not be locked after empty roots rejection"
    );
}

/// Test that sandbox scope cannot be changed after locking.
#[tokio::test]
async fn test_sandbox_scope_immutable_after_lock() {
    let server_default_scope = std::env::temp_dir().join("server");
    let session_manager = create_test_session_manager(Some(server_default_scope));

    let session_id = session_manager
        .create_session()
        .await
        .expect("Should create session");

    // First lock
    let first_roots = vec![McpRoot {
        uri: path_to_file_uri(&std::env::temp_dir().join("project_a")),
        name: None,
    }];

    session_manager
        .lock_sandbox(&session_id, &first_roots)
        .await
        .expect("First lock should succeed");

    // Attempt second lock with different roots
    let second_roots = vec![McpRoot {
        uri: path_to_file_uri(&std::env::temp_dir().join("project_b")),
        name: None,
    }];

    let result = session_manager
        .lock_sandbox(&session_id, &second_roots)
        .await;

    // Second lock returns Ok(false) - sandbox was already locked, no restart
    assert!(
        result.is_ok(),
        "Second lock_sandbox call should succeed but return false"
    );
    assert!(
        !result.unwrap(),
        "Second lock_sandbox should return false (already locked, no restart)"
    );
}

/// roots/list_changed after sandbox lock is a tolerated no-op: the committed
/// scope is immutable (R5.1/R5.2.2), so the notification is ignored and the
/// session is kept alive (no 403, no termination, no stdio-proxy respawn churn).
#[tokio::test]
async fn test_roots_change_after_lock_is_tolerated_noop() {
    let server_default_scope = std::env::temp_dir().join("server");
    let session_manager = create_test_session_manager(Some(server_default_scope));

    let session_id = session_manager
        .create_session()
        .await
        .expect("Should create session");

    // Lock sandbox
    let roots = vec![McpRoot {
        uri: path_to_file_uri(&std::env::temp_dir().join("project_locked")),
        name: None,
    }];

    session_manager
        .lock_sandbox(&session_id, &roots)
        .await
        .expect("Should lock sandbox");
    let locked_scopes = session_manager
        .get_session(&session_id)
        .unwrap()
        .get_sandbox_scopes()
        .await;

    // A client roots change after lock must be tolerated as a no-op.
    let result = session_manager.handle_roots_changed(&session_id).await;
    assert!(
        matches!(result, Ok(true)),
        "Roots change after lock should be a tolerated no-op (Ok(true)), got {result:?}"
    );

    // Session must survive...
    assert!(
        session_manager.session_exists(&session_id),
        "Session must NOT be terminated by a benign roots change"
    );
    // ...and the locked scope must be unchanged (never widened).
    let after_scopes = session_manager
        .get_session(&session_id)
        .unwrap()
        .get_sandbox_scopes()
        .await;
    assert_eq!(
        locked_scopes, after_scopes,
        "Locked sandbox scope must be immutable across a roots change"
    );

    // Scope commit must NOT be reverted to AwaitingRoots by the roots change.
    // (lock_sandbox transitions to Configuring; Active requires a real
    // subprocess `configured` notification, which this unit test doesn't have.)
    assert!(
        !matches!(
            session_manager
                .get_session(&session_id)
                .unwrap()
                .current_sandbox_state(),
            ahma_common::sandbox_state::SandboxState::AwaitingRoots
        ),
        "Sandbox commit must not revert to AwaitingRoots after a tolerated roots change"
    );
}

/// A roots/list_changed received BEFORE lock (AwaitingRoots) is allowed and
/// signals "proceed with the handshake" (Ok(false)), not a no-op.
#[tokio::test]
async fn test_roots_change_before_lock_proceeds() {
    let session_manager = create_test_session_manager(None);
    let session_id = session_manager
        .create_session()
        .await
        .expect("Should create session");

    let result = session_manager.handle_roots_changed(&session_id).await;
    assert!(
        matches!(result, Ok(false)),
        "Roots change before lock should proceed with handshake (Ok(false)), got {result:?}"
    );
    assert!(
        session_manager.session_exists(&session_id),
        "Session must survive a pre-lock roots change"
    );
}

/// Test that multiple sessions have independent sandbox scopes.
#[tokio::test]
async fn test_multiple_sessions_have_independent_sandbox_scopes() {
    let server_default_scope = std::env::temp_dir().join("server");
    let session_manager = create_test_session_manager(Some(server_default_scope));

    // Create two sessions (simulating two VS Code windows)
    let session1_id = session_manager
        .create_session()
        .await
        .expect("Should create session 1");

    let session2_id = session_manager
        .create_session()
        .await
        .expect("Should create session 2");

    // Each session has different workspace
    let path_a = std::env::temp_dir().join("project_a");
    let path_b = std::env::temp_dir().join("project_b");

    let roots1 = vec![McpRoot {
        uri: path_to_file_uri(&path_a),
        name: Some("Project A".to_string()),
    }];

    let roots2 = vec![McpRoot {
        uri: path_to_file_uri(&path_b),
        name: Some("Project B".to_string()),
    }];

    session_manager
        .lock_sandbox(&session1_id, &roots1)
        .await
        .expect("Should lock session 1 sandbox");

    session_manager
        .lock_sandbox(&session2_id, &roots2)
        .await
        .expect("Should lock session 2 sandbox");

    // Verify each session has its own sandbox scope
    let session1 = session_manager
        .get_session(&session1_id)
        .expect("Session 1 should exist");
    let session2 = session_manager
        .get_session(&session2_id)
        .expect("Session 2 should exist");

    let scope1 = session1
        .get_sandbox_scope()
        .await
        .expect("Session 1 should have sandbox scope");
    let scope2 = session2
        .get_sandbox_scope()
        .await
        .expect("Session 2 should have sandbox scope");

    assert_eq!(scope1, path_a);
    assert_eq!(scope2, path_b);

    assert_ne!(
        scope1, scope2,
        "Sessions should have independent sandbox scopes"
    );
}

/// Test that file:// URI prefix is correctly stripped from roots.
#[tokio::test]
async fn test_file_uri_prefix_correctly_stripped() {
    let server_default_scope = std::env::temp_dir().join("server");
    let session_manager = create_test_session_manager(Some(server_default_scope));

    let session_id = session_manager
        .create_session()
        .await
        .expect("Should create session");

    // Test various URI formats
    let test_path = std::env::temp_dir().join("my_project");
    let roots = vec![McpRoot {
        uri: path_to_file_uri(&test_path),
        name: None,
    }];

    session_manager
        .lock_sandbox(&session_id, &roots)
        .await
        .expect("Should lock sandbox");

    let session = session_manager
        .get_session(&session_id)
        .expect("Session should exist");

    let sandbox_scope = session
        .get_sandbox_scope()
        .await
        .expect("Should have scope");

    // Should be the path without file:// prefix
    assert_eq!(
        sandbox_scope, test_path,
        "file:// prefix should be stripped"
    );
}

/// Test session termination cleanup.
#[tokio::test]
async fn test_session_termination_removes_session() {
    let server_default_scope = std::env::temp_dir().join("server");
    let session_manager = create_test_session_manager(Some(server_default_scope));

    let session_id = session_manager
        .create_session()
        .await
        .expect("Should create session");

    assert!(
        session_manager.session_exists(&session_id),
        "Session should exist after creation"
    );

    session_manager
        .terminate_session(
            &session_id,
            ahma_http_bridge::session::SessionTerminationReason::ClientRequested,
        )
        .await
        .expect("Should terminate session");

    assert!(
        !session_manager.session_exists(&session_id),
        "Session should not exist after termination"
    );
}
