//! The system temp directory is in ahma's own scope by default, after the
//! workspace, with no prompt (SPEC R5.2.5).
//!
//! Commands may already write temp at the kernel layer; `[sandbox] tmp_access`
//! (on by default) lets ahma's own tools — `working_directory`, `format: path`
//! arguments, the built-in file tools — follow them there. A server used to ask
//! a human once per session before the temp directory joined the scope, which
//! made the most ordinary scratch space a question nobody wanted. Now every
//! commit admits it, behind the same rules as before: never the primary scope,
//! never with no workspace, never when the hard denylist refuses it for this
//! workspace (R-PERM.4.3). `[sandbox] disable_temp` is the expert opt-out and
//! keeps it out of the scope as well as the kernel rule.
//!
//! These drive a real in-process MCP connection whose notifier is the
//! production `PermissionBroker` and whose rung 1 is the production
//! `PeerElicitationSurface`; nothing is spawned. The clients would answer a
//! question if one were asked, so "nothing was asked" is observable.
//!
//! The workspaces live under `CARGO_TARGET_TMPDIR`, not the system temp
//! directory: the temp directory is refused for a workspace *inside* it, which
//! one test asserts and the rest must not trip over.

use std::path::{Path, PathBuf};

use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use ahma_mcp::sandbox::{Sandbox, SandboxMode, SandboxOptions};
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

fn options(tmp_access: bool, disable_temp: bool) -> SandboxOptions {
    SandboxOptions {
        tmp_access,
        no_temp_files: disable_temp,
        ..SandboxOptions::default()
    }
}

/// What `ahma serve --sandbox-scope <ws>` starts with: a sandbox whose scope
/// is committed before any client connects.
fn committed(scopes: Vec<PathBuf>, mode: SandboxMode, options: SandboxOptions) -> Sandbox {
    let sandbox = Sandbox::new(scopes, mode, options).unwrap();
    sandbox.mark_roots_received();
    let _ = sandbox.commit_existing_scopes();
    sandbox
}

/// Whether a scope covers `path`: the temp directory may be in scope through a
/// kernel temp root that contains it (`/private/var/folders` on macOS).
fn in_scope(sandbox: &Sandbox, path: &Path) -> bool {
    sandbox.scopes().iter().any(|s| path.starts_with(s))
}

/// A client that would grant anything it was asked, so a question shows up
/// in its log.
fn eager_client() -> RecordingClient {
    RecordingClient::new("claude-code").with_elicitation(Some("read-write-session"))
}

#[tokio::test]
async fn the_temp_dir_joins_an_explicit_scope_without_a_prompt() {
    let _home = private_home();
    let (_ws, ws) = workspace();
    let client = eager_client();
    let log = client.elicitation().unwrap();
    let (mcp, _broker) = create_in_process_mcp_with_broker_and_sandbox(
        client,
        committed(vec![ws.clone()], SandboxMode::Strict, options(true, false)),
        None,
    )
    .await
    .unwrap();
    let sandbox = mcp.service.adapter.sandbox_arc();

    assert!(
        sandbox.tmp_in_scope() && in_scope(&sandbox, &temp()),
        "the commit admits the temp dir: {:?}",
        sandbox.scopes().to_vec()
    );
    assert_eq!(
        sandbox.scopes().first(),
        Some(&ws),
        "the workspace stays the primary scope; temp is auxiliary"
    );
    assert!(sandbox.validate_path(&temp().join("scratch.o")).is_ok());
    assert!(
        log.messages().is_empty(),
        "nobody is asked: {:?}",
        log.messages()
    );
}

/// The roots path: the temp dir is part of the commit itself, so it is in
/// scope by the time `notifications/sandbox/configured` reaches the client.
#[tokio::test]
async fn the_temp_dir_is_in_a_roots_scope_when_it_is_announced() {
    let _home = private_home();
    let (_ws, ws) = workspace();
    let client =
        eager_client().with_roots(vec![Root::new(ahma_common::file_uri::encode_file_uri(&ws))]);
    let notifications = client.notifications();
    let log = client.elicitation().unwrap();
    // Uncommitted: the scope commits from the client's roots, as for an IDE.
    let sandbox = Sandbox::new(Vec::new(), SandboxMode::Strict, options(true, false)).unwrap();
    let (mcp, _broker) = create_in_process_mcp_with_broker_and_sandbox(client, sandbox, None)
        .await
        .unwrap();
    let sandbox = mcp.service.adapter.sandbox_arc();

    let configured = ahma_common::mcp_methods::SANDBOX_CONFIGURED_METHOD;
    let announced = wait_for_condition(
        TestTimeouts::get(TimeoutCategory::SandboxReady),
        TestTimeouts::poll_interval(),
        || {
            let notifications = notifications.clone();
            async move { notifications.methods().iter().any(|m| m == configured) }
        },
    )
    .await;
    assert!(
        announced,
        "no sandbox/configured: {:?}",
        notifications.methods()
    );
    assert!(sandbox.is_committed());
    assert!(in_scope(&sandbox, &ws), "{:?}", sandbox.scopes().to_vec());
    assert!(
        sandbox.tmp_in_scope(),
        "the temp dir is in the announced scope: {:?}",
        sandbox.scopes().to_vec()
    );
    assert!(
        log.messages().is_empty(),
        "nobody is asked: {:?}",
        log.messages()
    );
}

/// SPEC R5.2.5: `disable_temp` is the expert opt-out. It wins over
/// `tmp_access`, so temp stays out of ahma's scope as well as the kernel rule.
#[tokio::test]
async fn disable_temp_keeps_the_temp_dir_out_of_the_scope() {
    let _home = private_home();
    let (_ws, ws) = workspace();
    let client = eager_client();
    let log = client.elicitation().unwrap();
    let (mcp, _broker) = create_in_process_mcp_with_broker_and_sandbox(
        client,
        committed(vec![ws], SandboxMode::Strict, options(true, true)),
        None,
    )
    .await
    .unwrap();
    let sandbox = mcp.service.adapter.sandbox_arc();

    assert!(!sandbox.tmp_in_scope(), "{:?}", sandbox.scopes().to_vec());
    assert!(
        !sandbox.is_tmp_access(),
        "disable_temp overrides tmp_access"
    );
    assert!(sandbox.validate_path(&temp().join("scratch.o")).is_err());
    assert!(log.messages().is_empty(), "{:?}", log.messages());
}

/// `tmp_access = false` keeps temp out of ahma's own scope (commands still
/// get the kernel temp rule; that is `disable_temp`'s to close).
#[tokio::test]
async fn tmp_access_false_keeps_the_temp_dir_out_of_the_scope() {
    let _home = private_home();
    let (_ws, ws) = workspace();
    let client = eager_client();
    let log = client.elicitation().unwrap();
    let (mcp, _broker) = create_in_process_mcp_with_broker_and_sandbox(
        client,
        committed(vec![ws], SandboxMode::Strict, options(false, false)),
        None,
    )
    .await
    .unwrap();

    assert!(!mcp.service.adapter.sandbox().tmp_in_scope());
    assert!(log.messages().is_empty(), "{:?}", log.messages());
}

/// SPEC R5.2.4, R-PERM.4.3: a workspace inside the temp directory does not
/// widen to the whole of it — the denylist refuses a parent of the live
/// scope — and temp is never the scope on its own.
#[tokio::test]
async fn a_workspace_inside_temp_does_not_widen_to_it() {
    let _home = private_home();
    let inside = tempfile::tempdir().unwrap();
    let ws = dunce::canonicalize(inside.path()).unwrap();
    if !ws.starts_with(temp()) {
        // TMPDIR points somewhere `std::env::temp_dir()` does not; nothing to test.
        return;
    }
    let client = eager_client();
    let log = client.elicitation().unwrap();
    let (mcp, _broker) = create_in_process_mcp_with_broker_and_sandbox(
        client,
        committed(vec![ws.clone()], SandboxMode::Strict, options(true, false)),
        None,
    )
    .await
    .unwrap();
    let sandbox = mcp.service.adapter.sandbox_arc();

    assert!(
        !in_scope(&sandbox, &temp()),
        "the scope must not widen above the workspace: {:?}",
        sandbox.scopes().to_vec()
    );
    assert!(log.messages().is_empty(), "{:?}", log.messages());

    // And with no workspace at all, temp is not seeded as the sole scope.
    let empty = committed(Vec::new(), SandboxMode::Strict, options(true, false));
    assert!(empty.scopes().is_empty(), "{:?}", empty.scopes().to_vec());
}
