//! Worker execution engine.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
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

#[cfg(target_os = "macos")]
fn is_sandbox_exec_working() -> bool {
    let output = std::process::Command::new("sandbox-exec")
        .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
        .output();
    matches!(output, Ok(out) if out.status.success())
}

#[cfg(target_os = "macos")]
fn generate_worker_profile(cwd: &Path) -> String {
    let cwd_str = cwd.to_string_lossy();
    format!(
        r#"(version 1)
(deny default)
(allow process*)
(allow signal)
(allow sysctl-read)
(allow file-read*)
(allow file-write* (subpath "/private/tmp"))
(allow file-write* (subpath "/private/var/folders"))
(allow file-write* (subpath "{cwd}"))
(allow file-write* (literal "/dev/null"))
(allow file-write* (literal "/dev/zero"))
(allow mach-lookup)
(allow ipc-posix-shm*)
"#,
        cwd = cwd_str
    )
}

async fn run_with_timeout(
    program: &str,
    args: &[&str],
    cwd: &Path,
    extra_args: &Option<Vec<String>>,
    timeout: Duration,
) -> Result<std::process::Output> {
    #[cfg(target_os = "macos")]
    let use_sandbox = is_sandbox_exec_working();

    #[cfg(target_os = "macos")]
    let (real_program, real_args) = if use_sandbox {
        let profile = generate_worker_profile(cwd);
        let mut final_args = vec!["-p".to_string(), profile, program.to_string()];
        final_args.extend(args.iter().map(|s| s.to_string()));
        if let Some(extras) = extra_args {
            final_args.extend(extras.iter().cloned());
        }
        ("sandbox-exec".to_string(), final_args)
    } else {
        let mut final_args = args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        if let Some(extras) = extra_args {
            final_args.extend(extras.iter().cloned());
        }
        (program.to_string(), final_args)
    };

    #[cfg(not(target_os = "macos"))]
    let (real_program, real_args) = {
        let mut final_args = args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        if let Some(extras) = extra_args {
            final_args.extend(extras.iter().cloned());
        }
        (program.to_string(), final_args)
    };

    let mut cmd = tokio::process::Command::new(real_program);
    cmd.args(&real_args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

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

/// Compute the SHA-256 hex digest of `data`.
fn sha256_hex(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    hash.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
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
        // A real SHA-256 hex digest is always 64 lowercase hex characters.
        assert_eq!(h1.len(), 64, "SHA-256 hex digest must be 64 chars");
        assert!(
            h1.chars().all(|c| c.is_ascii_hexdigit()),
            "digest must be hex"
        );
    }

    #[test]
    fn sha256_hex_differs_for_different_inputs() {
        assert_ne!(sha256_hex(b"hello"), sha256_hex(b"world"));
    }

    #[tokio::test]
    async fn rust_runner_runs_hello_world() {
        let tmp = TempDir::new().unwrap();
        let runner = make_runner(&tmp, WorkerLanguage::Rust);
        let result = runner
            .run(r#"fn main() { println!("hello from rust worker"); }"#)
            .await
            .expect("Rust worker should run successfully");
        assert_eq!(
            result.exit_code, 0,
            "worker exited non-zero: {}",
            result.output
        );
        assert!(
            result.output.contains("hello from rust worker"),
            "unexpected output: {}",
            result.output
        );
    }
}
