//! [`Adapter::run_to_completion`]: a command a caller waits for runs as a
//! tracked operation, and the caller reads its result as the direct path gave
//! it (SPEC R2.1).

use super::*;
use crate::operation_monitor::MonitorConfig;
use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};

fn adapter_in(scope: &Path) -> Adapter {
    Adapter::new(
        Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            TestTimeouts::get(TimeoutCategory::ToolCall),
        ))),
        Arc::new(ShellPoolManager::new(
            crate::shell_pool::ShellPoolConfig::default(),
        )),
        Arc::new(
            sandbox::Sandbox::new(
                vec![scope.to_path_buf()],
                sandbox::SandboxMode::Test,
                Default::default(),
            )
            .unwrap(),
        ),
    )
    .unwrap()
}

fn shell_args(line: &str) -> Map<String, Value> {
    let mut args = Map::new();
    args.insert("command".into(), json!(line));
    args.insert("c_flag".into(), json!(true));
    args
}

fn shell_config(limit: u64) -> crate::config::SubcommandConfig {
    crate::AhmaMcpService::build_shell_subcommand_config(Some(limit), &ExecutionMode::Synchronous)
}

fn scope() -> (tempfile::TempDir, std::path::PathBuf) {
    let td = tempfile::tempdir().unwrap();
    let scope = dunce::canonicalize(td.path()).unwrap();
    (td, scope)
}

async fn waited(adapter: &Adapter, dir: &Path, line: &str, limit: u64) -> Result<String> {
    adapter
        .run_to_completion(
            None,
            crate::shell_pool::platform_shell_program(),
            Some(shell_args(line)),
            &dir.to_string_lossy(),
            Some(limit),
            Some(&shell_config(limit)),
        )
        .await
}

/// What a caller reads from a result, typed errors included.
fn reads(result: &Result<String>) -> String {
    match result {
        Ok(out) => format!("ok: {out:?}"),
        Err(e) => {
            let kind = if let Some(stopped) = e.downcast_ref::<time_limit::LimitReached>() {
                format!(
                    "limit {}s, printed {:?}",
                    stopped.limit_secs, stopped.output
                )
            } else if let Some(denial) = e.downcast_ref::<sandbox::SandboxError>() {
                format!("sandbox {denial:?}")
            } else {
                "untyped".to_string()
            };
            // The message chain, not `{e:?}`: under `RUST_BACKTRACE` the debug
            // form carries a backtrace whose frames differ by call path.
            format!("err ({kind}): {e:#}")
        }
    }
}

/// SPEC R2.1: a caller that waits reads what it read from the direct path,
/// byte for byte — a missing last newline, a `\r\n`, bytes that are not UTF-8,
/// an exit code, a refusal its own output names (typed, for the hook to read)
/// and a time limit (typed, with what it printed).
#[cfg(unix)]
#[tokio::test]
async fn a_waited_result_reads_as_the_direct_path_read() {
    let (_td, scope) = scope();
    let adapter = adapter_in(&scope);
    let outside = crate::test_utils::path_helpers::test_out_of_scope_path();
    let refused = format!(
        "printf 'touch: {}: Operation not permitted\\n' >&2; exit 1",
        outside.display()
    );
    let cases: Vec<(&str, u64)> = vec![
        ("printf 'out\\n'; printf 'err\\n' >&2", 30),
        ("printf 'no newline'", 30),
        ("true", 30),
        ("printf 'a\\r\\nb\\r\\n'", 30),
        ("printf 'bad \\377 byte\\n'", 30),
        ("printf 'out\\n'; printf 'err\\n' >&2; exit 3", 30),
        (&refused, 30),
        ("printf 'before the limit\\n'; sleep 30", 1),
    ];
    for (line, limit) in cases {
        let direct = adapter
            .execute_sync_in_dir(
                crate::shell_pool::platform_shell_program(),
                Some(shell_args(line)),
                &scope.to_string_lossy(),
                Some(limit),
                Some(&shell_config(limit)),
            )
            .await;
        let waited = waited(&adapter, &scope, line, limit).await;
        assert_eq!(reads(&waited), reads(&direct), "{line}");
    }
}

/// SPEC R2.1: the command a caller waits for is an operation like any
/// other — in history with its ending, its output spilled to its file — not
/// an invisible direct run.
#[tokio::test]
async fn a_waited_command_is_a_tracked_operation() {
    let (_td, scope) = scope();
    let adapter = adapter_in(&scope);
    let out = waited(&adapter, &scope, "echo tracked", 30)
        .await
        .expect("it succeeds");
    assert!(out.contains("tracked"), "{out}");
    let ops = adapter.monitor.get_completed_operations().await;
    let [op] = ops.as_slice() else {
        panic!("one operation: {ops:?}");
    };
    assert_eq!(op.state, OperationStatus::Completed);
    let spill = op.output_file.clone().expect("advertised");
    let spilled = tokio::fs::read_to_string(&spill).await.unwrap_or_default();
    assert!(spilled.contains("tracked"), "{spilled}");
}

/// SPEC R2.1: dropping the future of a caller that waits — what a Ctrl-C or
/// SIGTERM does to a hook or a CLI one-shot — cancels the operation, and its
/// whole process tree dies with it, a grandchild included.
#[cfg(unix)]
#[tokio::test]
async fn dropping_a_waiting_caller_kills_the_command_tree() {
    let (_td, scope) = scope();
    let adapter = adapter_in(&scope);
    let pid_file = scope.join("sleep.pid");
    let line = format!("sleep 300 & echo $! > '{}'; wait", pid_file.display());
    let mut run = Box::pin(waited(&adapter, &scope, &line, 600));
    let deadline = tokio::time::Instant::now() + TestTimeouts::get(TimeoutCategory::ToolCall);
    let pid: i32 = loop {
        tokio::select! {
            ended = &mut run => panic!("the command ended on its own: {ended:?}"),
            _ = tokio::time::sleep(TestTimeouts::poll_interval()) => {}
        }
        if let Some(pid) = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            break pid;
        }
        assert!(tokio::time::Instant::now() < deadline, "never started");
    };
    // SAFETY: signal 0 only asks whether the pid exists.
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "the sleep runs");

    drop(run);

    crate::test_utils::assertions::assert_eventually(
        TestTimeouts::get(TimeoutCategory::Cleanup),
        TestTimeouts::poll_interval(),
        "the grandchild `sleep` is gone",
        // SAFETY: as above.
        || async { (unsafe { libc::kill(pid, 0) }) != 0 },
    )
    .await;
    crate::test_utils::assertions::assert_eventually(
        TestTimeouts::get(TimeoutCategory::Cleanup),
        TestTimeouts::poll_interval(),
        "the operation ended cancelled, with why",
        || async {
            adapter
                .monitor
                .get_completed_operations()
                .await
                .iter()
                .any(|op| {
                    op.state == OperationStatus::Cancelled
                        && op.result.as_ref().is_some_and(|r| {
                            r["reason"]
                                .as_str()
                                .is_some_and(|why| why.contains("stopped waiting"))
                        })
                })
        },
    )
    .await;
}

/// SPEC R2.7.1: a caller that waits waits for its workspace no longer than
/// its command's own limit, is told once that it waits, and — when its turn
/// never comes — is told the command did **not** run and who held the
/// workspace, in the words the direct path used.
#[tokio::test]
async fn a_waiting_caller_is_told_once_and_not_run_past_its_limit() {
    let (td, scope) = scope();
    let notices = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
    let sink = notices.clone();
    let adapter = adapter_in(&scope)
        .with_workspace_queue(workspace_queue::WorkspaceQueue::with_lock_dir(
            true,
            Some(td.path().join("locks")),
        ))
        .with_queue_wait_notice(Arc::new(move |line: &str| {
            sink.lock().push(line.to_string())
        }));
    let key = adapter.workspace_key_for(&scope);
    let _held = adapter
        .workspace_queue()
        .enqueue(
            &key,
            workspace_queue::HolderInfo::new("op_holder", "cargo nextest run"),
        )
        .unwrap()
        .acquire(&tokio_util::sync::CancellationToken::new(), &|_| {})
        .await
        .unwrap();

    let started = Instant::now();
    let line = "touch out.txt";
    let err = waited(&adapter, &scope, line, 1)
        .await
        .expect_err("the workspace never frees up, so the command must not run");
    let text = err.to_string();
    assert!(
        text.starts_with("Not run: the workspace stayed busy for its whole 1s timeout. "),
        "{text}"
    );
    assert!(text.contains("op_holder"), "names the holder: {text}");
    assert!(
        text.ends_with("Retry once that finishes, or cancel it."),
        "{text}"
    );
    assert!(
        started.elapsed() < TestTimeouts::get(TimeoutCategory::Quick),
        "bounded by its own one-second limit"
    );
    let notices = notices.lock();
    assert_eq!(notices.len(), 1, "one notice per wait: {notices:?}");
    assert!(
        notices[0].starts_with("ahma: NOT started — `touch out.txt`"),
        "{notices:?}"
    );
    assert!(!scope.join("out.txt").exists(), "it did not run");
}

/// SPEC R2.7.2: a command a caller waits for takes its `tools.mutex_groups`
/// gate like every other operation; the direct path never did.
#[tokio::test]
async fn a_waited_command_waits_for_its_mutex_group() {
    let (_td, scope) = scope();
    let locks = tempfile::tempdir().unwrap();
    let group = ahma_common::config::MutexGroupConfig {
        name: "echo".to_string(),
        prefixes: vec!["echo".to_string()],
        max_wait_secs: 0,
    };
    let registry = Arc::new(CommandMutexRegistry::from_config_with_lock_dir(
        std::slice::from_ref(&group),
        Some(locks.path().to_path_buf()),
    ));
    let plain = adapter_in(&scope);
    let adapter = Adapter::new_with_registry(
        plain.monitor.clone(),
        plain.shell_pool.clone(),
        plain.sandbox.clone(),
        registry.clone(),
    )
    .unwrap();
    let _held = registry
        .acquire(&group, &adapter.workspace_key_for(&scope))
        .await
        .expect("the group is free");
    let err = waited(&adapter, &scope, "echo gated", 30)
        .await
        .expect_err("a held group stops it");
    assert!(err.to_string().contains("echo"), "{err}");
    let ops = adapter.monitor.get_completed_operations().await;
    assert!(
        ops.iter().any(|op| op.state == OperationStatus::Failed),
        "{ops:?}"
    );
}
