//! Migration of the retired `~/.config/ahma/approvals.json` into the unified
//! permission ledger (SPEC R-PERM.1).
//!
//! This lives in its own test binary because it drives *two* process-global env
//! seams at once (`AHMA_TEST_HOME` for `~/.ahma`, `AHMA_CONFIG_DIR` for the
//! legacy tree), and because it must observe the migration running exactly once
//! — which the `OnceLock` in `approvals` guarantees per process, not per test.
//!
//! What matters here is not just "the grants moved". It is that a user who has
//! been trusting `cargo_build` for months does not silently start getting
//! re-prompted, and that if this migration is ever wrong, their record of what
//! they chose to trust is still on disk to recover from — hence the archive
//! assertion rather than a deletion.

use std::path::Path;

use ahma_common::config::AhmaSettings;
use ahma_common::permissions::workspace_key;
use ahma_core::approvals::{GrantStatus, grant_status, is_tool_approved};
use tempfile::TempDir;

#[tokio::test]
async fn legacy_approvals_are_migrated_once_and_the_old_file_is_archived() {
    let home = TempDir::new().unwrap();
    let legacy_root = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();

    // A pre-existing approvals.json, exactly as a released ahma would have left it.
    let legacy_dir = legacy_root.path().join("ahma");
    std::fs::create_dir_all(&legacy_dir).unwrap();
    let legacy_file = legacy_dir.join("approvals.json");
    let key = workspace_key(workspace.path());
    std::fs::write(
        &legacy_file,
        serde_json::json!({
            key.to_string_lossy(): ["cargo_build", "list_dir"]
        })
        .to_string(),
    )
    .unwrap();

    // SAFETY: set once, before any approvals call; single-test binary, so there is
    // exactly one writer of these process-global vars.
    unsafe {
        std::env::set_var("AHMA_TEST_HOME", home.path());
        std::env::set_var("AHMA_CONFIG_DIR", legacy_root.path());
    }

    // The very first read through the approvals API triggers the migration.
    assert!(
        is_tool_approved(workspace.path(), "cargo_build").await
            || grant_status(workspace.path(), "cargo_build") == GrantStatus::ApprovedHere,
        "a grant the user already made must survive the move to the new ledger — \
         migrating must not silently re-prompt them"
    );
    // `grant_status` is the sync path and forces the migration even if the async
    // read raced ahead of it; after it, the grant is unambiguously in the ledger.
    assert_eq!(
        grant_status(workspace.path(), "list_dir"),
        GrantStatus::ApprovedHere,
        "every grant in the legacy file migrates, not just the first"
    );

    let settings_file = home.path().join(".ahma").join("settings.toml");
    let settings = AhmaSettings::load_from_result(&settings_file).expect("ledger parses");
    let migrated = settings
        .permissions
        .tool_approvals
        .iter()
        .find(|a| a.workspace == key)
        .expect("workspace entry exists in the ledger");
    assert_eq!(migrated.tools, vec!["cargo_build", "list_dir"]);
    assert_eq!(
        migrated.granted_by.as_deref(),
        Some("migrated"),
        "provenance records that these came from the legacy store, not a fresh prompt"
    );

    // Non-destructive: the legacy file is renamed aside, never deleted. If this
    // migration is ever wrong, the user's record of what they trusted survives.
    assert!(
        !legacy_file.exists(),
        "the legacy file no longer sits at the path ahma reads"
    );
    assert!(
        archived(&legacy_dir),
        "the legacy file is archived (.migrated), not destroyed"
    );

    // Idempotent: a second pass finds no legacy file and changes nothing.
    let before = std::fs::read_to_string(&settings_file).unwrap();
    ahma_common::permissions::migrate_legacy_approvals(&settings_file).unwrap();
    let after = std::fs::read_to_string(&settings_file).unwrap();
    assert_eq!(before, after, "re-running the migration is a no-op");
}

/// Whether an archived (`.migrated`) legacy file exists in `dir`.
fn archived(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().ends_with("json.migrated"))
        })
        .unwrap_or(false)
}
