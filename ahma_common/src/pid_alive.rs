//! Whether a process id belongs to a running process: the one liveness probe
//! every grant bounded by its owner's life uses (SPEC R-PERM.4.4, R-LINK.4,
//! R-CRED.3), on every platform.
//!
//! There were three, and the one session grants used answered `true` for
//! every pid off Unix, so on Windows a session grant outlived the harness
//! that owned it until the age cap.
//!
//! PID reuse can make a dead owner look alive; the consequence is that its
//! grant is swept one check later, never that a live owner's grant is
//! dropped.

/// Whether `pid` is held by a running process. `0` is never one process.
pub fn pid_alive(pid: u32) -> bool {
    pid != 0 && imp::alive(pid)
}

#[cfg(unix)]
mod imp {
    use std::ffi::c_int;

    unsafe extern "C" {
        /// `int kill(pid_t pid, int sig)`; with `sig` 0 it sends nothing and
        /// only says whether `pid` could be signalled.
        fn kill(pid: c_int, sig: c_int) -> c_int;
    }

    /// `EPERM`: the same value on every Unix ahma builds for.
    const EPERM: i32 = 1;

    /// `kill(pid, 0)` succeeds, or is refused with `EPERM` (another user's
    /// process, or one a sandbox may not signal): either way it exists.
    pub(super) fn alive(pid: u32) -> bool {
        // A pid above `c_int::MAX` would be read as a process group.
        let Ok(pid) = c_int::try_from(pid) else {
            return false;
        };
        // SAFETY: signal 0 performs only the existence and permission check;
        // nothing is sent.
        let signalled = unsafe { kill(pid, 0) } == 0;
        signalled || std::io::Error::last_os_error().raw_os_error() == Some(EPERM)
    }
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{CloseHandle, FALSE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    /// `STILL_ACTIVE` (`STATUS_PENDING`): the exit code Windows reports for a
    /// process that has not exited.
    const STILL_ACTIVE: u32 = 259;

    /// The process opens, and has no exit code yet.
    pub(super) fn alive(pid: u32) -> bool {
        // SAFETY: the handle is checked for null and closed before returning.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
            if handle.is_null() {
                return false;
            }
            let mut code: u32 = 0;
            let running = GetExitCodeProcess(handle, &mut code) != FALSE && code == STILL_ACTIVE;
            let _ = CloseHandle(handle);
            running
        }
    }
}

#[cfg(test)]
mod tests {
    use super::pid_alive;

    #[test]
    fn this_process_is_alive() {
        assert!(pid_alive(std::process::id()));
    }

    /// SPEC R-PERM.4.4: a grant bounded by its owner ends with it, on every
    /// platform. A child that has exited and been waited for is not alive.
    #[test]
    fn a_process_that_has_exited_is_not_alive() {
        // This test binary, asked to run a test that does not exist: it exits
        // at once, on every platform, with no other program needed.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "no_such_test_ahma_pid_alive"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!pid_alive(pid), "pid {pid} has exited");
    }

    #[test]
    fn pid_zero_is_never_one_process() {
        assert!(!pid_alive(0));
    }
}
