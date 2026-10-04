//! What the human is shown, and what the agent is told, when a grant question
//! goes through the *real* question ladder (SPEC R-PERM.3, R-PERM.3.4).
//!
//! These drive `sandbox_grant` over a real in-process MCP connection whose
//! scope-grant notifier is the production `PermissionBroker` and whose rung 1 is
//! the production `PeerElicitationSurface`. The client is a `RecordingClient`
//! that answers `elicitation/create` and records the message it was shown.
//!
//! REGRESSION (0.22.1): the broker implemented only the context-free notifier
//! method, so every prompt it raised read "unknown session" and "Risk: not
//! assessed", the agent was never told a session answer had been applied, and
//! the prompt budget was invisible to it. The fakes the earlier tests used were
//! handed whatever context the test chose, which is why none of them saw it.

use std::path::{Path, PathBuf};

use ahma_mcp::test_utils::in_process::create_in_process_mcp_with_broker;
use ahma_mcp::test_utils::recording_client::RecordingClient;
use rmcp::model::CallToolRequestParams;
use serde_json::json;
use tempfile::TempDir;

/// A home of its own, so no test can touch the developer's real ledger
/// (SPEC R-DOCTOR.4).
fn private_home() -> TempDir {
    let home = TempDir::new().unwrap();
    // SAFETY: nextest runs each test in its own process; set before any
    // settings access.
    unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
    home
}

fn canonical_dir(parent: &Path, name: &str) -> PathBuf {
    let p = parent.join(name);
    std::fs::create_dir_all(&p).unwrap();
    dunce::canonicalize(&p).unwrap()
}

async fn request_grant(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, RecordingClient>,
    path: &Path,
    access: &str,
    reason: &str,
) -> String {
    let args = json!({
        "path": path.to_string_lossy(),
        "access": access,
        "confirm": true,
        "reason": reason,
    });
    let result = client
        .call_tool(
            CallToolRequestParams::new("sandbox_grant")
                .with_arguments(args.as_object().unwrap().clone()),
        )
        .await
        .expect("sandbox_grant answers");
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<String>()
}

#[tokio::test]
async fn the_human_is_told_who_asked_what_for_and_how_risky_it_is() {
    let _home = private_home();
    let workspace = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let target = canonical_dir(outside.path(), "sccache");

    let client = RecordingClient::new("claude-code").with_elicitation(Some("deny"));
    let log = client.elicitation().unwrap();
    let (mcp, _broker) = create_in_process_mcp_with_broker(client, workspace.path(), None)
        .await
        .unwrap();

    request_grant(
        &mcp.client,
        &target,
        "rw",
        "sccache keeps its object cache there",
    )
    .await;

    let messages = log.messages();
    assert_eq!(messages.len(), 1, "one question, asked once: {messages:#?}");
    let shown = &messages[0];
    let workspace_canon = dunce::canonicalize(workspace.path()).unwrap();
    for needle in [
        "claude-code",
        &workspace_canon.display().to_string(),
        "sandbox_grant",
        "sccache keeps its object cache there",
        &target.display().to_string(),
        "NORMAL",
    ] {
        assert!(
            shown.contains(needle),
            "the prompt must name {needle:?}:\n{shown}"
        );
    }
    for absent in ["unknown session", "not assessed"] {
        assert!(
            !shown.contains(absent),
            "the production prompt must not fall back to {absent:?}:\n{shown}"
        );
    }
    assert!(
        !shown.contains("[n]") && !shown.contains("[Y]"),
        "TUI key letters mean nothing inside a form; the form carries the choices:\n{shown}"
    );
}

#[tokio::test]
async fn a_session_answer_is_applied_and_the_agent_is_told_so() {
    let home = private_home();
    let workspace = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let target = canonical_dir(outside.path(), "cache");

    let client = RecordingClient::new("claude-code").with_elicitation(Some("read-write-session"));
    let (mcp, _broker) = create_in_process_mcp_with_broker(client, workspace.path(), None)
        .await
        .unwrap();

    let text = request_grant(
        &mcp.client,
        &target,
        "rw",
        "the build writes its cache there",
    )
    .await;

    assert!(
        mcp.service.adapter.sandbox().is_path_in_scope(&target),
        "a session answer widens the live session (R-PERM.4.1)"
    );
    assert!(text.contains("A human approved"), "{text}");
    assert!(
        text.contains("this session only"),
        "the agent is told the tier it got: {text}"
    );
    let settings = home.path().join(".ahma").join("settings.toml");
    assert!(
        !settings.exists()
            || !std::fs::read_to_string(&settings)
                .unwrap()
                .contains("cache"),
        "a session answer is never written to disk"
    );
}

/// SPEC R-PERM.2.3 at the prompt: "for 24 hours" is saved as a lease bound to
/// the workspace, applied now, and the agent is told when it ends.
#[tokio::test]
async fn a_lease_answer_is_saved_with_its_end_and_applied() {
    let home = private_home();
    let workspace = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let target = canonical_dir(outside.path(), "task-cache");

    let client = RecordingClient::new("claude-code").with_elicitation(Some("read-write-24h"));
    let (mcp, _broker) = create_in_process_mcp_with_broker(client, workspace.path(), None)
        .await
        .unwrap();
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let text = request_grant(
        &mcp.client,
        &target,
        "rw",
        "the task writes its cache there",
    )
    .await;

    assert!(mcp.service.adapter.sandbox().is_path_in_scope(&target));
    assert!(text.contains("A human approved"), "{text}");
    assert!(
        text.contains("24 hours"),
        "the agent is told it ends: {text}"
    );
    let saved = ahma_common::config::AhmaSettings::load_from_result(
        &home.path().join(".ahma").join("settings.toml"),
    )
    .unwrap();
    let rec = saved
        .sandbox
        .persistent_scopes
        .iter()
        .find(|r| r.path == target)
        .expect("a lease is saved");
    let at = rec.expires_at.expect("with its end");
    assert!((before + 86_400..=before + 86_460).contains(&at), "{at}");
}

#[tokio::test]
async fn a_decline_is_reported_as_the_humans_answer() {
    let _home = private_home();
    let workspace = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let target = canonical_dir(outside.path(), "elsewhere");

    let client = RecordingClient::new("claude-code").with_elicitation(Some("deny"));
    let (mcp, _broker) = create_in_process_mcp_with_broker(client, workspace.path(), None)
        .await
        .unwrap();

    let text = request_grant(&mcp.client, &target, "ro", "reading a sibling project").await;

    assert!(
        text.contains("A human declined"),
        "the agent learns the answer, not a guess at why nothing happened: {text}"
    );
    assert!(
        !text.contains("Blocked until a human answers"),
        "the human already answered: {text}"
    );
    assert!(!mcp.service.adapter.sandbox().is_path_in_scope(&target));
}

#[tokio::test]
async fn a_spent_prompt_budget_is_reported_to_the_agent() {
    let _home = private_home();
    let workspace = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();

    let client = RecordingClient::new("claude-code").with_elicitation(Some("deny"));
    let log = client.elicitation().unwrap();
    let (mcp, _broker) = create_in_process_mcp_with_broker(client, workspace.path(), None)
        .await
        .unwrap();

    let budget = ahma_common::scope_grant::PROMPT_BUDGET;
    for i in 0..budget {
        let target = canonical_dir(outside.path(), &format!("dir{i}"));
        request_grant(&mcp.client, &target, "ro", "test").await;
    }
    let over = canonical_dir(outside.path(), "one-too-many");
    let text = request_grant(&mcp.client, &over, "ro", "test").await;

    assert_eq!(
        log.messages().len(),
        budget,
        "past the budget nothing more is put to the human (R-PERM.4.5)"
    );
    assert!(
        text.contains("prompt budget"),
        "the agent is told why, so it asks in conversation instead: {text}"
    );
}

/// A path already allowed is not asked about again (SPEC R-PERM.4): the agent
/// is told it already has the access. It was told "Not raised … Nothing is
/// granted" after the human had granted it, and asked again.
#[tokio::test]
async fn a_path_already_allowed_is_reported_as_allowed_not_asked_again() {
    let _home = private_home();
    let workspace = TempDir::new().unwrap();
    let inside = canonical_dir(workspace.path(), "build");

    let client = RecordingClient::new("claude-code").with_elicitation(Some("deny"));
    let log = client.elicitation().unwrap();
    let (mcp, _broker) = create_in_process_mcp_with_broker(client, workspace.path(), None)
        .await
        .unwrap();

    let text = request_grant(&mcp.client, &inside, "rw", "the build writes here").await;
    assert!(
        log.messages().is_empty(),
        "nothing to ask: {:#?}",
        log.messages()
    );
    assert!(text.starts_with("Already allowed"), "{text}");
    assert!(!text.contains("Nothing is granted"), "{text}");
}
