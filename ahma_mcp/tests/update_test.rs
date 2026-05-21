//! Integration tests for `ahma update`.

use ahma_mcp::update::{
    UpdateMode, build_cargo_install_command, classify_ref, default_install_dir,
};
use std::path::Path;

#[test]
fn test_classify_ref_branch_and_release() {
    assert_eq!(classify_ref(None), UpdateMode::LatestRelease);
    assert_eq!(
        classify_ref(Some("0.6.7")),
        UpdateMode::TaggedRelease {
            tag: "v0.6.7".to_string()
        }
    );
    assert_eq!(
        classify_ref(Some("feature/update")),
        UpdateMode::GitRef {
            branch: "feature/update".to_string()
        }
    );
}

#[test]
fn test_build_cargo_install_command_includes_branch() {
    let cmd = build_cargo_install_command("main", Path::new("/tmp/.local/bin"));
    assert!(cmd.iter().any(|a| a == "main"));
    assert!(cmd.iter().any(|a| a == "--branch"));
    assert!(cmd.iter().any(|a| a == "--root"));
}

#[test]
fn test_default_install_dir_ends_with_local_bin() {
    let dir = default_install_dir().expect("home dir");
    assert!(dir.ends_with(".local/bin") || dir.ends_with(".local\\bin"));
}
