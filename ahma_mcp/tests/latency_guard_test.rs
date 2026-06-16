//! Latency regression guards for the async execution path (R1 performance).
//!
//! These are benchmarks, not correctness tests, so they are `#[ignore]`d in
//! the default run and exercised via `cargo nextest run --run-ignored all`
//! (part of the Definition of Done).  Bounds are deliberately generous —
//! they catch order-of-magnitude regressions (an accidental sync wait, a
//! per-line lock turning quadratic), not millisecond drift.
//!
//! Note: commands are spawned directly through the sandboxed process path —
//! the prewarmed shell pool is not part of the async hot path.

use ahma_mcp::adapter::Adapter;
use ahma_mcp::operation_monitor::{MonitorConfig, OperationMonitor};
use ahma_mcp::sandbox::{Sandbox, SandboxMode};
use ahma_mcp::shell_pool::{ShellPoolConfig, ShellPoolManager};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn build_adapter(scope: std::path::PathBuf) -> (Arc<Adapter>, Arc<OperationMonitor>) {
    let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
        Duration::from_secs(60),
    )));
    let shell_pool = Arc::new(ShellPoolManager::new(ShellPoolConfig::default()));
    let sandbox =
        Arc::new(Sandbox::new(vec![scope], SandboxMode::Test, false, false, false).unwrap());
    (
        Arc::new(Adapter::new(monitor.clone(), shell_pool, sandbox).unwrap()),
        monitor,
    )
}

/// Write a shell script into the sandbox scope and return the command string
/// that runs it (cross-platform: bash on Unix, PowerShell on Windows).
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

/// End-to-end latency of a trivial async operation: dispatch → spawn →
/// stream → terminal event → result retrievable.  Guards against an
/// accidental synchronous wait or polling delay creeping into the path.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "latency benchmark — run via --run-ignored all"]
async fn async_echo_end_to_end_latency_guard() {
    let temp = tempdir().unwrap();
    let (adapter, monitor) = build_adapter(temp.path().to_path_buf());

    let cmd = write_script(
        temp.path(),
        "echo_once",
        "#!/bin/bash\necho run-ok\n",
        "Write-Output 'run-ok'\n",
    );

    const RUNS: usize = 10;
    let mut timings = Vec::with_capacity(RUNS);

    for i in 0..RUNS {
        let start = Instant::now();
        let id = adapter
            .execute_async_in_dir(
                "latency_echo",
                &cmd,
                None,
                temp.path().to_str().unwrap(),
                Some(30),
            )
            .await
            .expect("operation should start");
        let op = tokio::time::timeout(Duration::from_secs(10), monitor.wait_for_operation(&id))
            .await
            .expect("operation should finish within 10s")
            .expect("operation should be in history");
        timings.push(start.elapsed());
        let stdout = op
            .result
            .as_ref()
            .and_then(|r| r.get("stdout"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            stdout.contains("run-ok"),
            "run {i}: expected echoed output, result: {:?}",
            op.result
        );
    }

    timings.sort();
    let median = timings[RUNS / 2];
    let worst = *timings.last().unwrap();
    println!("async echo latency: median={median:?} worst={worst:?} all={timings:?}");

    // Direct spawn of a non-interactive shell is single-digit milliseconds;
    // the full pipeline (sandbox wrap, monitor, streaming, history) should
    // stay well under a second even on a loaded CI machine.
    assert!(
        median < Duration::from_millis(1000),
        "median async echo latency regressed: {median:?} (expected < 1s)"
    );
    assert!(
        worst < Duration::from_secs(3),
        "worst-case async echo latency regressed: {worst:?} (expected < 3s)"
    );
}

/// Per-line streaming cost guard: 5000 lines must flow through redaction,
/// the bounded collector, the spill writer, and the monitor's tail buffer +
/// event emission without the per-line locking turning pathological.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "throughput benchmark — run via --run-ignored all"]
async fn streaming_5000_lines_throughput_guard() {
    let temp = tempdir().unwrap();
    let (adapter, monitor) = build_adapter(temp.path().to_path_buf());

    let cmd = write_script(
        temp.path(),
        "seq_5000",
        "#!/bin/bash\ni=0; while [ $i -lt 5000 ]; do echo line-$i; i=$((i+1)); done\n",
        "0..4999 | ForEach-Object { Write-Output \"line-$_\" }\n",
    );

    let start = Instant::now();
    let id = adapter
        .execute_async_in_dir(
            "throughput_seq",
            &cmd,
            None,
            temp.path().to_str().unwrap(),
            Some(60),
        )
        .await
        .expect("operation should start");

    let op = tokio::time::timeout(Duration::from_secs(30), monitor.wait_for_operation(&id))
        .await
        .expect("operation should finish within 30s")
        .expect("operation should be in history");
    let elapsed = start.elapsed();
    println!("5000-line streaming completed in {elapsed:?}");

    let stdout = op
        .result
        .as_ref()
        .and_then(|r| r.get("stdout"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        stdout.contains("line-4999"),
        "last line must be present in the result"
    );

    // ~2 ms/line would already be pathological; allow 10s total (2 ms/line)
    // to absorb slow CI machines while still catching quadratic behaviour.
    assert!(
        elapsed < Duration::from_secs(10),
        "streaming 5000 lines took {elapsed:?} (expected < 10s) — per-line cost regressed"
    );
}
