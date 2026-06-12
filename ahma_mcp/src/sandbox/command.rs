use anyhow::Result;
use std::path::Path;

use super::core::Sandbox;
use super::types::SandboxMode;

impl Sandbox {
    /// Create a sandboxed tokio process Command.
    pub fn create_command(
        &self,
        program: &str,
        args: &[String],
        working_dir: &Path,
    ) -> Result<tokio::process::Command> {
        if self.mode == SandboxMode::Test {
            return Ok(self.base_command(program, args, working_dir));
        }

        self.create_platform_sandboxed_command(program, args, working_dir)
    }

    pub(super) fn base_command(
        &self,
        program: &str,
        args: &[String],
        working_dir: &Path,
    ) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .current_dir(working_dir)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        // Cargo can be configured (via config or env) to write its target dir outside
        // the session sandbox. Force it back inside the working directory.
        if std::path::Path::new(program)
            .file_name()
            .is_some_and(|n| n == "cargo")
        {
            cmd.env("CARGO_TARGET_DIR", working_dir.join("target"));
        }
        cmd
    }

    fn create_platform_sandboxed_command(
        &self,
        program: &str,
        args: &[String],
        working_dir: &Path,
    ) -> Result<tokio::process::Command> {
        #[cfg(target_os = "linux")]
        {
            // Landlock restricts only the calling thread, so the process-level
            // enforcement at startup does not cover children spawned from tokio
            // worker threads. Restrict each child at spawn time instead: build a
            // ruleset fd from the current scopes and apply it between fork and
            // exec, where the child is still single-threaded.
            let mut cmd = self.base_command(program, args, working_dir);
            if let Some(fd) = self.spawn_landlock_ruleset_fd()? {
                use std::os::fd::AsRawFd;
                // SAFETY: the closure only performs async-signal-safe syscalls
                // (prctl + landlock_restrict_self); the OwnedFd moved into it
                // stays open across fork so the raw fd remains valid in the child.
                unsafe {
                    cmd.pre_exec(move || {
                        super::landlock::apply_landlock_ruleset_in_child(fd.as_raw_fd())
                    });
                }
            }
            Ok(cmd)
        }

        #[cfg(target_os = "macos")]
        {
            // On macOS, wrap each command with sandbox-exec
            let mut full_command = vec![program.to_string()];
            full_command.extend(args.iter().cloned());

            let (sandbox_program, sandbox_args) =
                self.build_macos_sandbox_command(&full_command, working_dir)?;

            Ok(self.base_command(&sandbox_program, &sandbox_args, working_dir))
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            // Windows: route through the Windows sandbox backend. Today that
            // provides Job Object process-tree containment; per-command
            // AppContainer spawn isolation is still pending (R6.3.3).
            #[cfg(target_os = "windows")]
            {
                let scope = self
                    .scopes()
                    .first()
                    .cloned()
                    .unwrap_or_else(|| working_dir.to_path_buf());
                return super::windows::create_windows_sandboxed_command(
                    program,
                    args,
                    working_dir,
                    &scope,
                    &self.read_scopes(),
                );
            }
            // Other non-Linux/macOS platforms (e.g., FreeBSD): run unsandboxed.
            #[cfg(not(target_os = "windows"))]
            Ok(self.base_command(program, args, working_dir))
        }
    }

    /// Build a Landlock ruleset fd from the sandbox's *current* scopes for
    /// restricting one child process at spawn time (via `pre_exec`).
    ///
    /// Returns `Ok(None)` in Test mode, and on kernels without Landlock
    /// support — strict-mode servers already fail closed at startup via
    /// `check_sandbox_prerequisites`, so `None` here only occurs for
    /// in-process embedders, which fall back to application-level path checks.
    #[cfg(target_os = "linux")]
    pub fn spawn_landlock_ruleset_fd(&self) -> Result<Option<std::os::fd::OwnedFd>> {
        if self.mode == SandboxMode::Test {
            return Ok(None);
        }
        let scopes = self.scopes().to_vec();
        let fd = super::landlock::landlock_ruleset_fd(
            &scopes,
            &self.read_scopes(),
            self.is_no_temp_files(),
            self.package_cache_write(),
        )?;
        if fd.is_none() {
            tracing::warn!(
                "Landlock unavailable on this kernel — child process spawned without \
                 kernel-level restrictions (application-level path checks still apply)"
            );
        }
        Ok(fd)
    }

    /// Create a sandboxed shell command (e.g. `bash -c "..."` on Unix,
    /// `powershell -NoProfile -NonInteractive -Command "..."` on Windows).
    pub fn create_shell_command(
        &self,
        shell_program: &str,
        full_command: &str,
        working_dir: &Path,
    ) -> Result<tokio::process::Command> {
        #[cfg(target_os = "windows")]
        let args = vec![
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            full_command.to_string(),
        ];
        #[cfg(not(target_os = "windows"))]
        let args = vec!["-c".to_string(), full_command.to_string()];
        self.create_command(shell_program, &args, working_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// create_command in Test mode delegates directly to base_command.
    #[test]
    fn test_create_command_test_mode_succeeds() {
        let td = tempdir().unwrap();
        let sandbox = Sandbox::new(
            vec![td.path().to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap();
        let result = sandbox.create_command("echo", &["hello".to_string()], td.path());
        assert!(result.is_ok(), "create_command in Test mode should succeed");
    }

    /// create_command recognizes "cargo" and sets CARGO_TARGET_DIR env var.
    #[test]
    fn test_create_command_cargo_sets_target_dir() {
        let td = tempdir().unwrap();
        let sandbox = Sandbox::new(
            vec![td.path().to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap();
        // We can only observe the resulting Command via Debug since the env is private,
        // but at minimum this should not panic and return a valid Command.
        let result = sandbox.create_command("cargo", &["build".to_string()], td.path());
        assert!(result.is_ok(), "create_command for cargo should succeed");
    }

    /// create_shell_command in Test mode produces a valid command.
    #[test]
    fn test_create_shell_command_test_mode_succeeds() {
        let td = tempdir().unwrap();
        let sandbox = Sandbox::new(
            vec![td.path().to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap();

        // Use platform-appropriate shell
        #[cfg(not(target_os = "windows"))]
        let shell = "sh";
        #[cfg(target_os = "windows")]
        let shell = "powershell";

        let result = sandbox.create_shell_command(shell, "echo hello", td.path());
        assert!(
            result.is_ok(),
            "create_shell_command in Test mode should succeed"
        );
    }

    /// create_command in Strict mode on this platform should also succeed
    /// (on macOS wraps with sandbox-exec, on Linux runs directly via Landlock).
    #[test]
    fn test_create_command_strict_mode_succeeds() {
        let td = tempdir().unwrap();
        let sandbox = Sandbox::new(
            vec![td.path().to_path_buf()],
            SandboxMode::Strict,
            false,
            false,
            false,
        )
        .unwrap();
        let result = sandbox.create_command("echo", &["hi".to_string()], td.path());
        assert!(
            result.is_ok(),
            "create_command in Strict mode should succeed: {result:?}"
        );
    }
}
