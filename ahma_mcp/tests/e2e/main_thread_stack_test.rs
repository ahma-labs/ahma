//! The `ahma` binary must not depend on the size of the process's main-thread
//! stack.
//!
//! Windows gives the main thread a fixed 1 MiB stack, set in the PE header, and a
//! debug build of ahma had crept up to needing ~990 KiB of it just to parse its
//! command line. The next few bytes of CLI surface pushed it over, and every
//! subprocess test on the Windows leg died with `thread 'main' has overflowed its
//! stack` before the MCP handshake. `main` now runs ahma on a thread whose stack
//! size ahma chooses, so the platform's main-thread size no longer matters.
//!
//! Linux-only because the only way to reproduce a small main-thread stack from a
//! test is lowering `RLIMIT_STACK` before `exec`, which Linux honours for the
//! next image's main thread. macOS refuses the same `setrlimit` in the forked
//! child with `EINVAL` (below the stack the process already has), so the test
//! could not even spawn `ahma` there. The Windows leg exercises the real 1 MiB
//! constraint on every run through all its other subprocess tests.
#![cfg(target_os = "linux")]

use ahma_mcp::test_utils::cli::{build_binary_cached, test_command};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

/// A quarter of Windows' 1 MiB: far below what the CLI used to need, so the test
/// fails loudly if anything starts running on the main thread's own stack again.
const SMALL_MAIN_STACK: libc::rlim_t = 256 * 1024;

fn with_small_main_stack(cmd: &mut Command) -> &mut Command {
    // SAFETY: only async-signal-safe libc calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::getrlimit(libc::RLIMIT_STACK, &mut limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            limit.rlim_cur = SMALL_MAIN_STACK.min(limit.rlim_max);
            if libc::setrlimit(libc::RLIMIT_STACK, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        })
    }
}

#[test]
fn version_and_help_run_on_a_small_main_thread_stack() {
    let binary = build_binary_cached("ahma_bin", "ahma");
    for arg in ["--version", "--help"] {
        let output = with_small_main_stack(&mut test_command(&binary))
            .arg(arg)
            .output()
            .expect("spawn ahma");
        assert!(
            output.status.success(),
            "ahma {arg} must not need a large main-thread stack: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn a_hook_runs_on_a_small_main_thread_stack() {
    let binary = build_binary_cached("ahma_bin", "ahma");
    let mut child = with_small_main_stack(&mut test_command(&binary))
        .args(["hooks", "edit-guard", "--platform", "claude"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ahma");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"tool_name":"Read","tool_input":{}}"#)
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "ahma hooks edit-guard must not need a large main-thread stack: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
