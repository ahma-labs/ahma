//! `--tmp` is a question a human answers, not a downgrade ahma takes on its
//! own (SPEC R5.2.5, R5.3).
//!
//! A server started with `--tmp` (or `[sandbox] tmp_access = true`) used to
//! append the system temp directory to every committed scope. Now, once the
//! workspace scope is committed, it asks through the existing ladder
//! (R-PERM.3: the client's `elicitation/create`, else an attached TUI, else
//! nobody) for a session-tier grant of the canonical temp directory, and the
//! directory joins the scope only on a yes. These drive that over a real
//! in-process MCP connection whose notifier is the production
//! `PermissionBroker` and whose rung 1 is the production
//! `PeerElicitationSurface`; nothing is spawned.
//!
//! The workspaces live under `CARGO_TARGET_TMPDIR`, not the system temp
//! directory: granting the temp directory to a workspace *inside* it would
//! widen the scope above the workspace, which the denylist refuses
//! (R-PERM.4.3) — the right answer there, and the wrong precondition here.

use std::path::{Path, PathBuf};

use ahma_common::config::ScopeAccess;
use ahma_common::scope_grant::GrantReason;
use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use ahma_mcp::sandbox::{Sandbox, SandboxMode};
use ahma_mcp::test_utils::concurrency::wait_for_condition;
use ahma_mcp::test_utils::in_process::create_in_process_mcp_with_broker_and_sandbox;
use ahma_mcp::test_utils::recording_client::RecordingClient;
use rmcp::model::Root;
use tempfile::TempDir;

/// A home of its own, so no test can touch the developer's real ledger
/// (SPEC R-DOCTOR.4).
fn private_home() -> TempDir {
    let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    // SAFETY: nextest runs each test in its own process; set before any
    // settings access.
    unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
    home
}

/// A workspace outside the system temp directory.
fn workspace() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let canon = dunce::canonicalize(dir.path()).unwrap();
    assert!(
        !canon.starts_with(temp()),
        "precondition: the workspace {} must not lie inside the temp dir {}",
        canon.display(),
        temp().display()
    );
    (dir, canon)
}

fn temp() -> PathBuf {
    Sandbox::canonical_temp_dir().expect("the system temp dir canonicalizes")
}

/// What `ahma serve --sandbox-scope <ws> [--tmp]` starts with: an enforcing
/// sandbox whose scope is committed before any client connects.
fn committed(scopes: Vec<PathBuf>, mode: SandboxMode, tmp_access: bool) -> Sandbox {
    let sandbox = Sandbox::new(scopes, mode, false, false, tmp_access).unwrap();
    sandbox.set_roots_received(true);
    let _ = sandbox.commit_existing_scopes();
    sandbox
}

/// Wait until the `--tmp` question has run its course.
async fn settled(sandbox: std::sync::Arc<Sandbox>) -> bool {
    wait_for_condition(
        TestTimeouts::get(TimeoutCategory::Handshake),
        TestTimeouts::poll_interval(),
        || {
            let sandbox = sandbox.clone();
            async move { sandbox.tmp_consent_settled() }
        },
    )
    .await
}

fn in_scope(sandbox: &Sandbox, path: &Path) -> bool {
    sandbox.scopes().iter().any(|s| s == path)
}

#[tokio::test]
async fn the_human_is_asked_with_the_literal_temp_path_and_a_session_yes_grants_it() {
    let _home = private_home();
    let (_ws, ws) = workspace();
    let client = RecordingClient::new("claude-code").with_elicitation(Some("read-write-session"));
    let log = client.elicitation().unwrap();
    let (mcp, _broker) = create_in_process_mcp_with_broker_and_sandbox(
        client,
        committed(vec![ws], SandboxMode::Strict, true),
        None,
    )
    .await
    .unwrap();
    let sandbox = mcp.service.adapter.sandbox_arc();

    assert!(
        settled(sandbox.clone()).await,
        "the --tmp question never settled"
    );

    let messages = log.messages();
    assert_eq!(messages.len(), 1, "asked once: {messages:#?}");
    let shown = &messages[0];
    for needle in [
        temp().display().to_string(),
        "--tmp".to_string(),
        "nothing was blocked".to_string(),
    ] {
        assert!(
            shown.contains(&needle),
            "the prompt must name {needle:?}:\n{shown}"
        );
    }
    assert!(
        sandbox.tmp_in_scope() && in_scope(&sandbox, &temp()),
        "a session yes puts the temp dir in the live scope: {:?}",
        sandbox.scopes().to_vec()
    );
    assert!(sandbox.validate_path(&temp().join("scratch.o")).is_ok());
}

#[tokio::test]
async fn a_deny_leaves_the_temp_dir_out_and_is_remembered() {
    let _home = private_home();
    let (_ws, ws) = workspace();
    let client = RecordingClient::new("claude-code").with_elicitation(Some("deny"));
    let log = client.elicitation().unwrap();
    let (mcp, broker) = create_in_process_mcp_with_broker_and_sandbox(
        client,
        committed(vec![ws], SandboxMode::Strict, true),
        None,
    )
    .await
    .unwrap();
    let sandbox = mcp.service.adapter.sandbox_arc();

    assert!(settled(sandbox.clone()).await);
    assert_eq!(log.messages().len(), 1);
    assert!(!sandbox.tmp_in_scope(), "{:?}", sandbox.scopes().to_vec());
    assert!(sandbox.validate_path(&temp().join("scratch.o")).is_err());
    assert!(
        broker
            .coordinator()
            .begin(&temp(), ScopeAccess::Rw, GrantReason::StartupFlag, None)
            .is_none(),
        "a denial is an answer: the same question is not asked again this session (R-PERM.4)"
    );
}

/// SPEC R5.3.1: a client-side `cancel` is nobody's answer. It is not recorded
/// as a denial, so a later question about the same path is still raised.
#[tokio::test]
async fn a_cancel_is_not_a_denial() {
    let _home = private_home();
    let (_ws, ws) = workspace();
    let client = RecordingClient::new("claude-code").with_elicitation_cancel();
    let log = client.elicitation().unwrap();
    let (mcp, broker) = create_in_process_mcp_with_broker_and_sandbox(
        client,
        committed(vec![ws], SandboxMode::Strict, true),
        None,
    )
    .await
    .unwrap();
    let sandbox = mcp.service.adapter.sandbox_arc();

    assert!(settled(sandbox.clone()).await);
    assert_eq!(log.messages().len(), 1, "the client was asked");
    assert!(!sandbox.tmp_in_scope(), "a cancel grants nothing");
    assert!(
        broker
            .coordinator()
            .begin(
                &temp(),
                ScopeAccess::Rw,
                GrantReason::PreExecViolation,
                None
            )
            .is_some(),
        "a cancel must leave no dismissed memo: a later question about the temp dir is asked"
    );
}

/// The machine-wide temp directory is never saved, whatever the answer says:
/// a client that sends a saved tier gets it for this session.
#[tokio::test]
async fn an_always_answer_is_held_to_the_session() {
    let home = private_home();
    let (_ws, ws) = workspace();
    let client = RecordingClient::new("claude-code").with_elicitation(Some("read-write"));
    let (mcp, _broker) = create_in_process_mcp_with_broker_and_sandbox(
        client,
        committed(vec![ws], SandboxMode::Strict, true),
        None,
    )
    .await
    .unwrap();
    let sandbox = mcp.service.adapter.sandbox_arc();

    assert!(settled(sandbox.clone()).await);
    assert!(sandbox.tmp_in_scope(), "the yes applies, for the session");
    let settings = home.path().join(".ahma").join("settings.toml");
    let written = std::fs::read_to_string(&settings).unwrap_or_default();
    assert!(
        !written.contains(&temp().display().to_string()),
        "the temp dir must never reach the settings file:\n{written}"
    );
}

/// SPEC R5.3.2: with no client that can elicit and no TUI watching, nobody
/// can be asked, so the temp directory stays out — fail closed, narrow — and
/// the scope is still committed and announced: the question never holds up
/// `notifications/sandbox/configured`.
#[tokio::test]
async fn with_nobody_to_ask_the_temp_dir_stays_out_and_the_scope_is_still_announced() {
    let _home = private_home();
    let (_ws, ws) = workspace();
    let client = RecordingClient::new("claude-code")
        .with_roots(vec![Root::new(ahma_common::file_uri::encode_file_uri(&ws))]);
    let notifications = client.notifications();
    // Uncommitted: the scope commits from the client's roots, as for an IDE.
    let sandbox = Sandbox::new(Vec::new(), SandboxMode::Strict, false, false, true).unwrap();
    let (mcp, _broker) = create_in_process_mcp_with_broker_and_sandbox(client, sandbox, None)
        .await
        .unwrap();
    let sandbox = mcp.service.adapter.sandbox_arc();

    let configured = ahma_common::mcp_methods::SANDBOX_CONFIGURED_METHOD;
    let done = wait_for_condition(
        TestTimeouts::get(TimeoutCategory::SandboxReady),
        TestTimeouts::poll_interval(),
        || {
            let sandbox = sandbox.clone();
            let notifications = notifications.clone();
            async move {
                sandbox.tmp_consent_settled()
                    && notifications.methods().iter().any(|m| m == configured)
            }
        },
    )
    .await;
    assert!(
        done,
        "expected sandbox/configured and a settled --tmp question; got {:?}, settled={}",
        notifications.methods(),
        sandbox.tmp_consent_settled()
    );
    assert!(sandbox.is_committed());
    assert!(in_scope(&sandbox, &ws), "{:?}", sandbox.scopes().to_vec());
    assert!(
        !sandbox.tmp_in_scope(),
        "nobody said yes, so the temp dir is not in scope: {:?}",
        sandbox.scopes().to_vec()
    );
}

#[tokio::test]
async fn no_question_when_the_temp_dir_is_already_in_scope() {
    let _home = private_home();
    let (_ws, ws) = workspace();
    let client = RecordingClient::new("claude-code").with_elicitation(Some("deny"));
    let log = client.elicitation().unwrap();
    let (mcp, _broker) = create_in_process_mcp_with_broker_and_sandbox(
        client,
        committed(vec![ws, temp()], SandboxMode::Strict, true),
        None,
    )
    .await
    .unwrap();
    let sandbox = mcp.service.adapter.sandbox_arc();

    assert!(mcp.service.request_tmp_consent().await.is_none());
    assert!(log.messages().is_empty(), "{:?}", log.messages());
    assert!(
        sandbox.claim_tmp_consent(),
        "nothing claimed the question: there was nothing to ask"
    );
}

#[tokio::test]
async fn no_question_in_test_mode() {
    let _home = private_home();
    let (_ws, ws) = workspace();
    let client = RecordingClient::new("claude-code").with_elicitation(Some("read-write-session"));
    let log = client.elicitation().unwrap();
    let (mcp, _broker) = create_in_process_mcp_with_broker_and_sandbox(
        client,
        committed(vec![ws], SandboxMode::Test, true),
        None,
    )
    .await
    .unwrap();
    let sandbox = mcp.service.adapter.sandbox_arc();

    assert!(
        mcp.service.request_tmp_consent().await.is_none(),
        "--no-sandbox enforces no scope, so there is nothing to widen and nothing to ask"
    );
    assert!(log.messages().is_empty(), "{:?}", log.messages());
    assert!(!in_scope(&sandbox, &temp()));
    assert!(sandbox.claim_tmp_consent(), "nothing claimed the question");
}

#[tokio::test]
async fn no_question_without_tmp() {
    let _home = private_home();
    let (_ws, ws) = workspace();
    let client = RecordingClient::new("claude-code").with_elicitation(Some("read-write-session"));
    let log = client.elicitation().unwrap();
    let (mcp, _broker) = create_in_process_mcp_with_broker_and_sandbox(
        client,
        committed(vec![ws], SandboxMode::Strict, false),
        None,
    )
    .await
    .unwrap();

    assert!(mcp.service.request_tmp_consent().await.is_none());
    assert!(log.messages().is_empty(), "{:?}", log.messages());
    assert!(!mcp.service.adapter.sandbox().tmp_in_scope());
}
