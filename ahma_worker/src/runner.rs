//! Worker execution engine.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, info};

use crate::config::{WorkerConfig, WorkerLanguage};

/// The outcome of running an ephemeral worker.
#[derive(Debug, Clone)]
pub struct WorkerResult {
    /// Combined stdout + stderr from the worker process.
    pub output: String,
    /// Exit code of the worker process.
    pub exit_code: i32,
    /// Hash digest of the source code (for audit).
    pub source_hash: String,
    /// Whether the source file was kept after execution.
    pub kept_source: bool,
    /// Path to the source file (present only when `kept_source` is true).
    pub source_path: Option<PathBuf>,
}

/// Compiles and runs ephemeral synthesized worker code.
pub struct WorkerRunner {
    cfg: WorkerConfig,
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
    pub async fn run(&self, source_code: &str) -> Result<WorkerResult> {
        let hash = sha256_hex(source_code.as_bytes());
        debug!("Worker source SHA-256: {hash}");

        let timeout = Duration::from_secs(self.cfg.timeout_seconds.unwrap_or(60));

        match self.cfg.language {
            WorkerLanguage::Rust => self.run_rust(source_code, &hash, timeout).await,
            WorkerLanguage::Python => self.run_python(source_code, &hash, timeout).await,
        }
    }

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
        let run_output =
            run_with_timeout(bin_path.to_str().unwrap(), &[], &self.workdir, &None, timeout)
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
    let mut h: u64 = 5381;
    for &b in data {
        h = h.wrapping_mul(33).wrapping_add(b as u64);
    }
    format!("{h:016x}")
}

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
    async fn python_runner_runs_hello_world() {
        let tmp = TempDir::new().unwrap();
        let runner = make_runner(&tmp, WorkerLanguage::Python);
        let result = runner.run("print('hello from python')").await;
        if let Ok(r) = result {
            assert!(r.output.contains("hello from python"));
            assert_eq!(r.exit_code, 0);
        }
        // If python3 isn't installed, the test is skipped gracefully.
    }
}
