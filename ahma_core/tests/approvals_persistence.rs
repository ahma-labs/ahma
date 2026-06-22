//! Integration coverage for the persistent tool-approval store
//! (`ahma_core::approvals`).
//!
//! The module's in-crate unit tests already exercise the *pure* helpers
//! (`classify`, `parse_grants`, `reask_note_for`). What was previously
//! uncovered — and what this file pins — is the **on-disk persistence
//! round-trip** that actually gates whether a sandboxed agent re-prompts for a
//! tool: `remember_tool_approval` → `is_tool_approved` / `grant_status` /
//! `reask_note`, reading and writing a real `approvals.json`.
//!
//! These are security-relevant invariants. A silent break here is exactly the
//! kind that gets a change reverted:
//!   * **fail closed** — a missing/unreadable file must read as "not approved"
//!     so the caller prompts rather than silently allowing a tool;
//!   * **workspace scoping** — a grant in one workspace must NOT approve the
//!     same tool in a different workspace (it only triggers a re-ask);
//!   * **round-trip durability** — what the (sync) TUI writer persists must be
//!     what the (async) agent reader observes;
//!   * **idempotency** — re-granting must not duplicate entries.
//!
//! `AHMA_CONFIG_DIR` is the documented test seam (`approvals::config_dir`
//! honors it). Because that env var is process-global, the whole round-trip is
//! pinned in a SINGLE test so there is exactly one writer of the env in this
//! binary — correct under both `cargo nextest` (process per test) and
//! `cargo test` (threads sharing one process).

use std::path::{Path, PathBuf};

use ahma_core::approvals::{
    GrantStatus, grant_status, is_tool_approved, reask_note, remember_tool_approval,
};
use tempfile::TempDir;

/// Path of the persisted `approvals.json` given the `AHMA_CONFIG_DIR` root.
/// Mirrors `approvals::config_dir`, which joins `ahma/` onto the env root.
fn approvals_file(config_root: &Path) -> PathBuf {
    config_root.join("ahma").join("approvals.json")
}

/// Tools recorded under `key` in the on-disk grants map (empty if absent).
fn grants_for(file: &Path, key: &Path) -> Vec<String> {
    let content = std::fs::read_to_string(file).expect("approvals.json should exist after a grant");
    let map: std::collections::BTreeMap<String, Vec<String>> =
        serde_json::from_str(&content).expect("approvals.json should be valid JSON");
    // Writer canonicalises the key, so look it up canonically too.
    let canonical = std::fs::canonicalize(key).unwrap_or_else(|_| key.to_path_buf());
    map.get(&canonical.to_string_lossy().into_owned())
        .cloned()
        .unwrap_or_default()
}

#[tokio::test]
async fn approval_grants_persist_and_are_workspace_scoped() {
    // Fresh, empty config dir — no approvals.json exists yet.
    let config = TempDir::new().unwrap();
    // SAFETY: set once, before any approvals call; single-test binary so no
    // other thread/test races on this process-global env var.
    unsafe { std::env::set_var("AHMA_CONFIG_DIR", config.path()) };
    let file = approvals_file(config.path());

    // Two distinct, real workspaces so canonicalisation yields stable, different keys.
    let ws_a_dir = TempDir::new().unwrap();
    let ws_b_dir = TempDir::new().unwrap();
    let ws_a = ws_a_dir.path();
    let ws_b = ws_b_dir.path();

    // ---- Fail closed: nothing granted, no file on disk ----
    assert!(
        !is_tool_approved(ws_a, "cargo_build").await,
        "an ungranted tool with no approvals file must read as NOT approved (fail closed)"
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
        "merely reading approvals must not create the file"
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
}
