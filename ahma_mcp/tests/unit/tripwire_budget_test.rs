//! The tripwire budget: one scripted session, counted.
//!
//! Every rule this file touches already has a test of its own — edits wait for
//! rewriters (R2.7.8), a repeated refusal is one line (R2.7.8), a path is asked
//! about once per session (R-PERM.4). What none of them can see is the *sum*:
//! a session in which a build runs, the agent keeps editing, and a command
//! wanders out of scope, and the human and the agent end up told the same thing
//! several times over by different surfaces. This test drives that session and
//! asserts the totals, so a change that adds a notification anywhere on the path
//! — a second prompt for a denial that trips twice, a full refusal repeated on
//! every edit, a refusal for an edit that cannot race the writer — fails here
//! even when each surface's own test stays green.
//!
//! The script:
//!
//! 1. A source *reader* (`sleep …; cargo test`) holds the workspace lease. Five
//!    native edits through the harness edit guard (`hooks::edit_guard::decide`)
//!    are all let through: builds and tests do not block edits (R2.7.8).
//! 2. An unrecognised command run in the `gen/` subtree holds the lease: a
//!    possible rewriter bounded to its footprint. The same five-edit shape: the
//!    two outside `gen/` are let through, the three inside are refused — one full
//!    refusal, then one-line repeats (R2.7.8 "The full refusal is given once per
//!    running command"). Once it is gone, an edit inside `gen/` goes through.
//! 3. A command whose working directory lies outside the scope is refused three
//!    times by path validation, and the same directory is then reported twice
//!    more by the kernel-denial (stderr) detector. The human is asked exactly once
//!    (R-PERM.4: at most once per `(path, access)` per session).
//!
//! Steps 1–2 run on a queue-enabled server (`SandboxMode::Test`, so commands
//! really run); step 3 on a broker-wired server over a `Strict` sandbox, so path
//! validation really refuses — nothing is spawned there. Both are scoped to the
//! same workspace directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ahma_common::config::{ExecutionPolicy, ScopeAccess};
use ahma_common::scope_grant::{GrantContext, GrantReason};
use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use ahma_mcp::adapter::workspace_queue::{HolderInfo, LeaseProbe, SourceEffect, WorkspaceQueue};
use ahma_mcp::hooks::HookPlatform;
use ahma_mcp::hooks::edit_guard::decide;
use ahma_mcp::sandbox::grant_channel::notify_stderr_denial;
use ahma_mcp::sandbox::{Sandbox, SandboxMode, ScopeGrantNotifier};
use ahma_mcp::shell::cli::AppConfig;
use ahma_mcp::test_utils::concurrency::wait_for_condition;
use ahma_mcp::test_utils::in_process::{
    InProcessMcp, create_in_process_mcp_with_broker_and_sandbox,
    create_in_process_mcp_with_workspace_queue,
};
use ahma_mcp::test_utils::recording_client::RecordingClient;
use ahma_mcp::utils::logging::init_test_logging;
use anyhow::Result;
use rmcp::ClientHandler;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};
use tempfile::TempDir;

// ─── What the session is allowed to cost ─────────────────────────────────────

/// Everything the session told anyone, by kind.
#[derive(Debug, Default, PartialEq, Eq)]
struct Tally {
    /// Questions put to the human (`elicitation/create` messages).
    human_questions: usize,
    /// Full edit refusals (the multi-sentence R2.7.8 explanation).
    full_edit_refusals: usize,
    /// One-line edit refusals (repeats while the same command runs).
    short_edit_refusals: usize,
    /// Native edits the guard took no position on.
    edits_let_through: usize,
}

/// The budget: 5 + 2 + 1 edits through; one full refusal and two repeats for
/// the three in-footprint edits; one question for one out-of-scope directory
/// however many times it trips.
const BUDGET: Tally = Tally {
    human_questions: 1,
    full_edit_refusals: 1,
    short_edit_refusals: 2,
    edits_let_through: 8,
};

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// A home of its own, so nothing touches the developer's real ledger or audit
/// log (SPEC R-DOCTOR.4). Same pattern as `permission_prompt_context_test`.
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

fn result_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The operation id in an `AHMA ID: <id>` answer.
fn op_id(text: &str) -> String {
    text.split("AHMA ID: ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("no operation id in: {text}"))
        .to_string()
}

/// How long the background commands would run if nobody cancelled them: far
/// longer than the edit phase, on every platform. They are always cancelled.
fn hold_secs() -> u64 {
    TestTimeouts::scale_secs(30).as_secs()
}

/// A source reader: classified `ReadsSources` from its title (`sleep` and
/// `cargo test` both read), and long-running. `sleep N` is also a PowerShell
/// alias of `Start-Sleep` on Windows, so one spelling serves every platform.
fn reader_cmd() -> String {
    format!("sleep {}; cargo test", hold_secs())
}

/// A possible rewriter: `touch` classifies as `RewritesSources`, PowerShell's
/// `New-Item` as `Unknown`. Either blocks an edit, but only inside the subtree
/// the command runs in (its footprint).
fn writer_cmd() -> String {
    let secs = hold_secs();
    if cfg!(windows) {
        format!("Start-Sleep -Seconds {secs}; New-Item -ItemType File -Force writer-done.txt")
    } else {
        format!("sleep {secs}; touch writer-done.txt")
    }
}

async fn call<C: ClientHandler>(
    mcp: &InProcessMcp<C>,
    tool: &str,
    args: Value,
) -> Result<CallToolResult, rmcp::ServiceError> {
    let params = CallToolRequestParams::new(tool.to_string())
        .with_arguments(args.as_object().cloned().unwrap_or_default());
    tokio::time::timeout(
        TestTimeouts::get(TimeoutCategory::ToolCall),
        mcp.client.call_tool(params),
    )
    .await
    .unwrap_or_else(|_| panic!("{tool} must answer"))
}

/// Start `command` in `wd` and return its operation id (async mode, so a
/// long-running command comes back as an id).
async fn start(mcp: &InProcessMcp, command: &str, wd: &Path) -> String {
    let r = call(
        mcp,
        "run_terminal_command",
        json!({ "command": command, "working_directory": wd.to_string_lossy() }),
    )
    .await
    .expect("run_terminal_command answers");
    op_id(&result_text(&r))
}

/// Cancel `id` and wait until the workspace lease is free again.
async fn stop(mcp: &InProcessMcp, queue: &WorkspaceQueue, ws: &Path, id: &str) {
    let _ = call(mcp, "cancel", json!({ "id": id })).await;
    assert!(
        wait_for_lease(queue, ws, None).await,
        "the lease must be released once {id} is cancelled"
    );
}

/// Wait until `ws`'s lease is held by `holder` (`Some(op_id)`) or free (`None`).
async fn wait_for_lease(queue: &WorkspaceQueue, ws: &Path, holder: Option<&str>) -> bool {
    let want = holder.map(str::to_string);
    wait_for_condition(
        TestTimeouts::get(TimeoutCategory::ToolCall),
        TestTimeouts::poll_interval(),
        || {
            let probe = queue.probe(ws);
            let want = want.clone();
            async move {
                match (probe, want) {
                    (LeaseProbe::Free, None) => true,
                    (LeaseProbe::Held { holder: Some(h) }, Some(id)) => h.op_id == id,
                    _ => false,
                }
            }
        },
    )
    .await
}

/// The holder of `ws`'s lease right now (it must be held).
fn holder_of(queue: &WorkspaceQueue, ws: &Path) -> HolderInfo {
    match queue.probe(ws) {
        LeaseProbe::Held { holder: Some(h) } => h,
        other => panic!(
            "expected a published holder for {}, got {other:?}",
            ws.display()
        ),
    }
}

/// A Claude Code `Write` payload, as the harness hands it to the edit guard.
fn native_write(cwd: &Path, file: &Path) -> Value {
    json!({
        "tool_name": "Write",
        "cwd": cwd.to_string_lossy(),
        "tool_input": {"file_path": file.to_string_lossy(), "content": "x"}
    })
}

/// Run one native edit through the guard; `Some(reason)` when it is refused.
fn native_edit(queue: &WorkspaceQueue, ws: &Path, file: &Path) -> Option<String> {
    let allowed = vec![ws.to_path_buf()];
    let out = decide(
        Some(HookPlatform::Claude),
        &native_write(ws, file),
        queue,
        Some(&allowed),
    )?;
    assert_eq!(
        out["hookSpecificOutput"]["permissionDecision"], "deny",
        "the guard only ever denies or says nothing: {out}"
    );
    Some(
        out["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .expect("a deny carries its reason")
            .to_string(),
    )
}

/// The full R2.7.8 refusal (`workspace_queue::edit_refusal`) explains the rule;
/// the repeat (`edit_refusal_again`) is one line that only names the holder.
fn is_full_refusal(reason: &str) -> bool {
    reason.contains("may rewrite source files")
}

fn is_short_refusal(reason: &str) -> bool {
    !reason.contains('\n') && reason.contains("is still running") && !is_full_refusal(reason)
}

// ─── The session ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_session_with_a_build_edits_and_an_escape_stays_within_its_tripwire_budget() -> Result<()>
{
    init_test_logging();
    let _home = private_home();
    let mut tally = Tally::default();

    // The workspace: a repository (so the server and the hook key the lease on
    // the same directory, R2.7.2) with a source tree and a generated subtree.
    let temp = tempfile::tempdir()?;
    let ws = canonical_dir(temp.path(), "ws");
    std::fs::create_dir_all(ws.join(".git"))?;
    let src = canonical_dir(&ws, "src");
    let docs = canonical_dir(&ws, "docs");
    let gen_dir = canonical_dir(&ws, "gen");
    let locks = temp.path().join("locks");

    // The five edits of each phase: two outside `gen/`, three inside it (one
    // file edited twice, as an agent retrying would).
    let five_edits = [
        src.join("lib.rs"),
        gen_dir.join("schema.rs"),
        docs.join("notes.md"),
        gen_dir.join("schema.rs"),
        gen_dir.join("types.rs"),
    ];

    let config = AppConfig {
        execution_mode: ExecutionPolicy::Async,
        request_budget_override_secs: Some(2),
        ..AppConfig::default()
    };
    let mcp = create_in_process_mcp_with_workspace_queue(&ws, locks.clone(), config).await?;
    // The hook's view of the queue: same rendezvous directory as the server.
    let hook_queue = WorkspaceQueue::with_lock_dir(true, Some(locks.clone()));

    // ── 1. A source reader holds the workspace: nothing is refused ──────────
    let reader = start(&mcp, &reader_cmd(), &ws).await;
    assert!(
        wait_for_lease(&hook_queue, &ws, Some(&reader)).await,
        "the reader {reader} must hold the workspace lease"
    );
    assert_eq!(
        holder_of(&hook_queue, &ws).effect,
        SourceEffect::ReadsSources,
        "precondition: `{}` is classified as a source reader",
        reader_cmd()
    );
    for file in &five_edits {
        let refused = native_edit(&hook_queue, &ws, file);
        assert!(
            refused.is_none(),
            "a build or test never blocks an edit (R2.7.8); {} was refused: {refused:?}",
            file.display()
        );
        tally.edits_let_through += 1;
    }
    stop(&mcp, &hook_queue, &ws, &reader).await;

    // ── 2. A possible rewriter in `gen/`: refused inside it, once in full ───
    let writer = start(&mcp, &writer_cmd(), &gen_dir).await;
    assert!(
        wait_for_lease(&hook_queue, &ws, Some(&writer)).await,
        "the writer {writer} must hold the workspace lease"
    );
    let holder = holder_of(&hook_queue, &ws);
    assert_ne!(
        holder.effect,
        SourceEffect::ReadsSources,
        "precondition: `{}` is a possible rewriter, not a reader",
        writer_cmd()
    );
    assert_eq!(
        holder.footprint.as_deref(),
        Some(gen_dir.as_path()),
        "precondition: a command run in a subtree is bound to it (R2.7.8)"
    );

    let mut refusals: Vec<(PathBuf, String)> = Vec::new();
    for file in &five_edits {
        match native_edit(&hook_queue, &ws, file) {
            None => {
                assert!(
                    !file.starts_with(&gen_dir),
                    "{} is inside the writer's footprint and must be refused",
                    file.display()
                );
                tally.edits_let_through += 1;
            }
            Some(reason) => {
                assert!(
                    file.starts_with(&gen_dir),
                    "{} is outside the writer's footprint ({}) and must not be refused: {reason}",
                    file.display(),
                    gen_dir.display()
                );
                assert!(
                    reason.contains(&writer),
                    "every refusal names the writer ({writer}): {reason}"
                );
                refusals.push((file.clone(), reason));
            }
        }
    }
    let full: Vec<&str> = refusals
        .iter()
        .map(|(_, r)| r.as_str())
        .filter(|r| is_full_refusal(r))
        .collect();
    let short: Vec<&str> = refusals
        .iter()
        .map(|(_, r)| r.as_str())
        .filter(|r| is_short_refusal(r))
        .collect();
    assert_eq!(
        full.len() + short.len(),
        refusals.len(),
        "every refusal is either the full one or a one-line repeat: {refusals:#?}"
    );
    assert!(
        refusals.first().is_some_and(|(_, r)| is_full_refusal(r)),
        "the first refusal for a running command is the full one: {refusals:#?}"
    );
    tally.full_edit_refusals += full.len();
    tally.short_edit_refusals += short.len();

    stop(&mcp, &hook_queue, &ws, &writer).await;
    assert!(
        native_edit(&hook_queue, &ws, &gen_dir.join("schema.rs")).is_none(),
        "once the writer is gone, the same edit goes through"
    );
    tally.edits_let_through += 1;
    let _ = call(&mcp, "cancel", json!({ "all": true })).await;
    let _ = mcp.client.cancel().await;

    // ── 3. An escape: the same out-of-scope directory trips five times ──────
    let outside = TempDir::new()?;
    let cache = canonical_dir(outside.path(), "cache");

    // Strict, so `validate_path` really refuses (Test mode skips it). Nothing
    // is spawned on this server: every call is refused before it runs.
    let sandbox = Sandbox::new(vec![ws.clone()], SandboxMode::Strict, false, false, false)?;
    sandbox.set_roots_received(true);
    let _ = sandbox.commit_existing_scopes();
    let client = RecordingClient::new("claude-code").with_elicitation(Some("deny"));
    let questions = client.elicitation().expect("elicitation is enabled");
    let (guarded, broker) =
        create_in_process_mcp_with_broker_and_sandbox(client, sandbox, None).await?;

    // Three refusals up front (path validation → `notify_pre_exec`).
    for attempt in 1..=3 {
        let outcome = call(
            &guarded,
            "run_terminal_command",
            json!({
                "command": "touch out.o",
                "working_directory": cache.to_string_lossy(),
            }),
        )
        .await;
        assert!(
            outcome.is_err(),
            "attempt {attempt}: a working directory outside the scope must be refused"
        );
        assert!(
            !guarded.service.adapter.sandbox().is_path_in_scope(&cache),
            "attempt {attempt}: a declined path stays out of scope"
        );
    }

    // Two more trips of the same directory by the kernel-denial detector — the
    // helper the adapter calls on a sandboxed command's stderr.
    let notifier: Arc<dyn ScopeGrantNotifier> = broker.clone();
    let stderr = format!(
        "error: failed to create directory `{}`: Read-only file system",
        cache.display()
    );
    for _ in 0..2 {
        notify_stderr_denial(
            guarded.service.adapter.sandbox(),
            Some(&notifier),
            &stderr,
            "",
            "run_terminal_command",
        )
        .await;
    }
    // And straight at the broker, so the dedup is proven even if the stderr
    // scanner's path extraction ever changes.
    let again = notifier
        .notify_violation_with(
            &cache,
            ScopeAccess::Rw,
            GrantReason::StderrHeuristic,
            Some("run_terminal_command".to_string()),
            GrantContext::default(),
        )
        .await;
    assert!(
        again.is_none(),
        "an answered (path, access) raises no new request this session (R-PERM.4): {again:?}"
    );

    let asked = questions.messages();
    assert!(
        asked
            .first()
            .is_some_and(|m| m.contains(&cache.display().to_string())),
        "the one question names the directory: {asked:#?}"
    );
    tally.human_questions += asked.len();
    let _ = guarded.client.cancel().await;

    // ── The budget ──────────────────────────────────────────────────────────
    assert_eq!(
        tally, BUDGET,
        "the session told the human and the agent more (or less) than it should have. \
         Human questions: {asked:#?}\nEdit refusals: {refusals:#?}"
    );
    Ok(())
}
