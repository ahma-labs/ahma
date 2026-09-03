//! Unit tests for sandbox.rs - Kernel-level sandboxing
//!
//! These tests cover:
//! - Path validation and normalization
//! - Sandbox scope initialization
//! - SandboxError formatting
//! - Platform-specific sandbox checks

use ahma_mcp::sandbox::{Sandbox, SandboxMode, ScopeCommit, normalize_path_lexically};
use std::path::{Path, PathBuf};
use tempfile::tempdir;

#[test]
fn test_normalize_path_removes_single_dot() {
    let path = Path::new("/home/user/./project");
    let normalized = normalize_path_lexically(path);
    assert_eq!(normalized, PathBuf::from("/home/user/project"));
}

#[test]
fn test_normalize_path_resolves_parent_dir() {
    let path = Path::new("/home/user/project/../other");
    let normalized = normalize_path_lexically(path);
    assert_eq!(normalized, PathBuf::from("/home/user/other"));
}

#[test]
fn test_normalize_path_multiple_parent_dirs() {
    let path = Path::new("/home/user/a/b/c/../../d");
    let normalized = normalize_path_lexically(path);
    assert_eq!(normalized, PathBuf::from("/home/user/a/d"));
}

// ============= Test Mode Detection Tests =============

#[test]
fn test_is_test_mode_on_sandbox_instance() {
    let temp_test = tempdir().unwrap();
    let sandbox = Sandbox::new(
        vec![temp_test.path().to_path_buf()],
        SandboxMode::Test,
        false,
        false,
        false,
    )
    .unwrap();
    assert!(sandbox.is_test_mode());

    let temp = tempdir().unwrap();
    let sandbox_strict = Sandbox::new(
        vec![temp.path().to_path_buf()],
        SandboxMode::Strict,
        false,
        false,
        false,
    )
    .unwrap();
    assert!(!sandbox_strict.is_test_mode());
}

#[test]
fn test_is_no_temp_files_on_sandbox_instance() {
    let temp = tempdir().unwrap();
    let sandbox = Sandbox::new(
        vec![temp.path().to_path_buf()],
        SandboxMode::Strict,
        true,
        false,
        false,
    )
    .unwrap();
    assert!(sandbox.is_no_temp_files());

    let temp2 = tempdir().unwrap();
    let sandbox_default = Sandbox::new(
        vec![temp2.path().to_path_buf()],
        SandboxMode::Strict,
        false,
        false,
        false,
    )
    .unwrap();
    assert!(!sandbox_default.is_no_temp_files());
}

// ============= Path Validation Logic Tests =============

#[test]
fn test_path_validation_in_scope() {
    let temp = tempdir().unwrap();
    let scope = temp.path().to_path_buf();
    let sandbox = Sandbox::new(
        vec![scope.clone()],
        SandboxMode::Strict,
        false,
        false,
        false,
    )
    .unwrap();

    let valid_path = scope.join("test.txt");
    assert!(sandbox.validate_path(&valid_path).is_ok());
}

#[test]
fn test_path_validation_outside_scope() {
    let temp = tempdir().unwrap();
    let scope = temp.path().to_path_buf();
    let sandbox = Sandbox::new(vec![scope], SandboxMode::Strict, false, false, false).unwrap();

    let outside_path = PathBuf::from("/etc/passwd");
    assert!(sandbox.validate_path(&outside_path).is_err());
}

#[cfg(unix)]
#[test]
fn test_path_validation_accepts_symlink_alias_scope_for_nonexistent_nested_path() {
    let temp = tempdir().unwrap();
    let real_root = temp.path().join("real_root");
    std::fs::create_dir_all(&real_root).unwrap();

    let alias_root = temp.path().join("alias_root");
    std::os::unix::fs::symlink(&real_root, &alias_root).unwrap();

    let sandbox = Sandbox::new(
        vec![alias_root.clone()],
        SandboxMode::Strict,
        false,
        false,
        false,
    )
    .unwrap();

    // `nested` does not exist, so path resolution falls back to lexical normalization.
    // The alias scope must still be accepted.
    let alias_nested_target = alias_root.join("nested/new_file.txt");
    assert!(sandbox.validate_path(&alias_nested_target).is_ok());
}

// ============= Persistent sandbox_dir (--sandbox) tests =============

/// update_scopes preserves the sandbox_dir (~/sandbox) when roots/list fires.
#[test]
fn test_update_scopes_preserves_sandbox_dir() {
    let sandbox_dir_tmp = tempdir().unwrap();
    let workspace_tmp = tempdir().unwrap();

    let sandbox_dir = sandbox_dir_tmp.path().to_path_buf();
    let workspace = workspace_tmp.path().to_path_buf();

    // Start with sandbox_dir as the initial scope.
    let sandbox = Sandbox::new(
        vec![sandbox_dir.clone()],
        SandboxMode::Test,
        false,
        false,
        false,
    )
    .unwrap()
    .with_scratch_dir(Some(sandbox_dir.clone()));

    // Simulate roots/list arriving with a real workspace root.
    assert_eq!(
        sandbox.commit_scopes(vec![workspace.clone()]).unwrap(),
        ScopeCommit::Applied
    );

    let scopes = sandbox.scopes();
    assert!(
        scopes.contains(&workspace),
        "workspace must be in scopes after update: {:?}",
        scopes.to_vec()
    );
    assert!(
        scopes.contains(&sandbox_dir),
        "sandbox_dir must survive update_scopes: {:?}",
        scopes.to_vec()
    );
}

// ============= One-shot commit latch (SPEC R5.1.1) tests =============

/// A fresh sandbox is not yet committed; the first commit wins and every
/// later commit loses. This is the latch that prevents a repeat
/// `roots/list` / `roots/list_changed` from re-deriving (and widening) scope on
/// the direct-stdio path, mirroring the HTTP bridge's post-lock no-op.
#[test]
fn test_commit_latch_is_one_shot() {
    let tmp = tempdir().unwrap();
    let sandbox = Sandbox::new(
        vec![tmp.path().to_path_buf()],
        SandboxMode::Test,
        false,
        false,
        false,
    )
    .unwrap();

    assert!(
        !sandbox.is_committed(),
        "new sandbox must start uncommitted"
    );
    assert_eq!(
        sandbox.commit_existing_scopes(),
        ScopeCommit::Applied,
        "first commit must win the latch"
    );
    assert!(
        sandbox.is_committed(),
        "sandbox must report committed after winning"
    );
    assert_eq!(
        sandbox.commit_existing_scopes(),
        ScopeCommit::AlreadyCommitted,
        "second commit must lose (one-shot)"
    );
    assert_eq!(
        sandbox.commit_existing_scopes(),
        ScopeCommit::AlreadyCommitted,
        "every subsequent commit must keep losing"
    );
    assert!(sandbox.is_committed(), "commit state must remain latched");
}

/// Scope immutability holds by construction: `commit_scopes` is the only door
/// that replaces scopes, and once the latch is claimed it refuses to touch the
/// locked scope — there is no API left that can (SPEC R5.1.1 / R5.2.2).
#[test]
fn test_committed_scope_cannot_be_replaced() {
    let first = tempdir().unwrap();
    let second = tempdir().unwrap();
    let sandbox = Sandbox::new(
        vec![first.path().to_path_buf()],
        SandboxMode::Test,
        false,
        false,
        false,
    )
    .unwrap();

    assert_eq!(sandbox.commit_existing_scopes(), ScopeCommit::Applied);
    let locked = sandbox.scopes().to_vec();

    assert_eq!(
        sandbox
            .commit_scopes(vec![second.path().to_path_buf()])
            .unwrap(),
        ScopeCommit::AlreadyCommitted,
        "a second commit must be a tolerated no-op"
    );
    assert_eq!(
        sandbox.scopes().to_vec(),
        locked,
        "the locked scope must be untouched by the losing commit"
    );
}

/// The commit latch survives `Clone` (the service clones its handler), so a
/// clone cannot re-win the latch and re-apply scopes.
#[test]
fn test_commit_latch_survives_clone() {
    let tmp = tempdir().unwrap();
    let sandbox = Sandbox::new(
        vec![tmp.path().to_path_buf()],
        SandboxMode::Test,
        false,
        false,
        false,
    )
    .unwrap();

    assert_eq!(
        sandbox.commit_existing_scopes(),
        ScopeCommit::Applied,
        "first commit wins"
    );
    let cloned = sandbox.clone();
    assert!(
        cloned.is_committed(),
        "clone must observe the committed latch"
    );
    assert_eq!(
        cloned.commit_existing_scopes(),
        ScopeCommit::AlreadyCommitted,
        "clone must not be able to re-win the latch"
    );
}

/// When no sandbox_dir is set, update_scopes replaces scopes normally.
#[test]
fn test_update_scopes_no_sandbox_dir_replaces() {
    let old_tmp = tempdir().unwrap();
    let new_tmp = tempdir().unwrap();

    let old_scope = old_tmp.path().to_path_buf();
    let new_scope = new_tmp.path().to_path_buf();

    let sandbox = Sandbox::new(
        vec![old_scope.clone()],
        SandboxMode::Test,
        false,
        false,
        false,
    )
    .unwrap();
    // No sandbox_dir set.

    assert_eq!(
        sandbox.commit_scopes(vec![new_scope.clone()]).unwrap(),
        ScopeCommit::Applied
    );

    let scopes = sandbox.scopes();
    assert!(
        scopes.contains(&new_scope),
        "new scope must be present: {:?}",
        scopes.to_vec()
    );
    assert!(
        !scopes.contains(&old_scope),
        "old scope must be replaced: {:?}",
        scopes.to_vec()
    );
}

/// update_scopes preserves both sandbox_dir AND temp when both are set.
#[test]
fn test_update_scopes_preserves_sandbox_dir_and_tmp() {
    let sandbox_dir_tmp = tempdir().unwrap();
    let workspace_tmp = tempdir().unwrap();

    let sandbox_dir = sandbox_dir_tmp.path().to_path_buf();
    let workspace = workspace_tmp.path().to_path_buf();
    let canonical_temp = dunce::canonicalize(std::env::temp_dir()).unwrap();

    let sandbox = Sandbox::new(
        vec![sandbox_dir.clone()],
        SandboxMode::Test,
        false,
        false,
        true, // tmp_access = true
    )
    .unwrap()
    .with_scratch_dir(Some(sandbox_dir.clone()));

    assert_eq!(
        sandbox.commit_scopes(vec![workspace.clone()]).unwrap(),
        ScopeCommit::Applied
    );

    let scopes = sandbox.scopes();
    assert!(
        scopes.contains(&workspace),
        "workspace present: {:?}",
        scopes.to_vec()
    );
    assert!(
        scopes.contains(&sandbox_dir),
        "sandbox_dir present: {:?}",
        scopes.to_vec()
    );
    assert!(
        scopes.contains(&canonical_temp),
        "temp dir present when tmp_access=true: {:?}",
        scopes.to_vec()
    );
}

/// with_scratch_dir / sandbox_dir accessor roundtrip.
#[test]
fn test_sandbox_dir_accessor() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path().to_path_buf();

    let sandbox = Sandbox::new(vec![dir.clone()], SandboxMode::Test, false, false, false)
        .unwrap()
        .with_scratch_dir(Some(dir.clone()));

    assert_eq!(sandbox.scratch_dir(), Some(&dir));

    let sandbox_no_dir =
        Sandbox::new(vec![dir.clone()], SandboxMode::Test, false, false, false).unwrap();
    assert_eq!(sandbox_no_dir.scratch_dir(), None);
}
