use super::error::SandboxError;

/// Check if the platform's sandboxing prerequisites are met.
pub fn check_sandbox_prerequisites() -> Result<(), SandboxError> {
    #[cfg(target_os = "linux")]
    {
        check_landlock_available()
    }

    #[cfg(target_os = "macos")]
    {
        check_macos_sandbox_available()
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        #[cfg(target_os = "windows")]
        {
            super::windows::check_windows_sandbox_available()
        }
        #[cfg(not(target_os = "windows"))]
        {
            Err(SandboxError::UnsupportedOs(
                std::env::consts::OS.to_string(),
            ))
        }
    }
}

#[cfg(target_os = "linux")]
fn check_landlock_available() -> Result<(), SandboxError> {
    // Probe the syscall directly. Kernel version and the LSM list can both
    // lie: containers may block the syscall via seccomp, securityfs may be
    // unmounted, or Landlock may be compiled out / not enabled at boot — all
    // on kernels whose version implies support. The only authoritative answer
    // is asking the kernel for the Landlock ABI version.
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1 << 0;
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if abi >= 1 {
        Ok(())
    } else {
        Err(SandboxError::LandlockNotAvailable)
    }
}

#[cfg(target_os = "macos")]
fn check_macos_sandbox_available() -> Result<(), SandboxError> {
    use std::process::Command;
    let result = Command::new("which").arg("sandbox-exec").output();
    match result {
        Ok(output) if output.status.success() => Ok(()),
        _ => Err(SandboxError::MacOSSandboxNotAvailable),
    }
}

#[cfg(target_os = "macos")]
pub fn test_sandbox_exec_available() -> Result<(), SandboxError> {
    use std::process::Command;
    let test_profile = "(version 1)(allow default)";
    let result = Command::new("sandbox-exec")
        .args(["-p", test_profile, "/usr/bin/true"])
        .output();
    match result {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("Operation not permitted")
                || stderr.contains("sandbox_apply")
                || output.status.code() == Some(71)
            {
                Err(SandboxError::NestedSandboxDetected)
            } else {
                tracing::debug!("sandbox-exec test failed: {}", stderr);
                Err(SandboxError::NestedSandboxDetected)
            }
        }
        Err(e) => {
            tracing::debug!("sandbox-exec exec failed: {}", e);
            Err(SandboxError::MacOSSandboxNotAvailable)
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn test_sandbox_exec_available() -> Result<(), SandboxError> {
    Ok(())
}

/// Whether **this process** is already confined by a macOS Seatbelt profile —
/// asked of the kernel directly via `sandbox_check(getpid(), NULL, …)`, the
/// same call Chromium and others use. No subprocess, no filesystem probe.
///
/// This is the cheap half of nested-Seatbelt detection (SPEC R7.6): a process
/// that is not confined cannot be refused a nested profile, so the subprocess
/// probe in [`test_sandbox_exec_available`] only ever runs when this says
/// `true`. Every child of a confined process inherits the confinement — that
/// is what makes deferring to the outer sandbox safe in kind.
#[cfg(target_os = "macos")]
pub fn process_is_seatbelt_confined() -> bool {
    // SANDBOX_CHECK_NO_REPORT: answer without logging a violation.
    const SANDBOX_CHECK_NO_REPORT: libc::c_int = 0x0002;
    unsafe extern "C" {
        // libsystem_sandbox: `int sandbox_check(pid_t pid, const char *operation,
        // int type, ...)`. With a NULL operation it reports whether the process
        // is sandboxed at all: 1 when confined, 0 when not, -1 on error.
        fn sandbox_check(
            pid: libc::pid_t,
            operation: *const libc::c_char,
            type_: libc::c_int,
            ...
        ) -> libc::c_int;
    }
    // SAFETY: plain FFI query on our own pid with a NULL operation, which the
    // documented calling convention permits; no pointers are retained.
    let rc = unsafe {
        sandbox_check(
            std::process::id() as libc::pid_t,
            std::ptr::null(),
            SANDBOX_CHECK_NO_REPORT,
        )
    };
    rc == 1
}

#[cfg(not(target_os = "macos"))]
pub fn process_is_seatbelt_confined() -> bool {
    false
}

/// The outer sandbox ahma must defer to because macOS refuses to nest its
/// Seatbelt profile inside it (SPEC R7.6), or `None` when ahma can enforce.
///
/// Both halves of the proof are required, and the verdict is cached for the
/// life of the process (confinement is irrevocable, so it cannot change):
///
/// 1. [`process_is_seatbelt_confined`] — the kernel says this process is
///    inside a profile. Cheap, and `false` for the overwhelming majority of
///    processes, which therefore never pay for step 2.
/// 2. [`test_sandbox_exec_available`] — a nested `sandbox-exec` is actually
///    *refused* (`sandbox_apply: Operation not permitted`). Measured on
///    macOS 26: the kernel denies a nested profile whenever the outer profile
///    denies anything at all — `(allow default)` plus a single `deny` of an
///    unrelated path is enough — so every real sandbox, ahma's own included,
///    forbids nesting. Only a no-op `(allow default)` outer profile permits
///    it, and in that case ahma keeps enforcing.
///
/// The host is named when it can be (SPEC R7.1): ahma's own `run_terminal_command`
/// stamps its children, so the dogfooding case — ahma's test suite, or a nested
/// `ahma serve`, run *through* ahma — reports "ahma" rather than "an outer
/// sandbox".
pub fn nested_seatbelt_denial() -> Option<super::host_detect::HostSandbox> {
    static VERDICT: std::sync::OnceLock<Option<super::host_detect::HostSandbox>> =
        std::sync::OnceLock::new();
    *VERDICT.get_or_init(|| {
        if !process_is_seatbelt_confined() {
            return None;
        }
        match test_sandbox_exec_available() {
            Err(SandboxError::NestedSandboxDetected) => Some(
                super::host_detect::detect_host_sandbox()
                    .unwrap_or(super::host_detect::HostSandbox::Unidentified),
            ),
            _ => None,
        }
    })
}

pub fn exit_with_sandbox_error(error: &SandboxError) -> ! {
    eprintln!("\n\u{274c} SECURITY ERROR: Cannot start MCP server\n");
    eprintln!("Reason: {}\n", error);
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// check_sandbox_prerequisites should return Ok or a specific SandboxError
    /// depending on the current platform and environment. We just verify it
    /// doesn't panic and returns a recognizable result.
    #[test]
    fn test_check_sandbox_prerequisites_returns_result() {
        let result = check_sandbox_prerequisites();
        // It should return either Ok(()) or a typed SandboxError—never panic.
        match result {
            Ok(()) => {}
            Err(e) => {
                // The error must be a valid SandboxError variant.
                let msg = e.to_string();
                assert!(!msg.is_empty(), "SandboxError message must be non-empty");
            }
        }
    }

    /// test_sandbox_exec_available on non-macOS returns Ok(()) unconditionally.
    /// On macOS it calls sandbox-exec and returns Ok or a SandboxError.
    #[test]
    fn test_test_sandbox_exec_available_returns_result() {
        let result = test_sandbox_exec_available();
        match result {
            Ok(()) => {}
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    !msg.is_empty(),
                    "Error from test_sandbox_exec_available must be non-empty"
                );
            }
        }
    }

    /// Verify that `SandboxError::LandlockNotAvailable` message is actionable.
    #[test]
    fn test_landlock_error_message_actionable() {
        let err = SandboxError::LandlockNotAvailable;
        let msg = err.to_string();
        assert!(
            msg.contains("--no-sandbox"),
            "Error should advise --no-sandbox: {msg}"
        );
    }

    /// Verify that `SandboxError::MacOSSandboxNotAvailable` message is actionable.
    #[test]
    fn test_macos_sandbox_error_message_actionable() {
        let err = SandboxError::MacOSSandboxNotAvailable;
        let msg = err.to_string();
        assert!(
            msg.contains("--no-sandbox"),
            "Error should advise --no-sandbox: {msg}"
        );
    }

    /// Verify that `SandboxError::UnsupportedOs` includes the OS name.
    #[test]
    fn test_unsupported_os_error_includes_name() {
        let err = SandboxError::UnsupportedOs("plan9".to_string());
        let msg = err.to_string();
        assert!(msg.contains("plan9"), "Error should name the OS: {msg}");
    }
}
