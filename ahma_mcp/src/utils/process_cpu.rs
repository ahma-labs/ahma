//! Process-tree CPU sampling for the operation liveness watchdog.
//!
//! The idle watchdog used to equate "produced no output" with "made no progress"
//! and kill the operation. That is false for a great many healthy commands:
//!
//! * a shell pipeline whose last stage buffers (`… | tail`, `| grep`, `| sort`)
//!   emits nothing until EOF,
//! * a compile or link step can be silent for minutes,
//! * a test runner without `--nocapture` prints only at the end.
//!
//! It really did kill a healthy `cargo nextest run 2>&1 | tail -25` at exactly
//! 300s, reporting "likely wedged on a lock or a denied write".
//!
//! Silence is therefore not evidence of a stall. Burning CPU *is* evidence of
//! progress, so before declaring an operation wedged we ask whether its process
//! tree consumed any CPU since the last sample.
//!
//! The tree, not the child: the direct child is usually a shell, which itself
//! uses no measurable CPU while it waits — the work happens in grandchildren
//! (`cargo` → `rustc`). Summing only the direct child would report ~0 and kill
//! the very builds this is meant to protect.

use std::collections::HashMap;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// Total CPU time in milliseconds consumed by `root_pid` and every descendant.
///
/// Returns `None` when the root process is not (or no longer) visible — the
/// caller must treat that as "no information", never as "no progress".
pub fn process_tree_cpu_ms(root_pid: u32) -> Option<u64> {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_cpu(),
    );

    let root = Pid::from_u32(root_pid);
    sys.process(root)?;

    // parent -> children, so the tree can be walked without re-scanning per node.
    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (pid, process) in sys.processes() {
        if let Some(parent) = process.parent() {
            children.entry(parent).or_default().push(*pid);
        }
    }

    let mut total_ms: u64 = 0;
    let mut stack = vec![root];
    let mut visited = 0usize;
    while let Some(pid) = stack.pop() {
        // Guard against a pathological/cyclic parent chain reported by the OS.
        visited += 1;
        if visited > 10_000 {
            break;
        }
        if let Some(process) = sys.process(pid) {
            total_ms = total_ms.saturating_add(process.accumulated_cpu_time());
        }
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids.iter().copied());
        }
    }

    Some(total_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The current process is always visible and has consumed some CPU.
    #[test]
    fn samples_the_current_process() {
        let cpu = process_tree_cpu_ms(std::process::id())
            .expect("the current process must be visible to the sampler");
        // Its own accumulated time may round to 0ms on a fast machine, so assert
        // only that sampling succeeded and produced a sane value.
        assert!(cpu < 1_000 * 60 * 60 * 24, "implausible CPU total: {cpu}ms");
    }

    /// A process that does not exist yields `None` — "no information", which the
    /// watchdog must never confuse with "no progress".
    #[test]
    fn unknown_pid_is_none_not_zero() {
        // PID 0 is not a normal user process on any supported platform.
        assert_eq!(process_tree_cpu_ms(u32::MAX), None);
    }

    /// The whole point: CPU burned by a *descendant* must be counted, because the
    /// direct child is typically a shell that idles while its children work.
    #[cfg(unix)]
    #[test]
    fn counts_cpu_burned_by_a_grandchild() {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("grandchild.pid");

        // `sh` (the child) spawns a busy loop (the grandchild) and just waits, so
        // essentially all the CPU is burned one level down.
        //
        // `process_group(0)` makes the child a process-group leader — the same flag
        // `Sandbox::base_command` sets in production — so the cleanup below can
        // `kill(-pgid)` the whole group. This test used to spawn without it and
        // SIGKILL only the direct child, which orphaned the busy loop to `launchd`
        // where it spun at 100% CPU forever with nothing left to reap it. One such
        // process leaked per test run, so a dev machine accumulated a hot core per
        // `cargo nextest run` (invisible on CI, whose runners are discarded).
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "sh -c 'while : ; do : ; done' & echo $! > {}; wait",
                pidfile.display()
            ))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn busy-loop child");

        let pid = child.id();
        let first = process_tree_cpu_ms(pid).unwrap_or(0);
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let second = process_tree_cpu_ms(pid).unwrap_or(0);

        // Negative pid targets the process group, so the grandchild dies with the
        // shell rather than outliving it.
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        let _ = child.kill();
        let _ = child.wait();

        assert!(
            second > first,
            "a busy grandchild must register as CPU progress ({first}ms -> {second}ms); \
             without this the watchdog kills silent-but-working builds"
        );

        // Regression guard: the busy loop must not survive the test. Reading the pid
        // after the kill is deliberate — the assertion above is what this test is
        // *for*, and it must not be skipped just because cleanup is being checked.
        let gpid: i32 = std::fs::read_to_string(&pidfile)
            .expect("grandchild pid file should be written")
            .trim()
            .parse()
            .expect("grandchild pid should parse");
        let mut dead = false;
        for _ in 0..100 {
            // Signal 0 only probes for existence.
            if unsafe { libc::kill(gpid, 0) } != 0 {
                dead = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            dead,
            "the busy-loop grandchild (pid {gpid}) must be killed via the process-group \
             kill, not orphaned to spin at 100% CPU forever"
        );
    }
}
