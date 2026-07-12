//! Integration coverage for the persistent tool-approval store
//! (`ahma_core::approvals`), now backed by the unified permission ledger
//! (`~/.ahma/settings.toml`, `[permissions].tool_approvals` — SPEC R-PERM.1).
//!
//! The module's in-crate unit tests already exercise the *pure* helpers
//! (`classify`, `reask_note_for`). What this file pins is the **on-disk
//! persistence round-trip** that actually gates whether a sandboxed agent
//! re-prompts for a tool: `remember_tool_approval` → `is_tool_approved` /
//! `grant_status` / `reask_note`, reading and writing a real `settings.toml`.
//!
//! These are security-relevant invariants. A silent break here is exactly the
//! kind that gets a change reverted:
//!   * **fail closed** — a missing/unreadable ledger must read as "not approved"
//!     so the caller prompts rather than silently allowing a tool;
//!   * **workspace scoping** — a grant in one workspace must NOT approve the
//!     same tool in a different workspace (it only triggers a re-ask);
//!   * **round-trip durability** — what the (sync) TUI writer persists must be
//!     what the (async) agent reader observes;
//!   * **idempotency** — re-granting must not duplicate entries;
//!   * **migration** — an existing `~/.config/ahma/approvals.json` must be folded
//!     into the ledger without losing a grant, and without destroying the file.
//!
//! `AHMA_TEST_HOME` is the documented test seam for `~/.ahma`
//! (`config::ahma_home_dir` honors it in debug builds); `AHMA_CONFIG_DIR` still
//! redirects the *legacy* location so the migration path is testable. Both are
//! process-global, so each scenario is pinned in a SINGLE test with exactly one
//! writer of the env per test binary — correct under both `cargo nextest`
//! (process per test) and `cargo test` (threads sharing one process).

use std::path::{Path, PathBuf};

use ahma_common::config::AhmaSettings;
use ahma_common::permissions::workspace_key;
use ahma_core::approvals::{
    GrantStatus, grant_status, is_tool_approved, reask_note, remember_tool_approval,
};
use tempfile::TempDir;

/// Path of the ledger given the `AHMA_TEST_HOME` root.
fn settings_file(home: &Path) -> PathBuf {
    home.join(".ahma").join("settings.toml")
}

/// Tools recorded for `workspace` in the on-disk ledger (empty if absent).
fn grants_for(file: &Path, workspace: &Path) -> Vec<String> {
    let settings = AhmaSettings::load_from_result(file).expect("ledger should parse");
    let key = workspace_key(workspace);
    settings
        .permissions
        .tool_approvals
        .iter()
        .find(|a| a.workspace == key)
        .map(|a| a.tools.clone())
        .unwrap_or_default()
}

#[tokio::test]
async fn approval_grants_persist_and_are_workspace_scoped() {
    // Fresh, empty home — no settings.toml exists yet.
    let home = TempDir::new().unwrap();
    // SAFETY: set once, before any approvals call; single-test binary so no
    // other thread/test races on this process-global env var.
    unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
    let file = settings_file(home.path());

    // Two distinct, real workspaces so canonicalisation yields stable, different keys.
    let ws_a_dir = TempDir::new().unwrap();
    let ws_b_dir = TempDir::new().unwrap();
    let ws_a = ws_a_dir.path();
    let ws_b = ws_b_dir.path();

    // ---- Fail closed: nothing granted, no ledger on disk ----
    assert!(
        !is_tool_approved(ws_a, "cargo_build").await,
        "an ungranted tool with no ledger must read as NOT approved (fail closed)"
    );
    assert_eq!(
        grant_status(ws_a, "cargo_build"),
        GrantStatus::NewWorkspace,
        "a workspace with zero grants is a brand-new sandbox"
    );
    assert!(
        reask_note(ws_a, "cargo_build").is_some(),
        "a fresh workspace should carry an explanatory re-ask note"
    );
    assert!(
        !file.exists(),
        "merely reading approvals must not create the ledger"
    );

    // ---- Grant + durable round-trip (sync writer → async reader) ----
    remember_tool_approval(ws_a, "cargo_build").expect("persisting a grant should succeed");
    assert!(
        is_tool_approved(ws_a, "cargo_build").await,
        "a tool granted in this workspace must read back as approved"
    );
    assert_eq!(
        grant_status(ws_a, "cargo_build"),
        GrantStatus::ApprovedHere,
        "granted-here status after persisting"
    );
    assert!(
        reask_note(ws_a, "cargo_build").is_none(),
        "an already-approved tool needs no re-ask note"
    );
    assert_eq!(
        grants_for(&file, ws_a),
        vec!["cargo_build".to_string()],
        "the grant must be persisted under the workspace's canonical key"
    );

    // ---- The grant lands in the one ledger, not a second file ----
    assert!(
        file.exists(),
        "grants live in ~/.ahma/settings.toml — the single control-plane file the \
         sandbox never includes (R-PERM.1)"
    );
    assert!(
        !home.path().join(".config").join("ahma").exists(),
        "nothing may be written to the retired ~/.config/ahma tree"
    );

    // ---- Workspace scoping: a grant must NOT leak to another workspace ----
    assert!(
        !is_tool_approved(ws_b, "cargo_build").await,
        "a grant in workspace A must NOT approve the same tool in workspace B"
    );
    assert_eq!(
        grant_status(ws_b, "cargo_build"),
        GrantStatus::GrantedElsewhere,
        "the tool is known elsewhere, so workspace B should re-ask, not auto-allow"
    );
    assert!(
        reask_note(ws_b, "cargo_build").is_some(),
        "granted-elsewhere should surface a re-confirm note"
    );

    // ---- Known workspace, unseen tool ----
    assert!(
        !is_tool_approved(ws_a, "rm").await,
        "an unseen tool in an otherwise-known workspace is not approved"
    );
    assert_eq!(
        grant_status(ws_a, "rm"),
        GrantStatus::Unseen,
        "workspace has grants, just not for this tool"
    );
    assert!(
        reask_note(ws_a, "rm").is_none(),
        "an established workspace needs no extra note for a merely-unseen tool"
    );

    // ---- Idempotency + sorted multi-tool persistence ----
    remember_tool_approval(ws_a, "cargo_build").expect("re-granting should succeed");
    assert_eq!(
        grants_for(&file, ws_a),
        vec!["cargo_build".to_string()],
        "re-granting the same tool must not duplicate the entry"
    );
    remember_tool_approval(ws_a, "cargo_test").expect("granting a second tool should succeed");
    assert_eq!(
        grants_for(&file, ws_a),
        vec!["cargo_build".to_string(), "cargo_test".to_string()],
        "multiple grants for one workspace are stored sorted"
    );
    assert!(
        is_tool_approved(ws_a, "cargo_test").await,
        "the second granted tool reads back as approved"
    );
    // The first tool's approval is unaffected by adding a second.
    assert!(is_tool_approved(ws_a, "cargo_build").await);

    // ---- Unrelated settings survive a grant ----
    // The ledger shares a file with every other user setting, so the write path
    // must merge rather than replace. A grant that silently reset the user's
    // sandbox config would be a far worse bug than the one it fixes.
    let settings = AhmaSettings::load_from_result(&file).unwrap();
    assert_eq!(
        settings.sandbox.disable,
        AhmaSettings::default().sandbox.disable,
        "granting a tool must not disturb unrelated settings"
    );
}
