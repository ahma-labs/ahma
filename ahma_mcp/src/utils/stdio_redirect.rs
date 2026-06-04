use std::fs::File;
use std::io;
use std::sync::OnceLock;

static SAVED_STDOUT: OnceLock<File> = OnceLock::new();

/// Duplicates the current stdout and redirects stdout (fd 1) to stderr (fd 2).
/// Returns Ok(()) on success, or an I/O error if redirection fails.
/// Safe to call multiple times (subsequent calls are no-ops).
pub fn redirect_stdout_to_stderr() -> io::Result<()> {
    let mut err = None;
    SAVED_STDOUT.get_or_init(|| {
        match do_redirect() {
            Ok(file) => file,
            Err(e) => {
                err = Some(e);
                // Return a dummy File descriptor on error
                // In safety contexts, this is discarded anyway.
                // We'll return the error below.
                #[cfg(unix)]
                unsafe {
                    File::from_raw_fd(-1)
                }
                #[cfg(windows)]
                unsafe {
                    File::from_raw_handle(std::ptr::null_mut())
                }
                #[cfg(not(any(unix, windows)))]
                panic!("Unsupported platform");
            }
        }
    });

    // Clean up if raw file handle helpers are needed
    #[cfg(unix)]
    use std::os::unix::io::FromRawFd;
    #[cfg(windows)]
    use std::os::windows::io::FromRawHandle;

    if let Some(e) = err { Err(e) } else { Ok(()) }
}

/// Returns a clone of the saved stdout File, if redirect_stdout_to_stderr was successfully called.
pub fn get_saved_stdout() -> Option<File> {
    SAVED_STDOUT.get().and_then(|f| f.try_clone().ok())
}

#[cfg(unix)]
fn do_redirect() -> io::Result<File> {
    use std::os::unix::io::FromRawFd;

    let stdout_fd = 1;
    let stderr_fd = 2;

    // Duplicate stdout (fd 1) to get a new saved fd
    let saved_fd = unsafe { libc::dup(stdout_fd) };
    if saved_fd < 0 {
        return Err(io::Error::last_os_error());
    }

    // Redirect stdout (fd 1) to stderr (fd 2)
    if unsafe { libc::dup2(stderr_fd, stdout_fd) } < 0 {
        unsafe { libc::close(saved_fd) };
        return Err(io::Error::last_os_error());
    }

    Ok(unsafe { File::from_raw_fd(saved_fd) })
}

#[cfg(windows)]
fn do_redirect() -> io::Result<File> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{DuplicateHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    unsafe {
        let stdout_handle = GetStdHandle(STD_OUTPUT_HANDLE);
        let stderr_handle = GetStdHandle(STD_ERROR_HANDLE);
        if stdout_handle == INVALID_HANDLE_VALUE || stderr_handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::new(io::ErrorKind::Other, "Invalid std handles"));
        }

        let current_process = GetCurrentProcess();
        let mut saved_handle: HANDLE = std::ptr::null_mut();
        let success = DuplicateHandle(
            current_process,
            stdout_handle as HANDLE,
            current_process,
            &mut saved_handle,
            0,
            0, // FALSE for inherit
            windows_sys::Win32::Foundation::DUPLICATE_SAME_ACCESS,
        );
        if success == 0 {
            return Err(io::Error::last_os_error());
        }

        if SetStdHandle(STD_OUTPUT_HANDLE, stderr_handle) == 0 {
            windows_sys::Win32::Foundation::CloseHandle(saved_handle);
            return Err(io::Error::last_os_error());
        }

        Ok(File::from_raw_handle(saved_handle as *mut std::ffi::c_void))
    }
}

#[cfg(not(any(unix, windows)))]
fn do_redirect() -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Platform not supported",
    ))
}
