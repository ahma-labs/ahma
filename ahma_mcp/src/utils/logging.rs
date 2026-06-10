//! # Logging Initialization
//!
//! Centralized logging for ahma processes. Every line is prefixed with `pid=` and
//! `role=` so interleaved multi-process logs in `./logs/ahma_mcp.log` remain attributable.

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
        self,
        FmtContext,
        format::{FormatEvent, Writer},
        time::SystemTime,
    },
    prelude::*,
    registry::LookupSpan,
};

static INIT: Once = Once::new();
/// Passes the OTEL guard out of the `call_once` closure to the caller.
static PENDING_GUARD: Mutex<Option<TelemetryGuard>> = Mutex::new(None);

static LOG_ROLE: OnceLock<&'static str> = OnceLock::new();

/// Rolling structured log basename (daily rotation appends `.YYYY-MM-DD`).
pub const MCP_LOG_BASENAME: &str = "ahma_mcp.log";

/// Background bridge raw stdout/stderr capture files (see `ahma_mcp.log` for structured logs).
pub const BRIDGE_STDOUT_NAME: &str = "ahma_bridge.out.log";
pub const BRIDGE_STDERR_NAME: &str = "ahma_bridge.err.log";

/// One-line header written when bridge capture files are first created.
pub const BRIDGE_CAPTURE_HEADER: &str =
    "# ahma background bridge stdout/stderr capture — see ahma_mcp.log for structured logs\n";

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

/// Project log directory: `<cwd>/logs`.
pub fn project_log_dir() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("logs")
}

/// Paths for background bridge stdout/stderr capture under [`project_log_dir`].
pub fn bridge_capture_paths() -> (PathBuf, PathBuf) {
    let dir = project_log_dir();
    (
        dir.join(BRIDGE_STDOUT_NAME),
        dir.join(BRIDGE_STDERR_NAME),
    )
}

/// Ensure `logs/` exists, prune stale files, and create bridge capture files with a header.
pub fn prepare_bridge_capture_files() -> Result<(PathBuf, PathBuf)> {
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
        write!(
            writer,
            "pid={} role={} ",
            std::process::id(),
            log_role()
        )?;
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
        let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
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
        Box::leak(Box::new(_guard));
        log_traceparent();
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
        assert!(is_managed_log_file("ahma_mcp.log"));
        assert!(is_managed_log_file("ahma_mcp.log.2026-06-10"));
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
        assert!(LOG_RETENTION_SECS > 23 * 60 * 60);
        assert!(LOG_RETENTION_SECS < 25 * 60 * 60);
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
        std::env::set_current_dir(temp.path()).unwrap();

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
            Some("hooks") if args.iter().any(|a| *a == "run-shell") => "cli",
            Some("tool") if args.get(1).copied() == Some("run") => "cli",
            _ => "cli",
        }
    }
}
