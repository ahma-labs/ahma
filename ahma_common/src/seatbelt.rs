//! Whether a process runs inside a macOS Seatbelt sandbox, asked of the
//! kernel (`sandbox_check`). One declaration for every caller: the doctor,
//! the nested-sandbox check and the daemon finder each declared the call
//! themselves.

/// Whether process `pid` runs inside a Seatbelt sandbox. Any pid may be
/// asked; a process inherits its parent's profile for life. Always `false`
/// off macOS.
#[cfg(target_os = "macos")]
pub fn pid_is_confined(pid: u32) -> bool {
    // The `type` argument this code has always passed; with a NULL operation
    // the kernel only answers whether the pid is sandboxed at all.
    const SANDBOX_CHECK_NO_REPORT: std::ffi::c_int = 0x0002;
    unsafe extern "C" {
        // libsystem_sandbox: `int sandbox_check(pid_t pid, const char *operation,
        // int type, ...)`: 1 when confined, 0 when not, -1 on error. `pid_t` is
        // an `int` on macOS.
        fn sandbox_check(
            pid: std::ffi::c_int,
            operation: *const std::ffi::c_char,
            type_: std::ffi::c_int,
            ...
        ) -> std::ffi::c_int;
    }
    let Ok(pid) = std::ffi::c_int::try_from(pid) else {
        return false;
    };
    // SAFETY: a NULL operation asks only whether the pid is sandboxed; no
    // pointer is retained.
    unsafe { sandbox_check(pid, std::ptr::null(), SANDBOX_CHECK_NO_REPORT) == 1 }
}

#[cfg(not(target_os = "macos"))]
pub fn pid_is_confined(_pid: u32) -> bool {
    false
}

/// Whether this process runs inside a Seatbelt sandbox.
pub fn this_process_is_confined() -> bool {
    pid_is_confined(std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_and_its_pid_get_one_answer() {
        assert_eq!(
            this_process_is_confined(),
            pid_is_confined(std::process::id())
        );
        // No process 0 to be confined in.
        assert!(!pid_is_confined(u32::MAX));
    }
}
