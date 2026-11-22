//! Questions about macOS Seatbelt, asked of the kernel (`sandbox_check`). One
//! declaration of the call, and one way to spell its flags, for every caller:
//! whether a process is sandboxed at all (the doctor, the nested-sandbox check,
//! the daemon finder) and whether its profile lets it register a Mach name
//! (`ahma_mcp`'s leftover finder, which marks each run's profile that way).

#[cfg(target_os = "macos")]
mod ffi {
    use std::ffi::{c_char, c_int};

    /// `SANDBOX_FILTER_NONE`: no filter argument follows `type`.
    pub const SANDBOX_FILTER_NONE: c_int = 0;
    /// `SANDBOX_FILTER_GLOBAL_NAME`: one `const char *` Mach service name follows
    /// `type`.
    pub const SANDBOX_FILTER_GLOBAL_NAME: c_int = 2;

    unsafe extern "C" {
        /// libsystem_sandbox's private interface; no SDK header declares it, so
        /// this follows the declaration WebKit's `SandboxSPI.h` carries:
        /// `int sandbox_check(pid_t pid, const char *operation,
        /// enum sandbox_filter_type type, ...)`. With a NULL `operation` it says
        /// whether `pid` is sandboxed at all (1 yes, 0 no); with an operation,
        /// whether the profile allows it (0 allowed). `pid_t` and the enum are
        /// both `int` on macOS.
        pub fn sandbox_check(pid: c_int, operation: *const c_char, type_: c_int, ...) -> c_int;

        /// `extern const enum sandbox_filter_type SANDBOX_CHECK_NO_REPORT`: a
        /// flag OR-ed into `type` so a denied check is not logged as a
        /// violation. Apple exports it as a constant rather than a documented
        /// value, so it is read from the library: it is a high bit
        /// (`0x4000_0000` on Darwin 27), clear of every filter type. The literal
        /// `0x0002` once written here for it is `SANDBOX_FILTER_GLOBAL_NAME`.
        pub safe static SANDBOX_CHECK_NO_REPORT: c_int;
    }

    /// `type` for "is this process sandboxed at all": no filter, no report.
    pub fn confinement_query() -> c_int {
        SANDBOX_FILTER_NONE | SANDBOX_CHECK_NO_REPORT
    }

    /// `EPERM` on Darwin (`<sys/errno.h>`).
    const EPERM: c_int = 1;

    unsafe extern "C" {
        /// `int kill(pid_t pid, int sig)`; with `sig` 0 it sends nothing and
        /// only says whether `pid` could be signalled.
        fn kill(pid: c_int, sig: c_int) -> c_int;
    }

    /// Whether a process holds `pid`: `kill(pid, 0)` succeeds, or is refused
    /// with `EPERM` (another user's process, or one a sandbox may not
    /// signal). A pid of 0 or below names a process group, never one process.
    pub fn pid_exists(pid: c_int) -> bool {
        // SAFETY: signal 0 performs only the existence and permission check;
        // nothing is sent.
        pid > 0
            && (unsafe { kill(pid, 0) } == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(EPERM))
    }
}

/// Whether process `pid` runs inside a Seatbelt sandbox. Any pid may be
/// asked; a process inherits its parent's profile for life. Always `false`
/// off macOS, and for a pid no process holds.
#[cfg(target_os = "macos")]
pub fn pid_is_confined(pid: u32) -> bool {
    let Ok(pid) = std::ffi::c_int::try_from(pid) else {
        return false;
    };
    // SAFETY: a NULL operation takes no filter argument; no pointer is retained.
    let confined =
        unsafe { ffi::sandbox_check(pid, std::ptr::null(), ffi::confinement_query()) == 1 };
    // `sandbox_check` answers 1 for a pid no process holds too, so the answer
    // counts only for a process that is there: else `ahma doctor` names an
    // exited (or soon reused) pid as a sandboxed leftover to kill.
    confined && ffi::pid_exists(pid)
}

#[cfg(not(target_os = "macos"))]
pub fn pid_is_confined(_pid: u32) -> bool {
    false
}

/// Whether this process runs inside a Seatbelt sandbox.
pub fn this_process_is_confined() -> bool {
    pid_is_confined(std::process::id())
}

/// Whether the kernel would let process `pid` register the Mach global service
/// name `name` (`mach-register`), asked without logging a denial. A process in
/// no sandbox may register any name; `false` also when the question cannot be
/// put (a `name` holding a NUL, a `pid` out of range).
#[cfg(target_os = "macos")]
pub fn pid_may_register_mach_name(pid: u32, name: &str) -> bool {
    let (Ok(pid), Ok(name)) = (std::ffi::c_int::try_from(pid), std::ffi::CString::new(name)) else {
        return false;
    };
    // SAFETY: NUL-terminated strings that outlive the call, and exactly the one
    // `const char *` SANDBOX_FILTER_GLOBAL_NAME announces; nothing is retained.
    unsafe {
        ffi::sandbox_check(
            pid,
            c"mach-register".as_ptr(),
            ffi::SANDBOX_FILTER_GLOBAL_NAME | ffi::SANDBOX_CHECK_NO_REPORT,
            name.as_ptr(),
        ) == 0
    }
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

    /// The "is it sandboxed" query passes no filter type and carries the
    /// library's no-report flag. Its old spelling, a literal `0x0002`, was a
    /// filter type (`SANDBOX_FILTER_GLOBAL_NAME`) and no flag at all.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_confinement_query_is_no_filter_and_the_librarys_no_report_flag() {
        let query = ffi::confinement_query();
        assert_ne!(
            ffi::SANDBOX_CHECK_NO_REPORT,
            0,
            "libsystem_sandbox exports the flag"
        );
        assert_eq!(
            ffi::SANDBOX_CHECK_NO_REPORT & 0xffff,
            0,
            "a flag bit above every filter type, not one of them: {:#x}",
            ffi::SANDBOX_CHECK_NO_REPORT
        );
        assert_eq!(query & 0xffff, ffi::SANDBOX_FILTER_NONE, "{query:#x}");
        assert_ne!(query & ffi::SANDBOX_CHECK_NO_REPORT, 0, "{query:#x}");
    }

    /// The shared query answers as the call it replaced did. launchd (pid 1)
    /// is in no sandbox whoever asks, so it is the unconfined case even when
    /// this suite itself runs inside a Seatbelt profile (an agent's ahma
    /// sandbox), where asserting this process unconfined would be wrong.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_shared_query_agrees_with_the_call_it_replaced() {
        let previous = |pid: u32| {
            let pid = std::ffi::c_int::try_from(pid).unwrap();
            // SAFETY: as `pid_is_confined`, with the type it used to pass.
            unsafe { ffi::sandbox_check(pid, std::ptr::null(), 0x0002) == 1 }
        };
        assert!(!pid_is_confined(1), "launchd runs in no sandbox");
        assert!(!previous(1));
        let me = std::process::id();
        assert_eq!(pid_is_confined(me), previous(me));
    }

    /// `sandbox_check` answers 1 for a pid no process holds, the same answer
    /// as for a confined one. A pid that has exited is not confined: else
    /// `ahma doctor` names a reused or vanished pid as a sandboxed leftover
    /// and suggests killing it.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_dead_pid_is_not_confined() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!pid_is_confined(pid), "pid {pid} has exited");
    }

    /// Outside a sandbox every Mach name may be registered; a name that cannot
    /// be passed to the kernel is refused rather than truncated.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_unconfined_process_may_register_any_mach_name() {
        let me = std::process::id();
        assert!(!pid_may_register_mach_name(me, "ahma.run.\0none"));
        if this_process_is_confined() {
            eprintln!("nested in a sandbox: the unconfined case runs only outside one");
            return;
        }
        assert!(pid_may_register_mach_name(me, "ahma.run.none"));
    }
}
