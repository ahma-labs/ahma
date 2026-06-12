//! Full-output spill files for async operations.
//!
//! `Operation::stdout_tail` is a bounded window (100 lines) sized for status
//! polling and token economy.  The spill file holds the COMPLETE output of an
//! operation so agents can query arbitrarily large results with the existing
//! file tools (`tail`, `grep`, `head`) instead of re-running commands:
//!
//! ```text
//! <project log dir>/operations/<operation_id>.log
//! ```
//!
//! Lines are written as produced (stderr lines prefixed with `[stderr] `),
//! after secret redaction but BEFORE token-minimisation, so the file is the
//! faithful record.  Spilling degrades gracefully: any I/O error disables the
//! writer for that operation without affecting execution.  Files older than
//! the standard log retention window are cleaned up once per process.

use std::path::PathBuf;
use std::sync::OnceLock;
use tokio::io::AsyncWriteExt;

/// Directory under the project log dir holding per-operation output files.
fn spill_dir() -> PathBuf {
    crate::utils::logging::project_log_dir().join("operations")
}

/// Path of the spill file for an operation id.
pub fn operation_spill_path(op_id: &str) -> PathBuf {
    // Operation ids are generated internally, but sanitise defensively so an
    // id can never escape the spill directory.
    let safe: String = op_id
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    spill_dir().join(format!("{safe}.log"))
}

/// Best-effort writer for one operation's spill file.
pub struct SpillWriter {
    inner: Option<tokio::io::BufWriter<tokio::fs::File>>,
}

impl SpillWriter {
    /// Create the spill file for `op_id`.  On any error spilling is silently
    /// disabled — execution must never fail because of spill I/O.
    pub async fn create(op_id: &str) -> Self {
        let dir = spill_dir();
        if tokio::fs::create_dir_all(&dir).await.is_err() {
            return Self { inner: None };
        }
        cleanup_old_spills_once(dir.clone());

        match tokio::fs::File::create(operation_spill_path(op_id)).await {
            Ok(f) => Self {
                inner: Some(tokio::io::BufWriter::new(f)),
            },
            Err(e) => {
                tracing::debug!("output spill disabled for {op_id}: {e}");
                Self { inner: None }
            }
        }
    }

    /// Append one output line.  Stderr lines are prefixed with `[stderr] `.
    pub async fn write_line(&mut self, line: &str, is_stderr: bool) {
        if let Some(w) = &mut self.inner {
            let prefix: &[u8] = if is_stderr { b"[stderr] " } else { b"" };
            let failed = w.write_all(prefix).await.is_err()
                || w.write_all(line.as_bytes()).await.is_err()
                || w.write_all(b"\n").await.is_err();
            if failed {
                // Disk error (full, removed dir, ...) — stop spilling.
                self.inner = None;
            }
        }
    }

    /// Flush buffered output.  Call on every exit path (complete, cancel,
    /// timeout) so the file reflects everything received.
    pub async fn finish(&mut self) {
        if let Some(w) = &mut self.inner {
            let _ = w.flush().await;
        }
    }
}

/// Delete spill files older than the standard log retention window.
/// Runs at most once per process, in a background task.
fn cleanup_old_spills_once(dir: PathBuf) {
    static CLEANED: OnceLock<()> = OnceLock::new();
    if CLEANED.set(()).is_err() {
        return;
    }
    tokio::spawn(async move {
        let retention = std::time::Duration::from_secs(crate::utils::logging::LOG_RETENTION_SECS);
        let now = std::time::SystemTime::now();
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(meta) = entry.metadata().await else {
                continue;
            };
            let age_ok = meta
                .modified()
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .map(|age| age > retention)
                .unwrap_or(false);
            if age_ok {
                let _ = tokio::fs::remove_file(entry.path()).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spill_path_sanitises_ids() {
        let p = operation_spill_path("op/../../etc/passwd");
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(!name.contains('/') && !name.contains('.') || name.ends_with(".log"));
        assert!(name.starts_with("op_"), "specials replaced: {name}");
        assert!(name.ends_with("etc_passwd.log"), "got: {name}");
        // The only dot is the .log extension — no path traversal possible.
        assert_eq!(name.matches('.').count(), 1);
    }

    #[tokio::test]
    async fn write_and_flush_roundtrip() {
        // Redirect the spill dir via a temp log dir is not possible here
        // (project_log_dir is process-global), so exercise the writer against
        // a scratch file directly.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("op.log");
        let file = tokio::fs::File::create(&path).await.unwrap();
        let mut w = SpillWriter {
            inner: Some(tokio::io::BufWriter::new(file)),
        };
        w.write_line("hello", false).await;
        w.write_line("boom", true).await;
        w.finish().await;

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "hello\n[stderr] boom\n");
    }
}
