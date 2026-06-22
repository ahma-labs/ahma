use super::fs::get_workspace_dir;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

/// Cached binary paths to avoid redundant builds across tests.
/// Key: (package, binary) tuple as string "package:binary"
static BINARY_CACHE: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();

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

/// Decide whether `binary_path` must be (re)built: true if it is missing, or if
/// any workspace source file is newer than the binary. When the source mtime
/// can't be determined, an existing binary is trusted (returns false) so we
/// never rebuild — and therefore never change its feature set — under a binary
/// CI deliberately built (e.g. `--no-default-features`).
fn binary_needs_build(binary_path: &Path) -> bool {
    let Ok(bin_mtime) = binary_path.metadata().and_then(|m| m.modified()) else {
        return true; // missing or unreadable → build
    };
    match newest_source_mtime() {
        Some(src_mtime) => bin_mtime < src_mtime,
        None => false, // can't tell → trust the existing binary
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
/// turns off the `cluster`/`vault`/`simplify` default features) before running
/// the suite; an unconditional `cargo build` would silently rebuild it with
/// *default* features and flip feature-gated behavior (the cluster CLI test
/// stops skipping and runs against an unintended build). Because CI's binary is
/// always built last, the mtime gate treats it as fresh and leaves it untouched.
///
/// When a rebuild *is* needed we build by **bin name only** (`cargo build --bin
/// <bin>`): bin names are unique across the workspace, whereas callers pass the
/// `package` arg inconsistently (`ahma_bin`, `ahma`, even `ahma_mcp`) — and it
/// has always been ignored for path resolution too (see [`get_binary_path`]).
/// The result is cached so later calls in the same process skip the check, and
/// the whole operation is serialized under the cache mutex so parallel callers
/// wait for a single build instead of racing.
pub fn build_binary_cached(package: &str, binary: &str) -> PathBuf {
    let cache = BINARY_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = format!("{}:{}", package, binary);
    let binary_path = get_binary_path(package, binary);

    // Hold the lock across the freshness check + build: concurrent callers all
    // want the SAME fresh binary, so serializing avoids redundant/racing builds.
    let mut cache_guard = cache.lock().unwrap();
    if cache_guard.contains_key(&key) {
        return binary_path;
    }

    if !binary_needs_build(&binary_path) {
        cache_guard.insert(key, binary_path.clone());
        return binary_path;
    }

    let workspace = get_workspace_dir();
    let output = Command::new("cargo")
        .current_dir(&workspace)
        .args(["build", "--bin", binary])
        .output()
        .expect("Failed to run cargo build");

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
pub fn test_command(binary: &Path) -> Command {
    let mut cmd = Command::new(binary);
    cmd.args(["--no-sandbox", "--skip-probes"]);
    cmd
}
