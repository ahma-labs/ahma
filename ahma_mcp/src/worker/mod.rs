//! # Ephemeral Worker Code Synthesis
//!
//! The `worker` tool type compiles and runs synthesized Rust or Python programs
//! inside a sub-vault.  Because the synthesized code is deterministic and
//! executed without an LLM in the loop, it cannot be re-injected mid-run.
//!
//! ## Security model
//!
//! - The worker runs inside the vault's `workdir/` sandbox scope.
//! - Source is written to a tempfile, compiled/run, then deleted.
//! - `keep_source: true` in [`WorkerConfig`] overrides deletion.
//! - A SHA-256 digest of the source is recorded in the vault audit log.
//!
//! ## Languages
//!
//! | Language | Requirement | Command |
//! |----------|-------------|---------|
//! | Rust     | `rustc` on PATH | `rustc -o <out> <src>.rs` then `./<out>` |
//! | Python   | `python3` on PATH | `python3 <src>.py` |

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, info};

use crate::config::{WorkerConfig, WorkerLanguage};

// ─────────────────────────────────────────────────────────────────────────────
// WorkerResult
// ─────────────────────────────────────────────────────────────────────────────

/// The outcome of running an ephemeral worker.
#[derive(Debug, Clone)]
pub struct WorkerResult {
    /// Combined stdout + stderr from the worker process.
    pub output: String,
    /// Exit code of the worker process.
    pub exit_code: i32,
    /// SHA-256 hex digest of the source code (for audit).
    pub source_hash: String,
    /// Whether the source file was kept after execution.
    pub kept_source: bool,
    /// Path to the source file (present only when `kept_source` is true).
    pub source_path: Option<PathBuf>,
}

// ─────────────────────────────────────────────────────────────────────────────
// WorkerRunner
// ─────────────────────────────────────────────────────────────────────────────

/// Compiles and runs ephemeral synthesized worker code.
pub struct WorkerRunner {
    cfg: WorkerConfig,
    /// Directory in which compilation artefacts and output files are written.
    workdir: PathBuf,
}

impl WorkerRunner {
    /// Create a runner scoped to `workdir`.
    pub fn new(cfg: WorkerConfig, workdir: impl Into<PathBuf>) -> Self {
        Self {
            cfg,
            workdir: workdir.into(),
        }
    }

    /// Execute `source_code` and return the result.
    ///
    /// The source is written to a temp file, compiled or run, and the output
    /// captured.  The source file is deleted after execution unless
    /// `keep_source` is set in the config.
    pub async fn run(&self, source_code: &str) -> Result<WorkerResult> {
        let hash = sha256_hex(source_code.as_bytes());
        debug!("Worker source SHA-256: {hash}");

        let timeout = Duration::from_secs(self.cfg.timeout_seconds.unwrap_or(60));

        match self.cfg.language {
            WorkerLanguage::Rust => self.run_rust(source_code, &hash, timeout).await,
            WorkerLanguage::Python => self.run_python(source_code, &hash, timeout).await,
        }
    }

    // ── Rust ──────────────────────────────────────────────────────────────────

    async fn run_rust(&self, source: &str, hash: &str, timeout: Duration) -> Result<WorkerResult> {
        let src_path = self.workdir.join(format!("worker_{hash}.rs"));
        let bin_path = self.workdir.join(format!("worker_{hash}"));

        tokio::fs::write(&src_path, source)
            .await
            .context("Failed to write Rust worker source")?;

        info!("Compiling Rust worker: {}", src_path.display());

        let compile_output = run_with_timeout(
            "rustc",
            &[src_path.to_str().unwrap(), "-o", bin_path.to_str().unwrap()],
            &self.workdir,
            &self.cfg.extra_args,
            timeout,
        )
        .await?;

        if !compile_output.status.success() {
            let stderr = String::from_utf8_lossy(&compile_output.stderr).to_string();
            cleanup(&src_path, &bin_path, false).await;
            return Ok(WorkerResult {
                output: format!("Compilation failed:\n{stderr}"),
                exit_code: compile_output.status.code().unwrap_or(-1),
                source_hash: hash.to_string(),
                kept_source: false,
                source_path: None,
            });
        }

        info!("Running compiled Rust worker");
        let run_output = run_with_timeout(
            bin_path.to_str().unwrap(),
            &[],
            &self.workdir,
            &None,
            timeout,
        )
        .await?;

        let output = combine_output(&run_output);
        let exit_code = run_output.status.code().unwrap_or(-1);
        let kept_source = self.cfg.keep_source;

        cleanup(&src_path, &bin_path, kept_source).await;

        Ok(WorkerResult {
            output,
            exit_code,
            source_hash: hash.to_string(),
            kept_source,
            source_path: if kept_source { Some(src_path) } else { None },
        })
    }

    // ── Python ────────────────────────────────────────────────────────────────

    async fn run_python(
        &self,
        source: &str,
        hash: &str,
        timeout: Duration,
    ) -> Result<WorkerResult> {
        let src_path = self.workdir.join(format!("worker_{hash}.py"));

        tokio::fs::write(&src_path, source)
            .await
            .context("Failed to write Python worker source")?;

        info!("Running Python worker: {}", src_path.display());

        let run_output = run_with_timeout(
            "python3",
            &[src_path.to_str().unwrap()],
            &self.workdir,
            &self.cfg.extra_args,
            timeout,
        )
        .await?;

        let output = combine_output(&run_output);
        let exit_code = run_output.status.code().unwrap_or(-1);
        let kept_source = self.cfg.keep_source;

        if !kept_source {
            let _ = tokio::fs::remove_file(&src_path).await;
        }

        Ok(WorkerResult {
            output,
            exit_code,
            source_hash: hash.to_string(),
            kept_source,
            source_path: if kept_source { Some(src_path) } else { None },
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// helpers
// ─────────────────────────────────────────────────────────────────────────────

async fn run_with_timeout(
    program: &str,
    args: &[&str],
    cwd: &Path,
    extra_args: &Option<Vec<String>>,
    timeout: Duration,
) -> Result<std::process::Output> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    if let Some(extras) = extra_args {
        cmd.args(extras);
    }

    tokio::time::timeout(timeout, cmd.output())
        .await
        .context("Worker timed out")?
        .context("Worker process failed to run")
}

fn combine_output(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.is_empty() {
        stdout.to_string()
    } else if stdout.is_empty() {
        stderr.to_string()
    } else {
        format!("{stdout}\n--- stderr ---\n{stderr}")
    }
}

async fn cleanup(src: &Path, bin: &Path, keep: bool) {
    if !keep {
        let _ = tokio::fs::remove_file(src).await;
    }
    let _ = tokio::fs::remove_file(bin).await;
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    let hash = hasher.finalize();
    hash.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{WorkerConfig, WorkerLanguage};
    use tempfile::TempDir;

    fn make_runner(tmp: &TempDir, lang: WorkerLanguage) -> WorkerRunner {
        WorkerRunner::new(
            WorkerConfig {
                language: lang,
                extra_args: None,
                keep_source: false,
                timeout_seconds: Some(10),
            },
            tmp.path(),
        )
    }

    #[test]
    fn sha256_hex_is_deterministic() {
        let h1 = sha256_hex(b"hello");
        let h2 = sha256_hex(b"hello");
        assert_eq!(h1, h2);
    }

    #[test]
    fn sha256_hex_differs_for_different_inputs() {
        assert_ne!(sha256_hex(b"hello"), sha256_hex(b"world"));
    }

    #[tokio::test]
    async fn python_hello_world() {
        // Only runs if python3 is available
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // Skip silently
        }
        let tmp = TempDir::new().unwrap();
        let runner = make_runner(&tmp, WorkerLanguage::Python);
        let result = runner.run("print('hello from worker')").await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.output.contains("hello from worker"));
        assert!(!result.kept_source, "source should be deleted");
    }
}
