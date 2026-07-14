//! # Logging Initialization
//!
//! Centralized logging for ahma processes. Every line is prefixed with `pid=` and
//! `role=` so interleaved multi-process logs in `./logs/ahma.log` remain attributable.

use ahma_common::observability::{ObservabilityConfig, TelemetryGuard};
use anyhow::{Context, Result};
use std::{
    io::stderr,
    path::{Path, PathBuf},
    sync::{Mutex, Once, OnceLock},
};
use tracing_subscriber::{
    EnvFilter,
    fmt::{
        self, FmtContext,
        format::{FormatEvent, Writer},
        time::SystemTime,
    },
    prelude::*,
    registry::LookupSpan,
};

static INIT: Once = Once::new();
/// Passes the OTEL guard out of the `call_once` closure to the caller.
static PENDING_GUARD: Mutex<Option<TelemetryGuard>> = Mutex::new(None);

/// Flush guard for the non-blocking file writer. Held here (not leaked) so
/// [`flush_file_log`] can drop it on abnormal exit, flushing the buffered tail
/// (SPEC R-SIGN.5).
static FILE_LOG_FLUSH_GUARD: Mutex<Option<tracing_appender::non_blocking::WorkerGuard>> =
    Mutex::new(None);

/// Flush the buffered file log by dropping the writer's guard. Idempotent;
/// after this, further log lines may be dropped — call only on the way out
/// (panic hook, abnormal-exit paths).
pub fn flush_file_log() {
    if let Ok(mut guard) = FILE_LOG_FLUSH_GUARD.lock() {
        drop(guard.take());
    }
}

/// Chain a panic hook that flushes the file log after the default hook has
/// printed the panic, so the log's final buffered lines survive an abort
/// (SPEC R-SIGN.5). SIGKILL cannot be hooked; that path is mitigated by the
/// atomic-install requirement (R-SIGN.2) instead.
fn install_panic_flush_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        flush_file_log();
    }));
}

static LOG_ROLE: OnceLock<&'static str> = OnceLock::new();

/// Rolling structured log basename (daily rotation appends `.YYYY-MM-DD`).
pub const MCP_LOG_BASENAME: &str = "ahma.log";

/// Background bridge raw stdout/stderr capture files (see `ahma.log` for structured logs).
pub const BRIDGE_STDOUT_NAME: &str = "ahma_bridge.out.log";
pub const BRIDGE_STDERR_NAME: &str = "ahma_bridge.err.log";

/// One-line header written when bridge capture files are first created.
pub const BRIDGE_CAPTURE_HEADER: &str =
    "# ahma background bridge stdout/stderr capture — see ahma.log for structured logs\n";

/// Delete managed log files older than this many seconds (24 hours).
pub const LOG_RETENTION_SECS: u64 = 24 * 60 * 60;

/// Set the process role label included on every log line. Call once before [`init_logging`].
pub fn set_log_role(role: &'static str) {
    let _ = LOG_ROLE.set(role);
}

/// Current process role (`unknown` if [`set_log_role`] was not called).
pub fn log_role() -> &'static str {
    LOG_ROLE.get().copied().unwrap_or("unknown")
}

/// Infer process role from environment and argv (call before logging init).
pub fn detect_log_role_from_startup() -> &'static str {
    if std::env::var("AHMA_SERVER_CHILD").is_ok() {
        return "bridge";
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--server-child") {
        return "bridge";
    }

    match args.first().map(|s| s.as_str()) {
        None => "cli",
        Some("serve") => match args.get(1).map(|s| s.as_str()) {
            Some("stdio") => "proxy",
            Some("http") | Some("unix") => "bridge",
            // `ahma serve --sandbox-scope …` (background bridge child)
            Some(s) if s.starts_with('-') => "bridge",
            None => "bridge",
            _ => "bridge",
        },
        Some("tui") => "tui",
        Some("daemon") => "daemon",
        Some("update") => "update",
        Some("setup") => "setup",
        Some("hooks") if args.iter().any(|a| a == "run-shell") => "cli",
        Some("tool") if args.get(1).map(|s| s.as_str()) == Some("run") => "cli",
        _ => "cli",
    }
}

/// Process-wide log directory override set from the `--log-dir` CLI flag.
static LOG_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// Log directory derived from the primary sandbox scope after `roots/list`.
/// Lower priority than `--log-dir` / `AHMA_LOG_DIR`, higher than CWD fallback.
static LOG_DIR_FROM_SCOPE: OnceLock<PathBuf> = OnceLock::new();

/// Set the log directory from the `--log-dir` CLI flag.
/// Call once, early in startup, before any logging is initialised.
pub fn set_log_dir_override(dir: PathBuf) {
    let _ = LOG_DIR_OVERRIDE.set(dir);
}

/// Derive the default log directory from the primary sandbox scope.
///
/// Called once when the sandbox scope is established (after `roots/list`).
/// Has no effect if `--log-dir` was already set, or if already called.
/// Note: the tracing file appender opened at startup continues to write to
/// whichever path was used at init time; this affects `logs_list` display
/// and operation spill files created after the scope is locked.
pub fn set_log_dir_from_scope(dir: PathBuf) {
    let _ = LOG_DIR_FROM_SCOPE.set(dir);
}

/// Project log directory, checked in priority order:
/// 1. `--log-dir` CLI flag
/// 2. `AHMA_LOG_DIR` env var (deprecated)
/// 3. Primary sandbox scope `<scope>/logs` (set after `roots/list`)
/// 4. `<cwd>/logs` if writable
/// 5. `~/.ahma/logs/<project-namespace>` — a per-project subdirectory, not one
///    shared flat file: the sandbox already enforces per-project isolation on
///    disk, and a single shared log would quietly undo that at the
///    observability layer (one project's commands/paths/errors readable
///    alongside every other project ahma has ever touched).
pub fn project_log_dir() -> PathBuf {
    if let Some(dir) = LOG_DIR_OVERRIDE.get() {
        return dir.clone();
    }

    if let Ok(val) = std::env::var("AHMA_LOG_DIR")
        && !val.is_empty()
    {
        tracing::warn!(
            "Deprecated: AHMA_LOG_DIR environment variable is set. Use the --log-dir flag instead."
        );
        return PathBuf::from(val);
    }

    if let Some(dir) = LOG_DIR_FROM_SCOPE.get() {
        return dir.clone();
    }

    if let Ok(cwd) = std::env::current_dir()
        && cwd.parent().is_some()
    {
        let log_dir = cwd.join("logs");
        let exists_and_writeable = log_dir.exists() && is_writeable(&log_dir);
        let can_create = !log_dir.exists() && is_writeable(&cwd);
        if exists_and_writeable || can_create {
            return log_dir;
        }
    }

    if let Some(home) = dirs::home_dir() {
        return home
            .join(".ahma")
            .join("logs")
            .join(project_log_namespace());
    }

    PathBuf::from(".").join("logs")
}

/// A stable, filesystem-safe, human-legible directory name for this project,
/// used to namespace the `~/.ahma/logs` fallback so it never mixes different
/// projects' logs into one shared file. Combines the cwd's own directory name
/// (for legibility — `ahma`, `my-app`, …) with a short hash of the full
/// canonicalized path (to disambiguate same-named checkouts in different
/// locations). Falls back to `"unknown"` when the cwd cannot be resolved.
fn project_log_namespace() -> String {
    use std::hash::{Hash, Hasher};

    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(_) => return "unknown".to_string(),
    };
    let canonical = dunce::canonicalize(&cwd).unwrap_or(cwd);
    let name: String = canonical
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut hasher);
    let digest = hasher.finish();

    format!("{name}-{digest:08x}", digest = digest & 0xFFFF_FFFF)
}

/// Walk up from `start` looking for a `.git` entry (a directory for a normal
/// clone, a file for a worktree or submodule), returning the repo root when
/// found.
fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Best-effort check for whether `log_dir` is already covered by some
/// `.gitignore` between its parent and `repo_root` (inclusive). This is a
/// heuristic — it matches a bare directory-name line (`logs`, `/logs`,
/// `logs/`), not the full gitignore glob grammar — good enough to avoid
/// nagging when a reasonable ignore rule already exists, without pulling in a
/// full gitignore-pattern engine for this one check.
fn is_log_dir_gitignored(repo_root: &Path, log_dir: &Path) -> bool {
    let dir_name = log_dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if dir_name.is_empty() {
        return false;
    }
    let mut dir = log_dir.parent().map(Path::to_path_buf);
    while let Some(d) = dir {
        let candidate = d.join(".gitignore");
        if let Ok(contents) = std::fs::read_to_string(&candidate) {
            for line in contents.lines() {
                let pattern = line.trim().trim_start_matches('/').trim_end_matches('/');
                if pattern == dir_name && !pattern.is_empty() && !line.trim().starts_with('#') {
                    return true;
                }
            }
        }
        if d == repo_root {
            break;
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    false
}

static LOG_LOCATION_DISCLOSED: Once = Once::new();

/// Emit a one-time, loud disclosure of the active log directory — so it is
/// never ambiguous which of the possible locations ([`project_log_dir`]'s
/// priority order) ended up active — and, if that directory lives inside a
/// git repository without an existing ignore rule, a warning that plaintext
/// operational logs (including full tool-call transcripts) are about to be
/// written into the tracked working tree, with the remedy. Safe to call from
/// multiple places; only the first call does anything.
pub fn disclose_log_location_once() {
    LOG_LOCATION_DISCLOSED.call_once(|| {
        let log_dir = project_log_dir();
        tracing::info!(
            log_dir = %log_dir.display(),
            "ahma: writing operational logs to this directory"
        );

        if let Some(repo_root) = find_git_root(&log_dir)
            && !is_log_dir_gitignored(&repo_root, &log_dir)
        {
            tracing::warn!(
                log_dir = %log_dir.display(),
                "ahma is writing plaintext operational logs — including full tool-call \
                 transcripts — into a directory inside this git repository, and it is not \
                 yet covered by .gitignore. Run `ahma logs gitignore` to add an ignore rule, \
                 or set --log-dir to a path outside the repo to avoid this entirely."
            );
        }
    });
}

/// Add an ignore rule for the active log directory to the nearest
/// `.gitignore` (creating one at the repo root if none exists yet). Returns
/// `Ok(true)` if an entry was added, `Ok(false)` if one already covered it.
/// Errors if the active log directory is not inside a git repository at all.
pub fn ensure_gitignore_entry() -> Result<bool> {
    let log_dir = project_log_dir();
    let repo_root = find_git_root(&log_dir).ok_or_else(|| {
        anyhow::anyhow!(
            "{} is not inside a git repository — nothing to gitignore",
            log_dir.display()
        )
    })?;
    if is_log_dir_gitignored(&repo_root, &log_dir) {
        return Ok(false);
    }
    let dir_name = log_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("could not determine the log directory's name"))?;

    let gitignore_path = repo_root.join(".gitignore");
    let mut contents = std::fs::read_to_string(&gitignore_path).unwrap_or_default();
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str(dir_name);
    contents.push_str("/\n");
    std::fs::write(&gitignore_path, contents)
        .with_context(|| format!("Failed to write {}", gitignore_path.display()))?;
    Ok(true)
}

fn is_writeable(path: &Path) -> bool {
    let test_file = path.join(".ahma_write_test");
    match std::fs::write(&test_file, "test") {
        Ok(()) => {
            let _ = std::fs::remove_file(&test_file);
            true
        }
        Err(_) => false,
    }
}

/// Paths for background bridge stdout/stderr capture under [`project_log_dir`].
pub fn bridge_capture_paths() -> (PathBuf, PathBuf) {
    let dir = project_log_dir();
    (dir.join(BRIDGE_STDOUT_NAME), dir.join(BRIDGE_STDERR_NAME))
}

/// Ensure `logs/` exists, prune stale files, and create bridge capture files with a header.
pub fn prepare_bridge_capture_files() -> Result<(PathBuf, PathBuf)> {
    disclose_log_location_once();
    let log_dir = project_log_dir();
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("Failed to create log directory {}", log_dir.display()))?;
    cleanup_old_logs(&log_dir);

    let (stdout_path, stderr_path) = bridge_capture_paths();
    for path in [&stdout_path, &stderr_path] {
        if !path.exists() {
            std::fs::write(path, BRIDGE_CAPTURE_HEADER).with_context(|| {
                format!("Failed to create bridge capture file {}", path.display())
            })?;
        }
    }
    Ok((stdout_path, stderr_path))
}

/// Initialize verbose logging for tests.
pub fn init_test_logging() {
    set_log_role("test");
    let _ = init_logging("trace", false);
}

/// Initializes the logging system.
pub fn init_logging(log_level: &str, log_to_file: bool) -> Result<TelemetryGuard> {
    init_logging_with_observability(log_level, log_to_file, None)
}

/// Initializes logging with an optional explicit observability configuration.
pub fn init_logging_with_observability(
    log_level: &str,
    log_to_file: bool,
    observability: Option<ObservabilityConfig>,
) -> Result<TelemetryGuard> {
    INIT.call_once(|| {
        do_setup_logging(log_level, log_to_file, observability);
    });

    Ok(PENDING_GUARD
        .lock()
        .unwrap()
        .take()
        .unwrap_or_else(TelemetryGuard::none))
}

struct PidRoleFormatter {
    inner: fmt::format::Format<fmt::format::Full, SystemTime>,
}

impl PidRoleFormatter {
    fn new(with_ansi: bool) -> Self {
        Self {
            inner: fmt::format::Format::default()
                .with_timer(SystemTime)
                .with_ansi(with_ansi),
        }
    }
}

impl<S, N> FormatEvent<S, N> for PidRoleFormatter
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        write!(writer, "pid={} role={} ", std::process::id(), log_role())?;
        self.inner.format_event(ctx, writer, event)
    }
}

fn do_setup_logging(
    log_level: &str,
    log_to_file: bool,
    observability: Option<ObservabilityConfig>,
) {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));

    let (otel_layer, guard) = {
        let config = observability.unwrap_or_else(|| ObservabilityConfig::from_env("ahma_mcp"));
        ahma_common::observability::create_otel_layer(&config)
    };
    *PENDING_GUARD.lock().unwrap() = Some(guard);

    let file_appender_opt = if log_to_file {
        try_create_file_appender()
    } else {
        None
    };
    if let Some(file_appender) = file_appender_opt {
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
        tracing_subscriber::registry()
            .with(env_filter)
            .with(otel_layer)
            .with(
                tracing_subscriber::fmt::layer()
                    .event_format(PidRoleFormatter::new(false))
                    .with_writer(non_blocking)
                    .with_ansi(false),
            )
            .init();
        // Keep the writer's flush guard reachable so a panic can flush the
        // buffered tail (SPEC R-SIGN.5) — a leaked guard could never be
        // dropped, so the final lines before an abnormal exit were lost.
        *FILE_LOG_FLUSH_GUARD.lock().unwrap() = Some(guard);
        install_panic_flush_hook();
        log_traceparent();
        disclose_log_location_once();
        return;
    }

    tracing_subscriber::registry()
        .with(env_filter)
        .with(otel_layer)
        .with(
            tracing_subscriber::fmt::layer()
                .event_format(PidRoleFormatter::new(true))
                .with_writer(stderr)
                .with_ansi(true),
        )
        .init();
    log_traceparent();
}

fn try_create_file_appender() -> Option<tracing_appender::rolling::RollingFileAppender> {
    let log_dir = project_log_dir();

    if !test_write_permission(&log_dir) {
        return None;
    }

    cleanup_old_logs(&log_dir);

    let appender = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tracing_appender::rolling::daily(&log_dir, MCP_LOG_BASENAME)
    }))
    .ok()?;

    #[cfg(unix)]
    try_update_current_log_symlink(&log_dir);

    Some(appender)
}

#[cfg(unix)]
fn try_update_current_log_symlink(log_dir: &Path) {
    use std::os::unix::fs::symlink;

    let today = chrono::Local::now().format("%Y-%m-%d");
    let dated_name = format!("{MCP_LOG_BASENAME}.{today}");
    let symlink_path = log_dir.join(MCP_LOG_BASENAME);

    let _ = std::fs::remove_file(&symlink_path);

    if let Err(e) = symlink(&dated_name, &symlink_path) {
        eprintln!("ahma: could not create log symlink {symlink_path:?} → {dated_name}: {e}");
    }
}

/// Returns true if `name` is a managed rolling log file eligible for retention pruning.
pub(crate) fn is_managed_log_file(name: &str) -> bool {
    name == MCP_LOG_BASENAME
        || name.starts_with(&format!("{MCP_LOG_BASENAME}."))
        || name.starts_with("ahma_bridge.")
}

/// Remove managed log files in `log_dir` older than [`LOG_RETENTION_SECS`].
pub(crate) fn cleanup_old_logs(log_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(log_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_managed_log_file(name) {
            continue;
        }
        if let Ok(modified) = meta.modified()
            && let Ok(elapsed) = modified.elapsed()
            && elapsed.as_secs() > LOG_RETENTION_SECS
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Read up to `max_bytes` from the tail of a file (for post-mortem bridge diagnostics).
pub fn read_log_tail(path: &Path, max_bytes: usize) -> String {
    use std::io::{Read, Seek, SeekFrom};

    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let Ok(len) = file.metadata().map(|m| m.len() as usize) else {
        return String::new();
    };
    let start = len.saturating_sub(max_bytes);
    if file.seek(SeekFrom::Start(start as u64)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn log_traceparent() {
    if let Some(tp) = ahma_common::observability::env_traceparent() {
        tracing::debug!(traceparent = %tp, "subprocess trace context from HTTP bridge");
    }
}

fn test_write_permission(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }

    let test_file = dir.join(".ahma_log_test");
    match std::fs::write(&test_file, "test") {
        Ok(()) => {
            let _ = std::fs::remove_file(&test_file);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_detect_log_role_proxy_stdio() {
        // argv-based detection is tested via explicit strings (no env mutation).
        assert_eq!(
            role_for_args(&["serve", "stdio", "--tools", "rust"]),
            "proxy"
        );
    }

    #[test]
    fn test_detect_log_role_bridge_http() {
        assert_eq!(role_for_args(&["serve", "http"]), "bridge");
    }

    #[test]
    fn test_detect_log_role_bridge_child_flags() {
        assert_eq!(
            role_for_args(&["serve", "--sandbox-scope", "/tmp/ws"]),
            "bridge"
        );
    }

    #[test]
    fn test_is_managed_log_file_matches() {
        assert!(is_managed_log_file("ahma.log"));
        assert!(is_managed_log_file("ahma.log.2026-06-10"));
        assert!(is_managed_log_file("ahma_bridge.out.log"));
        assert!(is_managed_log_file("ahma_bridge.err.log"));
        assert!(!is_managed_log_file("other.log"));
    }

    #[test]
    fn test_cleanup_old_logs_keeps_recent_files() {
        let temp = tempdir().unwrap();
        let log_dir = temp.path().join("logs");
        fs::create_dir_all(&log_dir).unwrap();

        let fresh = log_dir.join("ahma_bridge.out.log");
        fs::write(&fresh, "recent bridge stdout\n").unwrap();

        cleanup_old_logs(&log_dir);

        assert!(fresh.exists(), "recent bridge log should be kept");
    }

    #[test]
    fn test_log_retention_threshold() {
        const _: () = assert!(LOG_RETENTION_SECS > 23 * 60 * 60);
        const _: () = assert!(LOG_RETENTION_SECS < 25 * 60 * 60);
    }

    #[test]
    fn test_set_log_role() {
        // OnceLock only accepts first set; use a dedicated check on detect helper instead.
        assert_eq!(log_role(), "unknown");
    }

    #[test]
    fn test_prepare_bridge_capture_files_creates_header() {
        let temp = tempdir().unwrap();
        let prev = std::env::current_dir().unwrap();
        let temp_path_canonical = dunce::canonicalize(temp.path()).unwrap();
        std::env::set_current_dir(&temp_path_canonical).unwrap();

        let (out, err) = prepare_bridge_capture_files().expect("prepare bridge logs");
        assert!(out.exists());
        assert!(err.exists());
        let stderr_content = fs::read_to_string(&err).unwrap();
        assert!(stderr_content.contains("ahma background bridge"));

        let _ = std::env::set_current_dir(prev);
    }

    #[test]
    fn test_read_log_tail_returns_suffix() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("sample.log");
        fs::write(&path, "0123456789").unwrap();
        let tail = read_log_tail(&path, 4);
        assert_eq!(tail, "6789");
    }

    #[test]
    fn test_project_log_dir_env_override() {
        unsafe {
            std::env::set_var("AHMA_LOG_DIR", "/custom/log/dir");
        }
        let dir = project_log_dir();
        unsafe {
            std::env::remove_var("AHMA_LOG_DIR");
        }
        assert_eq!(dir, PathBuf::from("/custom/log/dir"));
    }

    #[test]
    fn test_project_log_dir_cwd_writeable() {
        let temp = tempdir().unwrap();
        let prev = std::env::current_dir().unwrap();
        let temp_path_canonical = dunce::canonicalize(temp.path()).unwrap();
        std::env::set_current_dir(&temp_path_canonical).unwrap();

        let dir = project_log_dir();
        assert_eq!(dir, temp_path_canonical.join("logs"));

        let _ = std::env::set_current_dir(prev);
    }

    #[test]
    #[cfg(unix)]
    fn test_project_log_dir_cwd_root_fallback() {
        let prev = std::env::current_dir().unwrap();
        if std::env::set_current_dir(Path::new("/")).is_ok() {
            let dir = project_log_dir();
            if let Some(home) = dirs::home_dir() {
                // Namespaced per-project (see project_log_namespace): the root
                // has no file_name, so the namespace falls back to "unknown-<hash>".
                assert_eq!(
                    dir.parent().map(Path::to_path_buf),
                    Some(home.join(".ahma").join("logs"))
                );
                let leaf = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
                assert!(
                    leaf.starts_with("unknown-"),
                    "expected an 'unknown-<hash>' namespace, got {leaf:?}"
                );
            } else {
                assert_eq!(dir, PathBuf::from(".").join("logs"));
            }
            let _ = std::env::set_current_dir(prev);
        }
    }

    use std::sync::{LazyLock, Mutex as StdMutex};

    /// Serializes tests that mutate process-global state (env vars + cwd),
    /// since `project_log_dir`, `bridge_capture_paths`, `try_create_file_appender`,
    /// and `detect_log_role_from_startup` all read the environment / current dir.
    static ENV_MUTEX: LazyLock<StdMutex<()>> = LazyLock::new(|| StdMutex::new(()));

    #[test]
    fn test_detect_log_role_from_startup_server_child_env() {
        let _g = ENV_MUTEX.lock().unwrap();
        let prev = std::env::var("AHMA_SERVER_CHILD").ok();
        unsafe {
            std::env::set_var("AHMA_SERVER_CHILD", "1");
        }
        // The env check short-circuits before argv inspection (lines 56-57).
        let role = detect_log_role_from_startup();
        // Restore before releasing the lock.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("AHMA_SERVER_CHILD", v),
                None => std::env::remove_var("AHMA_SERVER_CHILD"),
            }
        }
        assert_eq!(role, "bridge");
    }

    #[test]
    fn test_read_log_tail_missing_file_returns_empty() {
        let temp = tempdir().unwrap();
        let missing = temp.path().join("does_not_exist.log");
        // File::open fails -> empty string (lines 367-369).
        assert_eq!(read_log_tail(&missing, 64), "");
    }

    #[test]
    fn test_read_log_tail_empty_file_returns_empty() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("empty.log");
        fs::write(&path, "").unwrap();
        // len == 0 -> start == 0 -> read yields empty buffer.
        assert_eq!(read_log_tail(&path, 64), "");
    }

    #[test]
    fn test_read_log_tail_max_bytes_exceeds_length_returns_whole_file() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("short.log");
        fs::write(&path, "abc").unwrap();
        // max_bytes > len -> saturating_sub clamps start to 0 (line 373).
        assert_eq!(read_log_tail(&path, 4096), "abc");
    }

    #[test]
    fn test_cleanup_old_logs_missing_dir_is_noop() {
        let temp = tempdir().unwrap();
        let missing = temp.path().join("no_such_logs_dir");
        // read_dir fails -> early return, must not panic (lines 337-339).
        cleanup_old_logs(&missing);
        assert!(!missing.exists());
    }

    #[test]
    fn test_cleanup_old_logs_skips_directories_and_unmanaged_files() {
        let temp = tempdir().unwrap();
        let log_dir = temp.path().join("logs");
        fs::create_dir_all(&log_dir).unwrap();

        // A directory whose name matches a managed prefix: is_managed == true
        // but meta.is_file() == false -> continue (lines 345-347).
        let managed_dir = log_dir.join("ahma_bridge.subdir");
        fs::create_dir_all(&managed_dir).unwrap();

        // A regular file that is NOT a managed log -> is_managed == false skip (line 351).
        let unmanaged = log_dir.join("notes.txt");
        fs::write(&unmanaged, "keep me").unwrap();

        cleanup_old_logs(&log_dir);

        assert!(
            managed_dir.exists(),
            "managed-named directory must be skipped"
        );
        assert!(unmanaged.exists(), "unmanaged file must be skipped");
    }

    #[test]
    fn test_prepare_bridge_capture_files_idempotent_does_not_duplicate_header() {
        let _g = ENV_MUTEX.lock().unwrap();
        let temp = tempdir().unwrap();
        let prev = std::env::current_dir().unwrap();
        let temp_canon = dunce::canonicalize(temp.path()).unwrap();
        std::env::set_current_dir(&temp_canon).unwrap();

        let (out1, err1) = prepare_bridge_capture_files().expect("first prepare");
        // Second call: files already exist -> path.exists() true skips write (line 177).
        let (out2, err2) = prepare_bridge_capture_files().expect("second prepare");

        assert_eq!(out1, out2);
        assert_eq!(err1, err2);
        let out_content = fs::read_to_string(&out1).unwrap();
        let err_content = fs::read_to_string(&err1).unwrap();

        let _ = std::env::set_current_dir(prev);

        // Header written exactly once (no append on the idempotent call).
        assert_eq!(out_content, BRIDGE_CAPTURE_HEADER);
        assert_eq!(err_content, BRIDGE_CAPTURE_HEADER);
    }

    #[test]
    fn test_bridge_capture_paths_live_under_project_log_dir() {
        let _g = ENV_MUTEX.lock().unwrap();
        let temp = tempdir().unwrap();
        let prev = std::env::current_dir().unwrap();
        let temp_canon = dunce::canonicalize(temp.path()).unwrap();
        std::env::set_current_dir(&temp_canon).unwrap();

        let (out, err) = bridge_capture_paths();

        let _ = std::env::set_current_dir(prev);

        let expected_dir = temp_canon.join("logs");
        assert_eq!(out, expected_dir.join(BRIDGE_STDOUT_NAME));
        assert_eq!(err, expected_dir.join(BRIDGE_STDERR_NAME));
    }

    #[test]
    fn test_test_write_permission_creates_dir_and_returns_true() {
        let temp = tempdir().unwrap();
        // Non-existent nested path: create_dir_all succeeds, write probe succeeds (lines 391-401).
        let nested = temp.path().join("a").join("b").join("logs");
        assert!(test_write_permission(&nested));
        assert!(
            nested.exists(),
            "test_write_permission should create the dir"
        );
        // Probe file must be cleaned up.
        assert!(!nested.join(".ahma_log_test").exists());
    }

    #[test]
    fn test_try_create_file_appender_returns_some_for_writeable_cwd() {
        let _g = ENV_MUTEX.lock().unwrap();
        let temp = tempdir().unwrap();
        let prev = std::env::current_dir().unwrap();
        let temp_canon = dunce::canonicalize(temp.path()).unwrap();
        std::env::set_current_dir(&temp_canon).unwrap();

        // project_log_dir() -> <cwd>/logs which is writeable -> Some(appender).
        // Exercises cleanup_old_logs + daily appender construction (+ unix symlink).
        let appender = try_create_file_appender();
        let log_dir = temp_canon.join("logs");

        let _ = std::env::set_current_dir(prev);

        assert!(
            appender.is_some(),
            "writeable cwd/logs should yield an appender"
        );
        assert!(log_dir.exists(), "log dir should be created");

        #[cfg(unix)]
        {
            // try_update_current_log_symlink creates a symlink named MCP_LOG_BASENAME.
            let symlink_path = log_dir.join(MCP_LOG_BASENAME);
            assert!(
                std::fs::symlink_metadata(&symlink_path).is_ok(),
                "current-log symlink should exist on unix"
            );
        }
    }

    #[test]
    fn test_project_log_namespace_is_filesystem_safe_and_stable() {
        let temp = tempdir().unwrap();
        let sub = temp.path().join("My Project!");
        fs::create_dir_all(&sub).unwrap();
        let temp_canon = dunce::canonicalize(&sub).unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&temp_canon).unwrap();

        let first = project_log_namespace();
        let second = project_log_namespace();

        let _ = std::env::set_current_dir(prev);

        assert_eq!(first, second, "namespace must be stable across calls");
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "namespace must be filesystem-safe, got {first:?}"
        );
        assert!(
            first.starts_with("My-Project-"),
            "namespace should stay legible, got {first:?}"
        );
    }

    #[test]
    fn test_project_log_namespace_disambiguates_same_named_dirs() {
        let temp = tempdir().unwrap();
        let a = temp.path().join("a").join("shared-name");
        let b = temp.path().join("b").join("shared-name");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let prev = std::env::current_dir().unwrap();

        std::env::set_current_dir(dunce::canonicalize(&a).unwrap()).unwrap();
        let ns_a = project_log_namespace();
        std::env::set_current_dir(dunce::canonicalize(&b).unwrap()).unwrap();
        let ns_b = project_log_namespace();

        let _ = std::env::set_current_dir(prev);

        assert_ne!(
            ns_a, ns_b,
            "two directories with the same name in different locations must not collide"
        );
    }

    #[test]
    fn test_find_git_root_walks_up_to_dot_git() {
        let temp = tempdir().unwrap();
        let repo_root = dunce::canonicalize(temp.path()).unwrap();
        fs::create_dir_all(repo_root.join(".git")).unwrap();
        let nested = repo_root.join("logs").join("nested");
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(find_git_root(&nested), Some(repo_root.clone()));
        assert_eq!(find_git_root(&repo_root), Some(repo_root));
    }

    #[test]
    fn test_find_git_root_none_outside_a_repo() {
        let temp = tempdir().unwrap();
        let dir = dunce::canonicalize(temp.path()).unwrap();
        assert_eq!(find_git_root(&dir), None);
    }

    #[test]
    fn test_is_log_dir_gitignored_matches_bare_dir_name() {
        let temp = tempdir().unwrap();
        let repo_root = dunce::canonicalize(temp.path()).unwrap();
        fs::write(repo_root.join(".gitignore"), "target/\nlogs/\n").unwrap();
        let log_dir = repo_root.join("logs");

        assert!(is_log_dir_gitignored(&repo_root, &log_dir));
    }

    #[test]
    fn test_is_log_dir_gitignored_false_when_uncovered() {
        let temp = tempdir().unwrap();
        let repo_root = dunce::canonicalize(temp.path()).unwrap();
        fs::write(repo_root.join(".gitignore"), "target/\n").unwrap();
        let log_dir = repo_root.join("logs");

        assert!(!is_log_dir_gitignored(&repo_root, &log_dir));
    }

    #[test]
    fn test_is_log_dir_gitignored_false_with_no_gitignore_at_all() {
        let temp = tempdir().unwrap();
        let repo_root = dunce::canonicalize(temp.path()).unwrap();
        let log_dir = repo_root.join("logs");

        assert!(!is_log_dir_gitignored(&repo_root, &log_dir));
    }

    #[test]
    fn test_ensure_gitignore_entry_creates_file_when_missing() {
        let temp = tempdir().unwrap();
        let repo_root = dunce::canonicalize(temp.path()).unwrap();
        fs::create_dir_all(repo_root.join(".git")).unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&repo_root).unwrap();

        let added = ensure_gitignore_entry();

        let _ = std::env::set_current_dir(prev);

        assert!(matches!(added, Ok(true)));
        let contents = fs::read_to_string(repo_root.join(".gitignore")).unwrap();
        assert!(contents.contains("logs/"), "got: {contents:?}");
    }

    #[test]
    fn test_ensure_gitignore_entry_appends_without_clobbering_existing_content() {
        let temp = tempdir().unwrap();
        let repo_root = dunce::canonicalize(temp.path()).unwrap();
        fs::create_dir_all(repo_root.join(".git")).unwrap();
        fs::write(repo_root.join(".gitignore"), "target/").unwrap(); // no trailing newline
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&repo_root).unwrap();

        let added = ensure_gitignore_entry();

        let _ = std::env::set_current_dir(prev);

        assert!(matches!(added, Ok(true)));
        let contents = fs::read_to_string(repo_root.join(".gitignore")).unwrap();
        assert_eq!(contents, "target/\nlogs/\n");
    }

    #[test]
    fn test_ensure_gitignore_entry_is_a_noop_when_already_covered() {
        let temp = tempdir().unwrap();
        let repo_root = dunce::canonicalize(temp.path()).unwrap();
        fs::create_dir_all(repo_root.join(".git")).unwrap();
        fs::write(repo_root.join(".gitignore"), "logs/\n").unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&repo_root).unwrap();

        let added = ensure_gitignore_entry();
        let contents_before = fs::read_to_string(repo_root.join(".gitignore")).unwrap();

        let _ = std::env::set_current_dir(prev);

        assert!(matches!(added, Ok(false)));
        assert_eq!(contents_before, "logs/\n", "must not duplicate the entry");
    }

    #[test]
    fn test_ensure_gitignore_entry_errors_outside_a_git_repo() {
        let temp = tempdir().unwrap();
        let dir = dunce::canonicalize(temp.path()).unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();

        let result = ensure_gitignore_entry();

        let _ = std::env::set_current_dir(prev);

        assert!(result.is_err());
    }

    fn role_for_args(args: &[&str]) -> &'static str {
        match args.first().copied() {
            None => "cli",
            Some("serve") => match args.get(1).copied() {
                Some("stdio") => "proxy",
                Some("http") | Some("unix") => "bridge",
                Some(s) if s.starts_with('-') => "bridge",
                None => "bridge",
                _ => "bridge",
            },
            Some("tui") => "tui",
            Some("daemon") => "daemon",
            Some("update") => "update",
            Some("setup") => "setup",
            Some("hooks") if args.contains(&"run-shell") => "cli",
            Some("tool") if args.get(1).copied() == Some("run") => "cli",
            _ => "cli",
        }
    }
}
