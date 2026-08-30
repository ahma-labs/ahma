//! Regression guards for the idle watchdog's proof-of-life wiring.
//!
//! The idle watchdog lives in [`OperationMonitor::check_timeouts`] and measures
//! from `last_activity`. Output touches that stamp; **CPU burned by a silent
//! process tree does not**, and cannot — the monitor has no view of the child.
//! The streaming loop in `adapter` is what bridges the two, by sampling the
//! process tree and calling `note_liveness` when the total rises.
//!
//! That bridge has been removed once already (it was replaced with a local
//! variable that the monitor never sees), and the unit tests did not notice:
//! `cpu_activity_keeps_a_silent_operation_alive` calls `note_liveness` itself,
//! so it passes whether or not production ever does. These tests therefore
//! assert the **wiring**, end to end, with nothing stubbed — a silent but busy
//! command must survive the watchdog, and a genuinely stalled one must not.

use ahma_mcp::adapter::Adapter;
use ahma_mcp::operation_monitor::{MonitorConfig, OperationMonitor, OperationStatus};
use ahma_mcp::sandbox::{Sandbox, SandboxMode};
use ahma_mcp::shell_pool::{ShellPoolConfig, ShellPoolManager};
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;

/// The budget cannot be shrunk to make this test fast, and it is worth saying
/// why. The CPU probe only runs on the streaming loop's **10s heartbeat**, and
/// only once the operation has been silent for `cpu_probe_threshold`
/// (`idle/3`, capped at 30s). The first sample is a baseline, so proof of life
/// needs a *second* one — 20s minimum. An idle budget below roughly 25s is
/// therefore killed before liveness can ever be reported, and a shorter budget
/// here would produce a test that passes for the wrong reason.
///
/// 45s: probe threshold 15s, so heartbeats at t=20s and t=30s bracket it.
const IDLE_BUDGET: Duration = Duration::from_secs(45);

/// Must exceed [`IDLE_BUDGET`], or the watchdog never gets the chance to make
/// the mistake this test exists to catch. Verified to fail (`TimedOut` at
/// ~t+46s) with the `note_liveness` call removed.
const OBSERVE_FOR: Duration = Duration::from_secs(60);

fn build_adapter(scope: std::path::PathBuf) -> (Arc<Adapter>, Arc<OperationMonitor>) {
    let monitor = Arc::new(OperationMonitor::new(
        MonitorConfig::with_timeout(Duration::from_secs(600)).with_idle_timeout(Some(IDLE_BUDGET)),
    ));
    let shell_pool = Arc::new(ShellPoolManager::new(ShellPoolConfig::default()));
    let sandbox =
        Arc::new(Sandbox::new(vec![scope], SandboxMode::Test, false, false, false).unwrap());
    (
        Arc::new(Adapter::new(monitor.clone(), shell_pool, sandbox).unwrap()),
        monitor,
    )
}

/// Write a script into the sandbox scope and return the command that runs it.
fn write_script(dir: &std::path::Path, name: &str, bash: &str, ps1: &str) -> String {
    #[cfg(windows)]
    {
        let _ = bash;
        let path = dir.join(format!("{name}.ps1"));
        std::fs::write(&path, ps1).unwrap();
        format!(
            "powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File {}",
            path.to_string_lossy()
        )
    }
    #[cfg(not(windows))]
    {
        let _ = ps1;
        let path = dir.join(format!("{name}.sh"));
        std::fs::write(&path, bash).unwrap();
        format!("bash {}", path.display())
    }
}

/// The operation's current state, wherever it lives.
///
/// A terminal transition *moves* the operation out of the active map and into
/// `completion_history`, so `get_operation` alone reports `None` for exactly the
/// outcome these tests are looking for — a guard that only checked the active
/// map would pass by failing to look.
async fn state_of(monitor: &OperationMonitor, id: &str) -> Option<OperationStatus> {
    if let Some(op) = monitor.get_operation(id).await {
        return Some(op.state);
    }
    monitor
        .check_completion_history_pub(id)
        .await
        .map(|op| op.state)
}

/// Drive the watchdog the way the production background task does (1s cadence),
/// stopping early if the operation reaches a terminal state.
async fn run_watchdog_for(
    monitor: &OperationMonitor,
    id: &str,
    observe_for: Duration,
) -> OperationStatus {
    let deadline = tokio::time::Instant::now() + observe_for;
    let mut last = OperationStatus::InProgress;
    while tokio::time::Instant::now() < deadline {
        monitor.check_timeouts().await;
        if let Some(state) = state_of(monitor, id).await {
            last = state;
            if last.is_terminal() {
                return last;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    last
}

/// A command that burns CPU without printing must NOT be reaped: silence is not
/// a stall. This is the shape of every buffered pipeline (`cargo test | tail`)
/// and every long quiet compile, and killing it is the bug this guards.
///
/// `#[ignore]` because it costs a real minute of wall clock and cannot be made
/// to cost less (see [`IDLE_BUDGET`]) — it is real-process, real-CPU, real-time
/// by construction. It is part of the required set via
/// `cargo nextest run --workspace --run-ignored all` (AGENTS.md, Definition of
/// Done), alongside the other expensive regression guards.
#[ignore = "expensive: needs ~60s of real wall clock to cross the idle budget"]
#[tokio::test(flavor = "multi_thread")]
async fn silent_but_cpu_busy_operation_survives_the_idle_watchdog() {
    let temp = tempdir().unwrap();
    let (adapter, monitor) = build_adapter(temp.path().to_path_buf());

    // Busy-loop, no output at all, for longer than we observe.
    let cmd = write_script(
        temp.path(),
        "busy_silent",
        "end=$(( $(date +%s) + 120 )); while [ $(date +%s) -lt $end ]; do :; done",
        "$end = (Get-Date).AddSeconds(120); while ((Get-Date) -lt $end) { }",
    );

    let id = adapter
        .execute_async_in_dir(
            "busy_silent",
            &cmd,
            None,
            temp.path().to_str().unwrap(),
            Some(600),
        )
        .await
        .expect("operation should start");

    let status = run_watchdog_for(&monitor, &id, OBSERVE_FOR).await;

    assert_ne!(
        status,
        OperationStatus::TimedOut,
        "a silent process tree that is burning CPU must not be reaped as stalled — \
         the streaming loop is failing to report liveness to the monitor"
    );

    // And the monitor's own clock — not just a local variable in the adapter —
    // must have advanced, because that clock is what check_timeouts reads.
    let op = monitor.get_operation(&id).await.expect("operation exists");
    let idle = std::time::SystemTime::now()
        .duration_since(op.last_activity.get())
        .unwrap_or_default();
    assert!(
        idle < IDLE_BUDGET,
        "the monitor's last_activity must have been touched by the CPU probe; it is \
         {idle:?} stale against a {IDLE_BUDGET:?} budget"
    );

    let _ = monitor.cancel_operation(&id).await;
}

/// The paired negative: a genuinely idle command — no output, no CPU — must
/// still be reaped. Without this, "never kill anything" would pass the test
/// above, and the watchdog would be dead code.
#[tokio::test(flavor = "multi_thread")]
async fn silent_and_cpu_idle_operation_is_still_reaped() {
    let temp = tempdir().unwrap();
    let (adapter, monitor) = build_adapter(temp.path().to_path_buf());

    let cmd = write_script(
        temp.path(),
        "silent_sleep",
        "sleep 300",
        "Start-Sleep -Seconds 300",
    );

    let id = adapter
        .execute_async_in_dir(
            "silent_sleep",
            &cmd,
            None,
            temp.path().to_str().unwrap(),
            Some(600),
        )
        .await
        .expect("operation should start");

    // Backdate the activity stamp rather than waiting out the real budget: the
    // question here is whether check_timeouts still fires on a tree with no CPU
    // to show, not how long it takes to get there.
    if let Some(op) = monitor.get_operation(&id).await {
        op.last_activity
            .set(std::time::SystemTime::now() - IDLE_BUDGET * 2);
    }

    monitor.check_timeouts().await;

    assert_eq!(
        state_of(&monitor, &id).await,
        Some(OperationStatus::TimedOut),
        "a silent operation whose process tree burned no CPU must still be reaped"
    );

    let _ = monitor.cancel_operation(&id).await;
}
