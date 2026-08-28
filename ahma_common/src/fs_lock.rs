//! Cross-process advisory file lock.
//!
//! Provides [`FsLock`]: a thin wrapper around [`std::fs::File::lock()`] (stable
//! since Rust 1.84) that gives exclusive, OS-enforced, cross-process locking
//! with **automatic cleanup on process death**.
//!
//! # Why this exists
//!
//! In-memory locks (`Mutex`, `Semaphore`, `OnceLock`) only serialise within a
//! single OS process.  Several ahma subsystems need cross-process coordination:
//!
//! * **Test infrastructure** ([`build_binary_cached`]): `cargo nextest` runs
//!   each test in its own process, so an in-memory cache cannot prevent N
//!   processes from spawning N concurrent `cargo build` subprocesses for the
//!   same binary.
//!
//! * **Command mutex groups** ([`CommandMutexRegistry`]): each MCP session,
//!   hook invocation, and TUI instance creates its own in-memory registry,
//!   so two sessions on the same workspace can run `cargo build` concurrently
//!   — the exact contention the registry was designed to prevent.
//!
//! [`FsLock`] solves both by delegating to the kernel's file-locking subsystem
//! (`flock` on Unix, `LockFileEx` on Windows).
//!
//! # Stale lock safety
//!
//! The lock is **not** stored in the file — the file is just a rendezvous
//! point.  The lock state lives in the kernel, tied to the file descriptor.
//! The OS releases it automatically when the owning process exits, panics,
//! is killed (`SIGKILL`), or crashes.  **There is no stale-lock scenario**
//! and no manual cleanup is ever required.  The empty lockfile on disk can
//! be deleted at any time (e.g. by `cargo clean`) without consequence — the
//! next caller simply recreates it.

use std::{
    fs::{self, File},
    io,
    path::Path,
};

/// An exclusive, cross-process advisory file lock.
///
/// Acquired with [`FsLock::acquire`] (blocking) or [`FsLock::try_acquire`]
/// (non-blocking).  Released automatically when dropped or when the process
/// exits for any reason (including panic, `SIGKILL`, crash).
///
/// The lockfile on disk is an empty file used as a rendezvous point.  It does
/// not "hold" the lock — the kernel does.  Deleting the file while no process
/// holds a lock is harmless; the next caller recreates it.
#[derive(Debug)]
pub struct FsLock {
    _file: File,
}

impl FsLock {
    /// Acquire an exclusive lock on the file at `lock_path`, blocking until
    /// available.  Creates the lockfile (and parent directories) if they do
    /// not exist.
    pub fn acquire(lock_path: &Path) -> io::Result<Self> {
        let file = Self::open_or_create(lock_path)?;
        file.lock()?;
        Ok(Self { _file: file })
    }

    /// Try to acquire an exclusive lock without blocking.
    ///
    /// Returns `Ok(Some(lock))` on success, `Ok(None)` if another process
    /// already holds the lock, or `Err` on I/O failure.
    pub fn try_acquire(lock_path: &Path) -> io::Result<Option<Self>> {
        let file = Self::open_or_create(lock_path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(e),
        }
    }

    /// Open or create the lockfile.  Creates parent directories as needed.
    fn open_or_create(lock_path: &Path) -> io::Result<File> {
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)?;
        }
        File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
    }
}

// Drop is implicit: File::drop closes the fd, which releases the flock.

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    #[test]
    fn acquire_and_drop_releases_lock() {
        let td = TempDir::new().unwrap();
        let lock_path = td.path().join("test.lock");

        // Acquire, then drop.
        let lock = FsLock::acquire(&lock_path).unwrap();
        drop(lock);

        // Should be able to re-acquire immediately.
        let _lock2 = FsLock::acquire(&lock_path).unwrap();
    }

    #[test]
    fn try_acquire_returns_none_when_held() {
        let td = TempDir::new().unwrap();
        let lock_path = td.path().join("test.lock");

        let _held = FsLock::acquire(&lock_path).unwrap();
        let result = FsLock::try_acquire(&lock_path).unwrap();
        assert!(result.is_none(), "should return None when lock is held");
    }

    #[test]
    fn try_acquire_succeeds_when_free() {
        let td = TempDir::new().unwrap();
        let lock_path = td.path().join("test.lock");

        let result = FsLock::try_acquire(&lock_path).unwrap();
        assert!(result.is_some(), "should succeed when no lock is held");
    }

    #[test]
    fn creates_parent_directories() {
        let td = TempDir::new().unwrap();
        let lock_path = td.path().join("a").join("b").join("deep.lock");

        let _lock = FsLock::acquire(&lock_path).unwrap();
        assert!(lock_path.exists());
    }

    #[test]
    fn cross_process_serialisation() {
        if std::env::var("AHMA_TEST_LOCK_HOLDER").is_ok() {
            let lock_path = std::path::PathBuf::from(std::env::var("AHMA_TEST_LOCK_PATH").unwrap());
            let _lock = FsLock::acquire(&lock_path).unwrap();
            println!("ACQUIRED");
            std::thread::sleep(Duration::from_millis(500));
            return;
        }

        let td = TempDir::new().unwrap();
        let lock_path = td.path().join("cross.lock");

        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.arg("fs_lock::tests::cross_process_serialisation")
            .arg("--nocapture")
            .env("AHMA_TEST_LOCK_HOLDER", "1")
            .env("AHMA_TEST_LOCK_PATH", &lock_path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        // Clear nextest environment variables that the child would inherit,
        // preventing nextest's test runner hooks from trying to list tests or
        // communicate with the parent nextest process via inherited pipes.
        for (key, _) in std::env::vars() {
            if key.starts_with("NEXTEST") {
                cmd.env_remove(&key);
            }
        }

        let mut child = cmd.spawn().unwrap();

        // Read stdout and stderr from the child to see what happened
        use std::io::{BufRead, BufReader};
        let stdout = child.stdout.take().unwrap();
        let reader = BufReader::new(stdout);
        let mut found = false;
        let mut child_output = String::new();
        for line in reader.lines().map_while(Result::ok) {
            child_output.push_str(&line);
            child_output.push('\n');
            if line.contains("ACQUIRED") {
                found = true;
                break;
            }
        }

        if !found {
            // Wait for child to exit and check output/status
            let status = child.wait().unwrap();
            panic!(
                "Child failed to acquire lock or output progress. Status: {:?}, Output:\n{}",
                status, child_output
            );
        }

        let start = Instant::now();
        let _lock = FsLock::acquire(&lock_path).unwrap();
        let elapsed = start.elapsed();

        // We should have blocked for roughly the child's remaining sleep time.
        assert!(
            elapsed >= Duration::from_millis(200),
            "expected to block, but only blocked {elapsed:?}"
        );

        let _ = child.wait();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Stale socket removal
// ─────────────────────────────────────────────────────────────────────────────

/// Unlink a Unix domain socket path, ignoring any failure.
///
/// **Why this is a sync `std::fs` call inside `async fn`s, deliberately.**
/// AGENTS.md forbids blocking I/O in an async function, and rightly: a
/// filesystem call that can block the reactor for milliseconds starves every
/// other task on that worker. This is the one shape where the rule's reasoning
/// does not apply, so it is expressed once, here, with the reasoning attached —
/// rather than as six bare `std::fs::remove_file` calls that read as violations
/// and would be cited as precedent by the next person who wants one.
///
/// Three reasons it is exempt:
///
/// 1. `unlink(2)` on a socket inode is a single metadata operation, on the order
///    of microseconds — faster than the `spawn_blocking` hop `tokio::fs` would
///    use to "avoid blocking".
/// 2. Every caller is on a shutdown or bind-retry path, where there is no
///    concurrent work left to starve.
/// 3. Two callers run immediately before `std::process::exit(0)`. An `.await`
///    there can only be reached if the runtime is still scheduling, which is
///    exactly what is being torn down.
///
/// Failure is ignored because the only outcomes are "already gone" (fine) and
/// "cannot remove" — in which case the next bind's stale-socket retry handles it,
/// and failing shutdown over it would be worse.
pub fn remove_stale_socket(path: impl AsRef<std::path::Path>) {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(path.as_ref());
    }
    #[cfg(not(unix))]
    {
        // No filesystem entry to unlink: Windows named pipes are removed by the
        // kernel when the last handle closes.
        let _ = path.as_ref();
    }
}

#[cfg(test)]
mod stale_socket_tests {
    use super::remove_stale_socket;

    #[test]
    fn removing_a_missing_path_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        // The common case on a clean start; it must be silent, not a panic.
        remove_stale_socket(dir.path().join("never-existed.sock"));
    }

    #[test]
    fn an_existing_file_is_removed_on_unix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale.sock");
        std::fs::write(&path, b"").unwrap();
        remove_stale_socket(&path);
        #[cfg(unix)]
        assert!(!path.exists(), "a stale socket path must be unlinked");
        #[cfg(not(unix))]
        assert!(path.exists(), "the non-Unix arm is a documented no-op");
    }
}
