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
