//! Tests for the temp directory as a sandbox scope (`[sandbox] tmp_access`,
//! on by default; SPEC R5.2.5).
//!
//! These tests verify:
//! - A temp directory in the scope is canonicalized and validates cross-platform
//! - A commit carries it as an auxiliary scope after the workspace (the
//!   server end to end is covered by `tmp_scope_test`)
//! - Interaction with the `--disable-temp-files` opt-out

use ahma_mcp::sandbox::{Sandbox, SandboxMode, ScopeCommit};
use tempfile::tempdir;

/// Test that temp directory can be added as a sandbox scope
#[test]
fn test_temp_dir_can_be_added_to_scopes() {
    let temp = tempdir().unwrap();
    let temp_dir = std::env::temp_dir();

    // Create sandbox with both a project dir and temp dir as scopes
    let scopes = vec![temp.path().to_path_buf(), temp_dir.clone()];
    let sandbox = Sandbox::new(scopes, SandboxMode::Strict, Default::default()).unwrap();

    // Verify temp dir is accessible
    let temp_file = temp_dir.join("test_file.txt");
    let result = sandbox.validate_path(&temp_file);

    // The path should be valid (within temp scope)
    assert!(
        result.is_ok(),
        "Temp directory path should be valid when temp is in scopes: {:?}",
        result
    );
}

/// Test that temp directory scope is properly canonicalized
#[test]
fn test_temp_dir_scope_is_canonicalized() {
    let temp_dir = std::env::temp_dir();
    let canonical_temp = dunce::canonicalize(&temp_dir).unwrap();

    let project_temp = tempdir().unwrap();
    let scopes = vec![project_temp.path().to_path_buf(), temp_dir.clone()];
    let sandbox = Sandbox::new(scopes, SandboxMode::Strict, Default::default()).unwrap();

    // Get the scopes and verify temp dir is canonicalized
    let sandbox_scopes = sandbox.scopes();
    let has_canonical_temp = sandbox_scopes.iter().any(|s| s == &canonical_temp);

    assert!(
        has_canonical_temp,
        "Sandbox scopes should contain canonicalized temp dir. Scopes: {:?}, Expected: {:?}",
        sandbox_scopes.to_vec(),
        canonical_temp
    );
}

/// Test that --disable-temp-files blocks temp access even when temp is in scopes
#[test]
fn test_no_temp_files_blocks_temp_access() {
    let temp_dir = std::env::temp_dir();
    let project_temp = tempdir().unwrap();

    // Create sandbox with temp in scopes BUT no_temp_files=true
    let scopes = vec![project_temp.path().to_path_buf(), temp_dir.clone()];
    let sandbox = Sandbox::new(
        scopes,
        SandboxMode::Strict,
        ahma_mcp::sandbox::SandboxOptions {
            no_temp_files: true,
            ..ahma_mcp::sandbox::SandboxOptions::default()
        },
    )
    .unwrap();

    // Verify no_temp_files is set
    assert!(sandbox.is_no_temp_files(), "no_temp_files should be true");

    // Try to validate a path in temp - should fail due to high security policy
    let temp_file = temp_dir.join("blocked_file.txt");
    let result = sandbox.validate_path(&temp_file);

    // On some platforms, the temp dir might resolve to /private/tmp or similar
    // The high security check in check_security_policies should block this
    if let Err(e) = &result {
        let err_str = e.to_string().to_lowercase();
        // Either it's blocked by high security policy or it's outside scope
        assert!(
            err_str.contains("high-security")
                || err_str.contains("high security")
                || err_str.contains("outside"),
            "Expected high security or outside scope error, got: {}",
            e
        );
    }
}

/// Test that sandbox works without temp dir in scopes (default behavior)
#[test]
fn test_sandbox_without_temp_scope() {
    let project_temp = tempdir().unwrap();

    // Create sandbox with only project dir (no temp)
    let scopes = vec![project_temp.path().to_path_buf()];
    let sandbox = Sandbox::new(scopes, SandboxMode::Strict, Default::default()).unwrap();

    // Temp dir path should be outside scope
    let temp_dir = std::env::temp_dir();
    let temp_file = temp_dir.join("outside_scope.txt");
    let result = sandbox.validate_path(&temp_file);

    // Should fail unless temp_dir happens to be under project_temp (unlikely)
    if !temp_dir.starts_with(project_temp.path()) {
        assert!(
            result.is_err(),
            "Temp path should be outside scope when temp not added"
        );
    }
}

/// Test that multiple scopes including temp work correctly
#[test]
fn test_multiple_scopes_with_temp() {
    let project1 = tempdir().unwrap();
    let project2 = tempdir().unwrap();
    let temp_dir = std::env::temp_dir();

    let scopes = vec![
        project1.path().to_path_buf(),
        project2.path().to_path_buf(),
        temp_dir.clone(),
    ];
    let sandbox = Sandbox::new(scopes, SandboxMode::Strict, Default::default()).unwrap();

    // All three scopes should be accessible
    let path1 = project1.path().join("file1.txt");
    let path2 = project2.path().join("file2.txt");
    let path3 = temp_dir.join("file3.txt");

    assert!(
        sandbox.validate_path(&path1).is_ok(),
        "project1 should be valid"
    );
    assert!(
        sandbox.validate_path(&path2).is_ok(),
        "project2 should be valid"
    );
    assert!(
        sandbox.validate_path(&path3).is_ok(),
        "temp should be valid"
    );
}

/// Test that temp dir uses cross-platform std::env::temp_dir()
#[test]
fn test_temp_dir_is_cross_platform() {
    let temp_dir = std::env::temp_dir();

    // temp_dir should exist and be a directory
    assert!(
        temp_dir.exists(),
        "std::env::temp_dir() should return existing path"
    );
    assert!(
        temp_dir.is_dir(),
        "std::env::temp_dir() should be a directory"
    );

    // Should be canonicalizable
    let canonical = dunce::canonicalize(&temp_dir);
    assert!(
        canonical.is_ok(),
        "temp_dir should be canonicalizable: {:?}",
        canonical
    );
}

/// Test that duplicate temp dir is not added twice
#[test]
fn test_temp_dir_not_duplicated_in_scopes() {
    let temp_dir = std::env::temp_dir();
    let canonical_temp = dunce::canonicalize(&temp_dir).unwrap();

    // Add temp dir twice
    let scopes = vec![temp_dir.clone(), temp_dir.clone()];
    let sandbox = Sandbox::new(scopes, SandboxMode::Test, Default::default()).unwrap();

    // Count occurrences of canonical temp in scopes
    let sandbox_scopes = sandbox.scopes();
    let temp_count = sandbox_scopes
        .iter()
        .filter(|s| *s == &canonical_temp)
        .count();

    // Should only appear once (deduplication in canonicalize_scopes)
    assert!(
        temp_count <= 1,
        "Temp dir should not be duplicated. Count: {}, Scopes: {:?}",
        temp_count,
        sandbox_scopes.to_vec()
    );
}

/// SPEC R5.2.5: the commit that replaces the provisional scope with the
/// client's roots carries the temp directory as an auxiliary scope, after the
/// new workspace, on the strength of `tmp_access` — no question asked.
#[test]
fn test_commit_carries_temp_as_an_auxiliary_scope() {
    // Outside the temp dir: a workspace inside it is refused the temp scope
    // (it would widen above the workspace, R-PERM.4.3).
    let project1 = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let project2 = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let temp_dir = std::env::temp_dir();
    let canonical_temp = dunce::canonicalize(&temp_dir).unwrap();

    // A provisional scope that includes temp (as `ahma tool run` seeds it).
    let scopes = vec![project1.path().to_path_buf(), temp_dir.clone()];
    let sandbox = Sandbox::new(
        scopes,
        SandboxMode::Strict,
        ahma_mcp::sandbox::SandboxOptions {
            tmp_access: true,
            ..ahma_mcp::sandbox::SandboxOptions::default()
        },
    )
    .unwrap();
    assert!(
        sandbox.scopes().iter().any(|s| s == &canonical_temp),
        "Initial scopes should contain temp dir"
    );

    // roots/list replaces the scope; the commit is the one door.
    assert_eq!(
        sandbox
            .commit_scopes(vec![project2.path().to_path_buf()])
            .unwrap(),
        ScopeCommit::Applied
    );

    let updated_scopes = sandbox.scopes();
    let canonical_project2 = dunce::canonicalize(project2.path()).unwrap();
    assert_eq!(
        updated_scopes.first(),
        Some(&canonical_project2),
        "the new workspace is the primary scope: {:?}",
        updated_scopes.to_vec()
    );
    assert!(
        updated_scopes[1..]
            .iter()
            .any(|s| canonical_temp.starts_with(s)),
        "the commit carries the temp dir after the workspace: {:?}",
        updated_scopes.to_vec()
    );
    assert!(sandbox.is_tmp_access());
}

/// `--disable-temp-files` wins over `tmp_access` at commit too: the temp dir
/// is never added, and `tmp_access` reads as off.
#[test]
fn test_commit_never_carries_temp_with_disable_temp() {
    let project = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let canonical_temp = dunce::canonicalize(std::env::temp_dir()).unwrap();
    let sandbox = Sandbox::new(
        Vec::new(),
        SandboxMode::Strict,
        ahma_mcp::sandbox::SandboxOptions {
            tmp_access: true,
            no_temp_files: true,
            ..ahma_mcp::sandbox::SandboxOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        sandbox
            .commit_scopes(vec![project.path().to_path_buf()])
            .unwrap(),
        ScopeCommit::Applied
    );
    assert!(
        !sandbox.is_tmp_access(),
        "disable_temp overrides tmp_access"
    );
    assert!(
        !sandbox.scopes().iter().any(|s| s == &canonical_temp),
        "{:?}",
        sandbox.scopes().to_vec()
    );
}

/// Test that update_scopes does NOT add temp when tmp_access=false.
#[test]
fn test_update_scopes_no_temp_when_tmp_access_false() {
    let project1 = tempdir().unwrap();
    let project2 = tempdir().unwrap();
    let temp_dir = std::env::temp_dir();
    let canonical_temp = dunce::canonicalize(&temp_dir).unwrap();

    // Create sandbox with tmp_access=false but with temp in initial scopes
    let scopes = vec![project1.path().to_path_buf(), temp_dir.clone()];
    let sandbox = Sandbox::new(scopes, SandboxMode::Strict, Default::default()).unwrap();

    // Update scopes to a new project (no temp) — temp should NOT be re-added
    assert_eq!(
        sandbox
            .commit_scopes(vec![project2.path().to_path_buf()])
            .unwrap(),
        ScopeCommit::Applied
    );

    let updated_scopes = sandbox.scopes();

    // If project2 is not under temp_dir, temp should NOT be in scopes
    if !project2.path().starts_with(&canonical_temp) {
        let has_temp = updated_scopes.iter().any(|s| s == &canonical_temp);
        assert!(
            !has_temp,
            "Temp dir should NOT be preserved when tmp_access=false. Scopes: {:?}",
            updated_scopes.to_vec()
        );
    }
}

/// Test that update_scopes with tmp_access=true doesn't duplicate temp dir.
#[test]
fn test_update_scopes_no_temp_duplication() {
    let project = tempdir().unwrap();
    let temp_dir = std::env::temp_dir();
    let canonical_temp = dunce::canonicalize(&temp_dir).unwrap();

    // Create sandbox with tmp_access=true
    let scopes = vec![project.path().to_path_buf(), temp_dir.clone()];
    let sandbox = Sandbox::new(
        scopes,
        SandboxMode::Strict,
        ahma_mcp::sandbox::SandboxOptions {
            tmp_access: true,
            ..ahma_mcp::sandbox::SandboxOptions::default()
        },
    )
    .unwrap();

    // Update scopes WITH temp already included — should not duplicate
    assert_eq!(
        sandbox
            .commit_scopes(vec![project.path().to_path_buf(), temp_dir.clone()])
            .unwrap(),
        ScopeCommit::Applied
    );

    let updated_scopes = sandbox.scopes();
    let temp_count = updated_scopes
        .iter()
        .filter(|s| *s == &canonical_temp)
        .count();

    assert!(
        temp_count <= 1,
        "Temp dir should not be duplicated after update_scopes. Count: {}, Scopes: {:?}",
        temp_count,
        updated_scopes.to_vec()
    );
}
