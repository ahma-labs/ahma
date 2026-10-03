//! Trust-handoff deny-tier detection (SPEC R6.1.7, R-HANDOFF.4, R-HANDOFF.10),
//! in-process.
//!
//! On Linux and Windows the kernel does not stop a shell command from writing
//! `.git/hooks/*` or the project's `.ahma/`. What ahma owes the user there is to
//! say so, loudly and durably, when it happens: the alert leads the tool result
//! and every changed entry is a `handoff_write` audit record. These tests force
//! the watch on with [`HandoffWatchMode::Always`], so they assert the same
//! behaviour on macOS, where production skips it because Seatbelt already
//! refuses the write.
//!
//! The audit log is process-wide and resolves once; each test filters it by its
//! own operation id, as `exec_audit_test` does.

use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use ahma_mcp::AhmaMcpService;
use ahma_mcp::adapter::{Adapter, AsyncExecOptions, ExecutionMode};
use ahma_mcp::operation_monitor::{MonitorConfig, Operation, OperationMonitor};
use ahma_mcp::sandbox::handoff_watch::{
    ALERT_PREFIX, HandoffChangeKind, HandoffWatch, HandoffWatchMode, INCOMPLETE_PREFIX,
};
use ahma_mcp::sandbox::{Sandbox, SandboxMode};
use ahma_mcp::shell_pool::{ShellPoolConfig, ShellPoolManager, platform_shell_program};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tempfile::TempDir;

// ─────────────────────────────────────────────────────────────────────────────
// Harness
// ─────────────────────────────────────────────────────────────────────────────

static AUDIT_DIR: OnceLock<TempDir> = OnceLock::new();

/// Redirect the process-wide audit log into a tempdir (first caller wins) and
/// return the path actually in effect.
fn audit_log_path() -> PathBuf {
    let dir = AUDIT_DIR.get_or_init(|| tempfile::tempdir().expect("audit tempdir"));
    let _ = ahma_mcp::adapter::audit::set_audit_log_path(dir.path().join("audit.jsonl"));
    ahma_mcp::adapter::audit::audit_log().path().to_path_buf()
}

/// `handoff_write` records for one operation, in file order.
async fn handoff_records(op_id: &str) -> Vec<Value> {
    let contents = tokio::fs::read_to_string(audit_log_path())
        .await
        .unwrap_or_default();
    contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).expect("complete JSON line"))
        .filter(|e| e["type"] == "handoff_write" && e["operation_id"] == op_id)
        .collect()
}

/// A canonical workspace holding a git directory with one sample hook and a
/// project tool-config directory with one tool definition.
fn workspace() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dunce::canonicalize(dir.path()).expect("canonicalize");
    std::fs::create_dir_all(root.join(".git").join("hooks")).unwrap();
    std::fs::write(
        root.join(".git").join("hooks").join("pre-commit.sample"),
        "#!/bin/sh\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join(".ahma")).unwrap();
    std::fs::write(root.join(".ahma").join("echo.json"), "{}").unwrap();
    (dir, root)
}

fn adapter(root: &Path, mode: HandoffWatchMode) -> (Adapter, Arc<OperationMonitor>) {
    audit_log_path();
    let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
        TestTimeouts::get(TimeoutCategory::ToolCall),
    )));
    let shell_pool = Arc::new(ShellPoolManager::new(ShellPoolConfig::default()));
    let sandbox = Arc::new(
        Sandbox::new(
            vec![root.to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .expect("test sandbox"),
    );
    let adapter = Adapter::new(monitor.clone(), shell_pool, sandbox)
        .expect("adapter")
        .with_handoff_watch(mode);
    (adapter, monitor)
}

fn shell_args(command: &str) -> Map<String, Value> {
    let mut args = Map::new();
    args.insert("command".into(), json!(command));
    args.insert("c_flag".into(), json!(true));
    args
}

/// Create an empty `.git/hooks/pre-commit` — the attack in its smallest form.
fn create_hook_cmd() -> &'static str {
    if cfg!(windows) {
        "New-Item -ItemType File -Force .git/hooks/pre-commit | Out-Null"
    } else {
        "touch .git/hooks/pre-commit"
    }
}

/// Change an existing tool definition in `.ahma/`.
fn edit_tool_cmd() -> &'static str {
    if cfg!(windows) {
        "Add-Content -Path .ahma/echo.json -Value injected"
    } else {
        "echo injected >> .ahma/echo.json"
    }
}

fn harmless_cmd() -> &'static str {
    if cfg!(windows) {
        "Write-Output hello"
    } else {
        "echo hello"
    }
}

/// Run `command` through the asynchronous `run_terminal_command` path and wait
/// for the finished operation.
async fn run_async(
    adapter: &Adapter,
    monitor: &OperationMonitor,
    root: &Path,
    command: &str,
) -> Operation {
    let sc = AhmaMcpService::build_shell_subcommand_config(None, &ExecutionMode::AsyncResultPush);
    let id = adapter
        .execute_async_in_dir_with_options(
            "run_terminal_command",
            platform_shell_program(),
            root.to_str().unwrap(),
            AsyncExecOptions {
                id: None,
                args: Some(shell_args(command)),
                timeout: Some(TestTimeouts::get(TimeoutCategory::ToolCall).as_secs()),
                subcommand_config: Some(&sc),
                log_monitor_config: None,
            },
        )
        .await
        .expect("dispatch");
    tokio::time::timeout(
        TestTimeouts::get(TimeoutCategory::ToolCall),
        monitor.wait_for_operation(&id),
    )
    .await
    .expect("the command finishes")
    .expect("the operation is known")
}

fn result_alert(op: &Operation) -> Option<String> {
    op.result
        .as_ref()
        .and_then(|r| r.get("handoff_alert"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

// ─────────────────────────────────────────────────────────────────────────────
// Through the adapter
// ─────────────────────────────────────────────────────────────────────────────

/// The threat itself: a sandboxed shell command plants a hook that git will
/// later run unsandboxed. It must lead the result, reach the hub/TUI alert
/// stream, and be on the audit record.
#[tokio::test]
async fn a_hook_written_by_a_command_is_reported_in_the_result_and_the_audit_log() {
    let (_dir, root) = workspace();
    let (adapter, monitor) = adapter(&root, HandoffWatchMode::Always);

    let op = run_async(&adapter, &monitor, &root, create_hook_cmd()).await;
    let hook = root.join(".git").join("hooks").join("pre-commit");
    assert!(hook.exists(), "the command really did write the hook");

    let alert = result_alert(&op).expect("a trust-handoff alert in the result");
    let expected = format!(
        "{ALERT_PREFIX} {} (created) — git runs files in .git/hooks outside any sandbox; \
         review before your next git command",
        hook.display()
    );
    assert!(alert.starts_with(&expected), "{alert}");
    assert!(
        alert.contains("not prevented"),
        "it must not read as protection: {alert}"
    );
    assert!(
        op.alerts.iter().any(|a| a.contains(ALERT_PREFIX)),
        "the alert event is what the hub and the TUI show: {:?}",
        op.alerts
    );
    assert_eq!(
        op.result.as_ref().unwrap()["handoff_writes"]["changes"][0]["change"],
        "created"
    );

    let records = handoff_records(&op.id).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["path"], hook.display().to_string());
    assert_eq!(records[0]["change"], "created");
    assert!(
        records[0]["trigger"]
            .as_str()
            .is_some_and(|t| t.contains("git")),
        "{records:?}"
    );
}

/// The other deny-tier member: a project's own tool definitions, which ahma
/// itself would run after a `restart`.
#[tokio::test]
async fn a_tool_definition_changed_by_a_command_is_reported() {
    let (_dir, root) = workspace();
    let (adapter, monitor) = adapter(&root, HandoffWatchMode::Always);

    let op = run_async(&adapter, &monitor, &root, edit_tool_cmd()).await;
    let tool = root.join(".ahma").join("echo.json");

    let alert = result_alert(&op).expect("a trust-handoff alert in the result");
    assert!(
        alert.contains(&format!("{ALERT_PREFIX} {} (modified)", tool.display())),
        "{alert}"
    );
    assert!(alert.contains("restart"), "names what will run it: {alert}");
    let records = handoff_records(&op.id).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["change"], "modified");
}

/// Detection must cost nothing visible when nothing happened — an alert on
/// every command would be noise nobody reads.
#[tokio::test]
async fn a_command_that_touches_nothing_raises_no_alert() {
    let (_dir, root) = workspace();
    let (adapter, monitor) = adapter(&root, HandoffWatchMode::Always);

    let op = run_async(&adapter, &monitor, &root, harmless_cmd()).await;
    assert!(result_alert(&op).is_none(), "{:?}", op.result);
    assert!(
        !op.alerts.iter().any(|a| a.contains(ALERT_PREFIX)),
        "{:?}",
        op.alerts
    );
    assert!(handoff_records(&op.id).await.is_empty());
}

/// The synchronous path (terminal hooks, `ahma tool run`, sync-mode
/// `run_terminal_command`) leads its returned text with the same alert.
#[tokio::test]
async fn the_synchronous_path_leads_its_result_with_the_alert() {
    let (_dir, root) = workspace();
    let (adapter, _monitor) = adapter(&root, HandoffWatchMode::Always);
    let sc = AhmaMcpService::build_shell_subcommand_config(None, &ExecutionMode::Synchronous);

    let output = adapter
        .execute_sync_in_dir(
            platform_shell_program(),
            Some(shell_args(create_hook_cmd())),
            root.to_str().unwrap(),
            Some(TestTimeouts::get(TimeoutCategory::ToolCall).as_secs()),
            Some(&sc),
        )
        .await
        .expect("the command succeeds");
    assert!(
        output.starts_with(&format!(
            "{ALERT_PREFIX} {}",
            root.join(".git").join("hooks").join("pre-commit").display()
        )),
        "{output}"
    );
}

/// `Never` is a real off switch: the same write, no alert.
#[tokio::test]
async fn a_disabled_watch_reports_nothing() {
    let (_dir, root) = workspace();
    let (adapter, monitor) = adapter(&root, HandoffWatchMode::Never);

    let op = run_async(&adapter, &monitor, &root, create_hook_cmd()).await;
    assert!(root.join(".git/hooks/pre-commit").exists());
    assert!(result_alert(&op).is_none(), "{:?}", op.result);
    assert!(handoff_records(&op.id).await.is_empty());
}

// ─────────────────────────────────────────────────────────────────────────────
// The watch itself
// ─────────────────────────────────────────────────────────────────────────────

/// The targets are the same set the macOS kernel rules deny.
#[tokio::test]
async fn the_watch_covers_the_resolved_hooks_and_the_project_tool_config() {
    let (_dir, root) = workspace();
    let watch = HandoffWatch::begin(std::slice::from_ref(&root)).await;
    let targets = watch.targets().to_vec();
    assert!(
        targets.contains(&root.join(".git").join("hooks")),
        "{targets:?}"
    );
    assert!(targets.contains(&root.join(".ahma")), "{targets:?}");
}

/// A target too large to inventory in full is reported — on every command
/// while it stays that way — and changes before the cap are still found.
#[tokio::test]
async fn the_cap_is_reported() {
    let (_dir, root) = workspace();
    let hooks = root.join(".git").join("hooks");
    for name in ["a.sample", "b.sample", "c.sample"] {
        std::fs::write(hooks.join(name), "#").unwrap();
    }

    let watch = HandoffWatch::begin_with_cap(std::slice::from_ref(&root), 2).await;
    // Sorts first, so it lies inside the inventoried prefix.
    std::fs::write(hooks.join("0-pre-commit"), "#!/bin/sh\n").unwrap();
    let report = watch.finish().await;

    assert!(report.capped.contains(&hooks), "{report:?}");
    assert!(
        report
            .changes
            .iter()
            .any(|c| c.path == hooks.join("0-pre-commit") && c.kind == HandoffChangeKind::Created),
        "{report:?}"
    );
    let alert = report.render_alert();
    assert!(
        alert.contains(&format!("{INCOMPLETE_PREFIX} {}", hooks.display())),
        "{alert}"
    );

    // Nothing changed, still capped: still said.
    let quiet = HandoffWatch::begin_with_cap(std::slice::from_ref(&root), 2)
        .await
        .finish()
        .await;
    assert!(quiet.changes.is_empty(), "{quiet:?}");
    assert!(quiet.capped.contains(&hooks), "{quiet:?}");
}

/// ahma writes its own logs under `.ahma/logs` while every command runs; that
/// must never read as a planted tool definition.
#[tokio::test]
async fn ahmas_own_log_directory_is_not_reported() {
    let (_dir, root) = workspace();
    let watch = HandoffWatch::begin(std::slice::from_ref(&root)).await;
    let logs = root.join(".ahma").join("logs").join("operations");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(logs.join("op_1.log"), "output").unwrap();
    std::fs::write(root.join(".ahma").join(".gitignore"), "logs/\n").unwrap();

    let report = watch.finish().await;
    assert!(report.is_empty(), "{report:?}");
}

/// Removing a hook is a change too — a deleted guard is a weakened guard.
#[tokio::test]
async fn a_removed_hook_is_reported() {
    let (_dir, root) = workspace();
    let sample = root.join(".git").join("hooks").join("pre-commit.sample");
    let watch = HandoffWatch::begin(std::slice::from_ref(&root)).await;
    std::fs::remove_file(&sample).unwrap();

    let report = watch.finish().await;
    assert_eq!(report.changes.len(), 1, "{report:?}");
    assert_eq!(report.changes[0].path, sample);
    assert_eq!(report.changes[0].kind, HandoffChangeKind::Removed);
}
