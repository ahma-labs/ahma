use super::fs::get_workspace_dir;
use ahma_common::fs_lock::FsLock;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::SystemTime;

/// Cached binary paths to avoid redundant builds across tests.
/// Key: (package, binary) tuple as string "package:binary"
///
/// This is a **per-process** fast-path: it skips the filesystem lock entirely
/// for repeated calls within a single test process.  Cross-process
/// serialisation is handled by [`FsLock`] below.
static BINARY_CACHE: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();

/// Set once, in the *test process itself*, so that every ahma binary the test
/// spawns inherits it — however it is spawned.
///
/// Marking only [`test_command`] is not enough: 17 integration tests build their
/// own `Command`, and any new test may do the same. But all of them must first
/// call [`build_binary_cached`] to locate the binary, and a child process
/// inherits its parent's environment — so setting the marker here covers every
/// spawn path, present and future, without touching a single test.
///
/// Why it matters: an unmarked test-spawned ahma resolves the machine-global
/// bridge socket, sees a different `BUILD_ID` (it was just rebuilt), concludes
/// the running bridge is stale, and POSTs `/restart` — killing the developer's
/// live MCP server, and any other application sharing that socket.
static TEST_ISOLATION_MARKER: std::sync::Once = std::sync::Once::new();

fn mark_process_test_isolated() {
    TEST_ISOLATION_MARKER.call_once(|| {
        // SAFETY: run exactly once, before this process spawns any ahma binary.
        // nextest gives each test binary its own process, so the write is not
        // visible to — and cannot race with — any other test process.
        unsafe { std::env::set_var("AHMA_TEST_ISOLATION", "1") };
    });
}

/// Newest mtime among workspace source files (`*.rs`, `Cargo.toml`, `Cargo.lock`),
/// computed once per process. Used to decide whether a spawned binary is stale.
static NEWEST_SOURCE_MTIME: OnceLock<Option<SystemTime>> = OnceLock::new();

/// Walk the workspace (skipping `target`/`.git`/`node_modules`) and return the
/// most recent modification time across files that affect a binary build. The
/// result is cached for the life of the process — source does not change while a
/// test run is in flight.
fn newest_source_mtime() -> Option<SystemTime> {
    *NEWEST_SOURCE_MTIME.get_or_init(|| {
        let mut newest: Option<SystemTime> = None;
        let mut stack = vec![get_workspace_dir()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                let name = entry.file_name();
                if file_type.is_dir() {
                    if name == "target" || name == ".git" || name == "node_modules" {
                        continue;
                    }
                    stack.push(entry.path());
                } else if file_type.is_file() {
                    let path = entry.path();
                    let relevant = path.extension().is_some_and(|e| e == "rs")
                        || name == "Cargo.toml"
                        || name == "Cargo.lock";
                    if relevant && let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
                        newest = Some(newest.map_or(modified, |n| n.max(modified)));
                    }
                }
            }
        }
        newest
    })
}

/// Why a binary must be (re)built — carried into the rebuild log line so a
/// test that spends its timeout budget inside a surprise rebuild is
/// diagnosable from the captured output alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StaleReason {
    /// No binary at the expected path (fresh checkout, `cargo clean`, or a
    /// build interrupted mid-write — e.g. by a full disk).
    Missing,
    /// A workspace source file is newer than the binary.
    OlderThanSource,
}

impl StaleReason {
    fn describe(self) -> &'static str {
        match self {
            StaleReason::Missing => "missing",
            StaleReason::OlderThanSource => "older than the newest workspace source file",
        }
    }
}

/// Decide whether `binary_path` must be (re)built and why: `Missing` if there
/// is no readable binary, `OlderThanSource` if any workspace source file is
/// newer. When the source mtime can't be determined, an existing binary is
/// trusted (returns `None`) so we never rebuild — and therefore never change
/// its feature set — under a binary CI deliberately built (e.g.
/// `--no-default-features`).
fn stale_reason(binary_path: &Path) -> Option<StaleReason> {
    let Ok(bin_mtime) = binary_path.metadata().and_then(|m| m.modified()) else {
        return Some(StaleReason::Missing); // missing or unreadable → build
    };
    match newest_source_mtime() {
        Some(src_mtime) if bin_mtime < src_mtime => Some(StaleReason::OlderThanSource),
        _ => None, // fresh, or can't tell → trust the existing binary
    }
}

/// Get the path to a binary in the target directory, resolving CARGO_TARGET_DIR correctly.
///
/// This function handles relative `CARGO_TARGET_DIR` paths (e.g., `target`) by resolving
/// them relative to the workspace root. This is critical for CI environments that set
/// `CARGO_TARGET_DIR` to a relative path.
///
/// Does NOT build the binary - caller is responsible for ensuring it exists.
/// For automatic building with caching, use `build_binary_cached()` instead.
pub fn get_binary_path(_package: &str, binary: &str) -> PathBuf {
    let workspace = get_workspace_dir();
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|p| {
            if p.is_absolute() {
                p
            } else {
                workspace.join(p)
            }
        })
        .unwrap_or_else(|_| workspace.join("target"));

    // On Windows, executables have a `.exe` extension; std::env::consts::EXE_EXTENSION
    // is "exe" on Windows and "" on Unix, so appending it is always safe.
    let mut name = binary.to_string();
    if !std::env::consts::EXE_EXTENSION.is_empty() {
        name.push('.');
        name.push_str(std::env::consts::EXE_EXTENSION);
    }

    target_dir.join("debug").join(name)
}

/// Get or build a binary, ensuring it is **up to date**, then cache it.
///
/// Integration tests spawn the real `ahma` binary as a subprocess. Cargo builds
/// the *test* crates but has no dependency edge to that separate binary artifact,
/// so a stale `target/debug/ahma` (e.g. left over from before a version bump or
/// source change) would silently be exercised by every test — the exact skew
/// that made `test_health_check_version_and_restart` fail after `/ahmadev bump`.
///
/// To eliminate that class of bug, the first call per process **rebuilds the
/// binary only when it is stale** — missing, or older than the newest workspace
/// source file (see [`binary_needs_build`]). A fresh binary is used as-is.
///
/// Critically, we do **not** rebuild a binary that is already up to date. CI
/// builds the binary with specific flags (e.g. `--no-default-features`, which
/// turns off the `vault`/`simplify` default features) before running
/// the suite; an unconditional `cargo build` would silently rebuild it with
/// *default* features and flip feature-gated behavior. Because CI's binary is
/// always built last, the mtime gate treats it as fresh and leaves it untouched.
///
/// When a rebuild *is* needed we build by **bin name only** (`cargo build --bin
/// <bin>`): bin names are unique across the workspace, whereas callers pass the
/// `package` arg inconsistently (`ahma_bin`, `ahma`, even `ahma_mcp`) — and it
/// has always been ignored for path resolution too (see [`get_binary_path`]).
///
/// ## Cross-process serialisation
///
/// `cargo nextest` runs each test in its own OS process, so an in-memory lock
/// cannot prevent N processes from concurrently spawning `cargo build` for the
/// same binary.  This function therefore uses **two layers** of locking:
///
/// 1. **Per-process fast-path** (`BINARY_CACHE`): an in-memory `OnceLock` that
///    skips everything — including the filesystem lock — for repeated calls
///    within the same process.
///
/// 2. **Cross-process serialisation** ([`FsLock`]): an OS-level advisory file
///    lock on `<target>/debug/<binary>.build-lock`.  Only one process at a time
///    proceeds past the lock; losers block and then re-check freshness (the
///    winner will have already built it).  The lock is released automatically
///    when the `FsLock` drops or the process exits for any reason — including
///    panic, `SIGKILL`, or crash.  No stale locks, no manual cleanup.
pub fn build_binary_cached(package: &str, binary: &str) -> PathBuf {
    mark_process_test_isolated();

    let cache = BINARY_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = format!("{}:{}", package, binary);
    let binary_path = get_binary_path(package, binary);

    // ── Fast-path: per-process in-memory cache ──────────────────────────
    // If this process has already verified/built this binary, skip everything.
    {
        let cache_guard = cache.lock();
        if cache_guard.contains_key(&key) {
            return binary_path;
        }
    }

    // ── Cross-process serialisation via filesystem advisory lock ─────────
    // The lockfile lives next to the binary so `cargo clean` removes it.
    let lock_path = binary_path.with_extension("build-lock");
    let _fs_lock = FsLock::acquire(&lock_path).unwrap_or_else(|e| {
        // If we can't acquire the lock (e.g. read-only filesystem), fall
        // through without cross-process protection — the build is still
        // correct, just potentially redundant.
        eprintln!(
            "warning: could not acquire build lock {}: {e} (proceeding without cross-process serialisation)",
            lock_path.display()
        );
        // Return a lock on a temp file so the type works out — it's
        // released immediately but that's fine, we're in degraded mode.
        FsLock::acquire(&std::env::temp_dir().join(format!("ahma-build-{binary}.lock")))
            .expect("fallback lock in temp dir should always succeed")
    });

    // Re-check under the filesystem lock: another process may have built
    // the binary while we were waiting.
    let mut cache_guard = cache.lock();
    if cache_guard.contains_key(&key) {
        return binary_path;
    }

    let Some(reason) = stale_reason(&binary_path) else {
        cache_guard.insert(key, binary_path.clone());
        return binary_path;
    };

    // Be LOUD: this rebuild runs inside whichever test happened to call the
    // harness first, so its cost counts against that test's timeout budget.
    // Without these lines a cold or damaged `target/` shows up as a random
    // integration test timing out with no indication why.
    eprintln!(
        "[ahma test harness] binary `{}` is {} — rebuilding it now, inside the \
         current test's timeout budget. If this test times out here, the rebuild \
         is the cause, not the test; prebuild with `cargo build --bin {}` and re-run.",
        binary,
        reason.describe(),
        binary
    );
    let rebuild_started = std::time::Instant::now();

    let workspace = get_workspace_dir();
    let output = Command::new("cargo")
        .current_dir(&workspace)
        .args(["build", "--bin", binary])
        .output()
        .expect("Failed to run cargo build");

    eprintln!(
        "[ahma test harness] rebuild of `{}` finished in {:.1}s (success: {})",
        binary,
        rebuild_started.elapsed().as_secs_f64(),
        output.status.success()
    );

    // If the build fails but a binary is already present, prefer not to break the
    // test on a transient/edge build issue — use what's there. Only a missing
    // binary is fatal.
    if !output.status.success() && !binary_path.exists() {
        panic!(
            "Failed to build bin `{}` and no existing binary at {}: {}",
            binary,
            binary_path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        binary_path.exists(),
        "cargo build of `{}` reported success but no binary was found at {}",
        binary,
        binary_path.display()
    );

    cache_guard.insert(key, binary_path.clone());
    binary_path
}

/// Create a command for a binary with test mode enabled (bypasses sandbox checks).
/// Disables the sandbox and skips probes via environment variables.
/// The caller must add the appropriate subcommand (e.g., `serve stdio`, `run`, `tool list`).
///
/// `AHMA_TEST_ISOLATION` marks the spawned binary as test-owned. Without it the
/// child resolves the machine-global bridge endpoint (`/tmp/ahma.sock`), and —
/// because a freshly built test binary carries a different `BUILD_ID` — decides
/// the running bridge is stale and restarts it. That bridge is the developer's
/// live MCP server (or another application's), so a test run would tear down a
/// session it has nothing to do with. `cfg!(test)` cannot cover this: the child
/// is an ordinary binary, not a test harness.
///
/// Under `cargo nextest` the inherited `NEXTEST` variable now provides the same
/// isolation as a fail-closed backstop (SPEC R-ISO.1), but this explicit
/// variable remains authoritative: plain `cargo test` sets nothing comparable.
pub fn test_command(binary: &Path) -> Command {
    let mut cmd = Command::new(binary);
    cmd.args(["--no-sandbox", "--skip-probes"]);
    cmd.env("AHMA_TEST_ISOLATION", "1");
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locating the binary must mark the process test-isolated, because a child
    /// inherits its parent's environment. This is what protects the ~17 tests
    /// that spawn ahma with a raw `Command::new` instead of [`test_command`] —
    /// without it, they restart the developer's live bridge.
    #[test]
    fn build_binary_cached_marks_the_process_test_isolated() {
        mark_process_test_isolated();
        assert_eq!(
            std::env::var("AHMA_TEST_ISOLATION").ok().as_deref(),
            Some("1"),
            "the marker must be set in the test process so every spawned ahma inherits it"
        );
    }

    /// The harness-built command carries the marker explicitly too (belt and braces).
    #[test]
    fn test_command_carries_the_isolation_marker() {
        let cmd = test_command(Path::new("/nonexistent/ahma"));
        let marked = cmd
            .get_envs()
            .any(|(k, v)| k == "AHMA_TEST_ISOLATION" && v == Some("1".as_ref()));
        assert!(marked, "test_command must mark the child as test-isolated");
    }

    #[test]
    fn stale_reason_missing_binary() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("no-such-binary");
        assert_eq!(stale_reason(&path), Some(StaleReason::Missing));
    }

    #[test]
    fn stale_reason_fresh_binary_is_trusted() {
        // A file created now is newer than every workspace source file, so it
        // must be trusted as-is (this is what protects CI's
        // `--no-default-features` binary from a feature-flipping rebuild).
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("fresh-binary");
        std::fs::write(&path, b"bin").unwrap();
        assert_eq!(stale_reason(&path), None);
    }

    #[test]
    fn stale_reason_old_binary_needs_rebuild() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("old-binary");
        std::fs::write(&path, b"bin").unwrap();
        // Backdate the binary to well before any workspace source file.
        let epoch = std::fs::FileTimes::new()
            .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1));
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(epoch)
            .unwrap();
        assert_eq!(stale_reason(&path), Some(StaleReason::OlderThanSource));
    }

    #[test]
    fn stale_reasons_describe_themselves() {
        assert_eq!(StaleReason::Missing.describe(), "missing");
        assert!(StaleReason::OlderThanSource.describe().contains("older"));
    }
}
