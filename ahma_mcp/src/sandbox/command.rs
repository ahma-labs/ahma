use anyhow::Result;
use std::path::Path;
use std::sync::RwLock;

use super::core::Sandbox;
use super::types::SandboxMode;

/// Operator-configured passthrough allowlist (`[sandbox] env_allow`): variable
/// names preserved in tool subprocess environments despite matching a secret
/// pattern. Set once at startup from the loaded settings; empty by default so
/// the safe posture (scrub everything secret-looking) holds even when nothing
/// is configured. Process-global because it is a single operator policy, not
/// per-session state.
static SECRET_ENV_ALLOW: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// Install the passthrough allowlist from `[sandbox] env_allow`. Called once at
/// startup. Names are stored as-is; matching is case-insensitive.
pub fn set_secret_env_allow(allow: Vec<String>) {
    if let Ok(mut guard) = SECRET_ENV_ALLOW.write() {
        *guard = allow;
    }
}

/// Return `true` if `name` looks like it holds a secret and should be scrubbed
/// from tool subprocess environments.
///
/// Uses unambiguous substring markers plus a `_`-delimited `TOKEN` segment
/// check, so genuine credentials (`ANTHROPIC_API_KEY`, `AWS_SECRET_ACCESS_KEY`,
/// `GITHUB_TOKEN`) match while non-secret look-alikes (`TOKENIZERS_PARALLELISM`)
/// do not.
pub fn is_secret_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    const MARKERS: [&str; 8] = [
        "API_KEY",
        "APIKEY",
        "ACCESS_KEY",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "PASSPHRASE",
        "CREDENTIAL",
    ];
    if MARKERS.iter().any(|m| upper.contains(m)) {
        return true;
    }
    if upper.contains("PRIVATE") && upper.contains("KEY") {
        return true;
    }
    // `TOKEN` only as a whole `_`-delimited segment, so `TOKENIZERS_PARALLELISM`
    // (a non-secret ML flag) is not swept up but `GITHUB_TOKEN` is.
    upper.split('_').any(|seg| seg == "TOKEN")
}

/// Given an iterator of environment variable names and an allowlist, return the
/// names that must be scrubbed: secret-looking and not allow-listed
/// (case-insensitive exact match). Pure — the unit of behaviour under test.
pub fn secret_env_keys<I>(names: I, allow: &[String]) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let allow_upper: Vec<String> = allow.iter().map(|a| a.to_ascii_uppercase()).collect();
    names
        .into_iter()
        .filter(|n| is_secret_env_name(n) && !allow_upper.contains(&n.to_ascii_uppercase()))
        .collect()
}

/// Remove secret-bearing environment variables from `cmd`, honoring the operator
/// `[sandbox] env_allow` passthrough list. Every place that spawns a tool
/// subprocess must call this: the kernel sandbox restricts the filesystem, not
/// environment inheritance, so a child would otherwise inherit the server's
/// `ANTHROPIC_API_KEY`, `AWS_*`, etc. and could dump them via `env`.
pub fn scrub_secret_env(cmd: &mut tokio::process::Command, context: &str) {
    let allow = SECRET_ENV_ALLOW
        .read()
        .map(|g| g.clone())
        .unwrap_or_default();
    let scrubbed = secret_env_keys(std::env::vars().map(|(k, _)| k), &allow);
    if !scrubbed.is_empty() {
        tracing::debug!(
            "Scrubbed {} secret-bearing env var(s) from {context}: {:?}",
            scrubbed.len(),
            scrubbed
        );
        for key in &scrubbed {
            cmd.env_remove(key);
        }
    }
}

/// The `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` variables that route every sandboxed
/// subprocess through the local guarded egress proxy (`--restrict-network`,
/// R-NET). Empty unless the operator turned restriction on; set once at startup
/// after the proxy binds its port. Process-global for the same reason as
/// [`SECRET_ENV_ALLOW`]: it is a single operator policy, and it must reach every
/// spawn site — `base_command` *and* the shell pool — none of which share a
/// `Sandbox` handle.
static EGRESS_PROXY_ENV: RwLock<Vec<(String, String)>> = RwLock::new(Vec::new());

/// Install the egress-proxy environment (from a started [`super::super::egress::EgressProxy`]).
/// Called once at server startup when `--restrict-network` is on. Passing an empty
/// vec (the default) leaves subprocess egress unrestricted.
pub fn set_egress_proxy_env(vars: Vec<(String, String)>) {
    if let Ok(mut guard) = EGRESS_PROXY_ENV.write() {
        *guard = vars;
    }
}

/// Inject the egress-proxy variables into `cmd`. A no-op when restriction is off.
/// Applied *after* [`scrub_secret_env`] at every subprocess spawn site so a tool
/// that honours `HTTP_PROXY` reaches the network only through the allow-listed,
/// SSRF-guarded proxy. (A tool that ignores the proxy variables is not contained
/// by this alone — see the network-restriction limitations in the README.)
pub fn apply_egress_proxy_env(cmd: &mut tokio::process::Command) {
    let vars = EGRESS_PROXY_ENV
        .read()
        .map(|g| g.clone())
        .unwrap_or_default();
    apply_proxy_vars(cmd, &vars);
}

/// Pure core of [`apply_egress_proxy_env`]: set `vars` on `cmd` (a no-op when
/// empty). Split out so the injection is unit-testable without the process-global.
fn apply_proxy_vars(cmd: &mut tokio::process::Command, vars: &[(String, String)]) {
    if !vars.is_empty() {
        cmd.envs(vars.iter().cloned());
    }
}

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

        // Scrub ahma's internal supervision markers so they never leak into the
        // commands ahma runs. Without this, a tool/user command (or a test) that
        // itself launches `ahma serve` would inherit a bogus server-child role and
        // spawn depth, making the nested server skip its bridge or trip the
        // spawn-depth backstop. These vars are set deliberately only on intentional
        // `ahma serve` children (spawn_background_bridge / SubprocessPeerFactory).
        cmd.env_remove("AHMA_SERVER_CHILD")
            .env_remove(ahma_common::process_guard::SPAWN_DEPTH_ENV)
            .env_remove("AHMA_RESTARTED");

        // Scrub secret-bearing environment variables so a sandboxed (or
        // prompt-injected) tool cannot read the server's credentials out of its
        // own process environment and exfiltrate them. The kernel sandbox
        // restricts the filesystem, not environment inheritance — `env`,
        // `printenv`, and `/proc/self/environ` all read process memory the
        // sandbox cannot gate. Names on the operator's `[sandbox] env_allow`
        // list are preserved for tools that legitimately need a token.
        scrub_secret_env(&mut cmd, program);

        // Route the subprocess through the guarded egress proxy when
        // `--restrict-network` is on (a no-op otherwise). After the scrub so the
        // proxy vars are never mistaken for secrets.
        apply_egress_proxy_env(&mut cmd);

        // Run each command as its own process-group leader so the whole tree can
        // be killed as a unit on timeout/cancellation. On macOS the direct child
        // is `sandbox-exec`, which execs `sh -c "<cmd>"`, which may fan out to
        // `cargo` → `rustc` → `cc`; signalling only the direct child (what
        // `kill_on_drop`/`Child::kill` do) orphans those descendants, leaking
        // build processes until they finish. With a dedicated group the adapter
        // can `kill(-pgid)` the entire subtree (see `kill_process_tree`).
        #[cfg(unix)]
        cmd.process_group(0);

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

    #[test]
    fn apply_proxy_vars_injects_and_empty_is_noop() {
        // Empty vars → no env is set on the command.
        let mut cmd = tokio::process::Command::new("true");
        apply_proxy_vars(&mut cmd, &[]);
        assert_eq!(
            cmd.as_std().get_envs().count(),
            0,
            "empty proxy vars must not touch the command environment"
        );

        // Non-empty vars → each is set explicitly on the command.
        let vars = vec![
            ("HTTP_PROXY".to_string(), "http://127.0.0.1:9".to_string()),
            (
                "NO_PROXY".to_string(),
                "127.0.0.1,::1,localhost".to_string(),
            ),
        ];
        let mut cmd = tokio::process::Command::new("true");
        apply_proxy_vars(&mut cmd, &vars);
        let set: std::collections::HashMap<_, _> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(
            set.get("HTTP_PROXY").and_then(|v| v.as_deref()),
            Some("http://127.0.0.1:9")
        );
        assert_eq!(
            set.get("NO_PROXY").and_then(|v| v.as_deref()),
            Some("127.0.0.1,::1,localhost")
        );
    }

    fn make_test_sandbox(scope: &std::path::Path) -> Sandbox {
        Sandbox::new(
            vec![scope.to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap()
    }

    #[test]
    fn secret_env_names_match_real_credentials() {
        for name in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_ACCESS_KEY_ID",
            "AWS_SESSION_TOKEN",
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "MY_SECRET",
            "DB_PASSWORD",
            "SIGNING_PASSPHRASE",
            "GCP_CREDENTIALS",
            "SSH_PRIVATE_KEY",
            "apikey",
        ] {
            assert!(
                is_secret_env_name(name),
                "{name} should be treated as secret"
            );
        }
    }

    #[test]
    fn benign_env_names_are_not_secret() {
        for name in [
            "PATH",
            "HOME",
            "LANG",
            "CARGO_HOME",
            "RUSTUP_TOOLCHAIN",
            "TOKENIZERS_PARALLELISM", // contains "TOKEN" but not as a segment
            "KEYBOARD_LAYOUT",
            "ACCESSIBILITY",
        ] {
            assert!(
                !is_secret_env_name(name),
                "{name} should not be treated as secret"
            );
        }
    }

    #[test]
    fn secret_env_keys_respects_allowlist_case_insensitively() {
        let names = vec![
            "ANTHROPIC_API_KEY".to_string(),
            "GITHUB_TOKEN".to_string(),
            "PATH".to_string(),
        ];
        // Allow GITHUB_TOKEN through (lowercase in config to prove case-insensitivity).
        let allow = vec!["github_token".to_string()];
        let scrubbed = secret_env_keys(names, &allow);
        assert!(scrubbed.contains(&"ANTHROPIC_API_KEY".to_string()));
        assert!(
            !scrubbed.contains(&"GITHUB_TOKEN".to_string()),
            "allow-listed var must survive"
        );
        assert!(
            !scrubbed.contains(&"PATH".to_string()),
            "non-secret var must survive"
        );
    }

    /// A tool subprocess must not inherit a secret-looking env var.
    #[test]
    fn base_command_scrubs_secret_env_from_child() {
        // SAFETY: single-threaded test; sets/removes a uniquely-named var.
        unsafe {
            std::env::set_var("AHMA_TEST_FAKE_API_KEY", "sk-should-be-scrubbed");
            std::env::set_var("AHMA_TEST_PLAIN_VAR", "keepme");
        }
        let td = tempdir().unwrap();
        let sandbox = make_test_sandbox(td.path());
        let cmd = sandbox.base_command("env", &[], td.path());
        let child_env: std::collections::HashMap<String, String> = cmd
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| {
                // Only keys with an explicit override appear here; a scrubbed key
                // shows up as (key, None).
                Some((
                    k.to_string_lossy().into_owned(),
                    v?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        // The scrubbed key must be explicitly removed (present as a removal).
        let removed: Vec<String> = cmd
            .as_std()
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert!(
            removed.contains(&"AHMA_TEST_FAKE_API_KEY".to_string()),
            "secret var must be scrubbed (removed) from child env; removed={removed:?}"
        );
        assert!(
            !child_env.contains_key("AHMA_TEST_PLAIN_VAR"),
            "non-secret var is inherited normally (not explicitly overridden)"
        );
        unsafe {
            std::env::remove_var("AHMA_TEST_FAKE_API_KEY");
            std::env::remove_var("AHMA_TEST_PLAIN_VAR");
        }
    }

    /// create_command in Test mode delegates directly to base_command.
    #[test]
    fn test_create_command_test_mode_succeeds() {
        let td = tempdir().unwrap();
        let sandbox = make_test_sandbox(td.path());
        let result = sandbox.create_command("echo", &["hello".to_string()], td.path());
        assert!(result.is_ok(), "create_command in Test mode should succeed");
    }

    /// create_shell_command in Test mode produces a valid command.
    #[test]
    fn test_create_shell_command_test_mode_succeeds() {
        let td = tempdir().unwrap();
        let sandbox = make_test_sandbox(td.path());

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
