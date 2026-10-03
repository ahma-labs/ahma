//! Windows AppContainer sandbox tests (SPEC R6.3).
//!
//! Two halves, deliberately:
//!
//! * The **cross-platform** half exercises the parts of `sandbox::windows` that
//!   are ordinary logic — the launcher argv protocol, container naming, launcher
//!   resolution — and runs everywhere. Those pieces are the ones a developer on
//!   macOS or Linux can actually break, so they are the ones that must not be
//!   parked behind `#[cfg(windows)]`.
//! * The **Windows-only** half is the real R6.3.3 proof: a write outside the
//!   locked scope must fail at the OS level, and an in-scope write must still
//!   succeed. Its three behavioural tests are `#[ignore]`d because no
//!   `windows-latest` run has yet shown the in-scope write succeeding, and why
//!   is not yet known. `appcontainer_dacl_diagnostics` is the instrument for
//!   finding out.
//!
//! Anything that mutates process-global AppContainer state (`plan_windows_sandboxed_spawn`,
//! `cleanup_windows_sandbox`) is confined to `appcontainer_*` tests, which clean
//! up after themselves.

use ahma_mcp::sandbox::windows::{
    LAUNCHER_ARGV0, appcontainer_has_default_read, appcontainer_name_for_scope, build_command_line,
    decode_grant_journal, encode_grant_journal, launcher_args, parse_launcher_args,
    quote_windows_arg, resolve_launcher_exe_from,
};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Cross-platform: the launcher protocol
// ---------------------------------------------------------------------------

/// The launcher is a second process in the chain, exactly like `sandbox-exec` on
/// macOS, so the argv it is handed is a wire format between two ahma processes.
/// If it stops round-tripping, commands run with the wrong program or the wrong
/// container — and a wrong container silently means a *different* set of granted
/// paths.
#[test]
fn launcher_argv_is_a_stable_round_trip() {
    let program = "powershell";
    let args = vec![
        "-NoProfile".to_string(),
        "-NonInteractive".to_string(),
        "-Command".to_string(),
        "Set-Content -Path 'C:\\ws\\a b.txt' -Value \"x\"".to_string(),
    ];
    let container = appcontainer_name_for_scope(Path::new("C:\\ws"));

    let mut argv = vec!["C:\\Program Files\\ahma\\ahma.exe".to_string()];
    argv.extend(launcher_args(&container, program, &args));

    let parsed = parse_launcher_args(argv)
        .expect("well-formed re-entry must parse")
        .expect("marker present means this is a re-entry");
    assert_eq!(parsed.container, container);
    assert_eq!(parsed.program, program);
    assert_eq!(
        parsed.args, args,
        "quoting-sensitive arguments must survive the launcher hop untouched"
    );
}

/// A normal `ahma serve stdio` invocation must not be mistaken for a re-entry —
/// that would swallow the CLI.
#[test]
fn normal_invocations_are_not_launcher_reentries() {
    for argv in [
        vec!["ahma".to_string(), "serve".to_string(), "stdio".to_string()],
        vec!["ahma".to_string()],
        vec!["ahma".to_string(), "--version".to_string()],
        // A user argument that merely *contains* the marker is not a re-entry:
        // only argv[1] counts.
        vec![
            "ahma".to_string(),
            "run".to_string(),
            LAUNCHER_ARGV0.to_string(),
        ],
    ] {
        assert_eq!(
            parse_launcher_args(argv.clone()).expect("must not error"),
            None,
            "ordinary invocation misread as a launcher re-entry: {argv:?}"
        );
    }
}

/// SPEC R7: ahma never silently runs unsandboxed. A re-entry that starts with the
/// marker but is malformed must be a hard error — falling through to the normal
/// CLI would execute the command *outside* the AppContainer.
#[test]
fn a_broken_launcher_reentry_never_falls_through_to_the_cli() {
    let argv = vec![
        "ahma".to_string(),
        LAUNCHER_ARGV0.to_string(),
        "ahma-sandbox-0000000000000000".to_string(),
        // `--` separator missing.
        "powershell".to_string(),
    ];
    assert!(
        parse_launcher_args(argv).is_err(),
        "a malformed re-entry must fail loudly, not degrade to an unsandboxed run"
    );
}

/// The container name is what the launcher re-derives the SID from, and what the
/// crash-recovery sweep matches on. It has to be stable across processes and
/// legal as a Windows AppContainer profile name.
#[test]
fn container_names_are_stable_scoped_and_windows_legal() {
    let a = appcontainer_name_for_scope(Path::new("C:\\Users\\dev\\project"));
    let b = appcontainer_name_for_scope(Path::new("C:\\Users\\dev\\project"));
    let other = appcontainer_name_for_scope(Path::new("C:\\Users\\dev\\other"));

    assert_eq!(
        a, b,
        "the launcher gets only the name, so it must be stable"
    );
    assert_ne!(a, other, "two workspaces must not share one container");
    assert!(a.len() <= 64, "Windows rejects names longer than 64 chars");
    assert!(
        a.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.'),
        "name must be alphanumeric plus `-`/`.`: {a}"
    );
    assert!(
        a.starts_with("ahma-sandbox-"),
        "leftover profiles must be attributable to ahma: {a}"
    );
}

/// A test binary lives in `target\debug\deps\` and has no launcher dispatch of
/// its own, so it must find the real `ahma` executable or refuse to spawn.
#[test]
fn launcher_resolution_finds_ahma_from_a_test_binary() {
    let td = tempfile::tempdir().unwrap();
    let debug = td.path().join("debug");
    let deps = debug.join("deps");
    std::fs::create_dir_all(&deps).unwrap();
    let ahma = debug.join("ahma");
    std::fs::write(&ahma, b"").unwrap();

    let resolved = resolve_launcher_exe_from(
        &deps.join("windows_sandbox_integration_test-0123abcd"),
        &[],
        "ahma",
        &|p: &Path| p.is_file(),
    );
    assert_eq!(resolved, Some(ahma));
}

#[test]
fn launcher_resolution_reports_absence_rather_than_guessing() {
    let td = tempfile::tempdir().unwrap();
    let lonely = td.path().join("nowhere").join("some-binary");
    assert_eq!(
        resolve_launcher_exe_from(&lonely, &[], "ahma", &|p: &Path| p.is_file()),
        None,
        "with no launcher available the caller must fail closed, not improvise"
    );
}

/// `CreateProcessW` takes one flat string and the child rebuilds `argv` with
/// `CommandLineToArgvW`. Mis-quoting here changes the command the sandbox runs,
/// which is a correctness *and* a security problem.
#[test]
fn command_line_quoting_survives_paths_spaces_and_quotes() {
    assert_eq!(quote_windows_arg("plain"), "plain");
    assert_eq!(quote_windows_arg("C:\\ws\\file.txt"), "C:\\ws\\file.txt");
    assert_eq!(
        quote_windows_arg("C:\\Program Files\\x"),
        "\"C:\\Program Files\\x\""
    );
    // A trailing backslash needs doubling only inside quotes, where it would
    // otherwise escape the closing quote and swallow the following argument.
    assert_eq!(quote_windows_arg("C:\\ws\\"), "C:\\ws\\");
    assert_eq!(
        quote_windows_arg("C:\\Program Files\\ws\\"),
        "\"C:\\Program Files\\ws\\\\\""
    );
    assert_eq!(quote_windows_arg("say \"hi\""), "\"say \\\"hi\\\"\"");

    assert_eq!(
        build_command_line("powershell", &["-Command".into(), "echo a b".into()]),
        "powershell -Command \"echo a b\""
    );
}

// ---------------------------------------------------------------------------
// Cross-platform: crash-recovery journal and system-directory policy
// ---------------------------------------------------------------------------

/// The journal is what turns "ahma was SIGKILLed mid-session" from "your source
/// tree keeps a stray ACE forever" into "the next run cleans it up".
#[test]
fn grant_journal_round_trips_windows_paths() {
    let paths = vec![
        PathBuf::from("C:\\Users\\dev\\project"),
        PathBuf::from("C:\\Users\\dev\\.cargo\\registry"),
        PathBuf::from("D:\\scratch"),
    ];
    let (container, decoded) = decode_grant_journal(&encode_grant_journal(
        "ahma-sandbox-feedfacecafebeef",
        &paths,
    ))
    .expect("a journal we just wrote must decode");
    assert_eq!(container, "ahma-sandbox-feedfacecafebeef");
    assert_eq!(decoded, paths);
}

/// A truncated or empty journal must decode to `None` rather than to a container
/// named "" — the sweep would otherwise derive a bogus SID and revoke nothing
/// while reporting success.
#[test]
fn a_corrupt_journal_decodes_to_nothing() {
    assert_eq!(decode_grant_journal(""), None);
    assert_eq!(decode_grant_journal("   \n"), None);
}

/// ahma must not touch system directory ACLs: it has no right to, the attempt
/// would need administrator privileges, and the AppContainer can already read
/// them via the default `ALL APPLICATION PACKAGES` ACE.
#[test]
fn system_directories_are_left_alone() {
    let system_dirs = vec![
        PathBuf::from("C:\\Windows"),
        PathBuf::from("C:\\Program Files"),
        PathBuf::from("C:\\Program Files (x86)"),
    ];
    for already_readable in [
        "C:\\Windows",
        "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
        "c:\\program files\\Git\\cmd",
    ] {
        assert!(
            appcontainer_has_default_read(Path::new(already_readable), &system_dirs),
            "{already_readable} must be recognised as already readable"
        );
    }
    for needs_a_grant in [
        "C:\\Users\\dev\\project",
        // Prefix-similar but a different directory: a plain string prefix test
        // would wrongly skip granting this one.
        "C:\\WindowsProjects\\app",
        "C:\\Program FilesX\\thing",
    ] {
        assert!(
            !appcontainer_has_default_read(Path::new(needs_a_grant), &system_dirs),
            "{needs_a_grant} must still get an explicit grant"
        );
    }
}

// ---------------------------------------------------------------------------
// Windows-only: the actual boundary (SPEC R6.3.1, R6.3.3)
//
// Executed on windows-latest; the boundary is not yet shown to hold. See
// `appcontainer_dacl_diagnostics` for what is and is not known.
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod appcontainer {
    use ahma_common::timeouts::TestTimeouts;
    use ahma_mcp::sandbox::windows::{
        PROBE_DONE_KEY, ProbeCase, ProbeOp, appcontainer_name_for_scope,
        check_windows_sandbox_available, cleanup_windows_sandbox, parse_probe_report,
        plan_windows_sandboxed_spawn, probe_args, probe_case_key, resolve_launcher_exe,
    };
    use ahma_mcp::sandbox::{Sandbox, SandboxMode};
    use ahma_mcp::test_utils::cli::build_binary_cached;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;

    /// R6.3.1: on any supported Windows the AppContainer API and the launcher
    /// must both be present, or strict mode has to fail closed at startup.
    #[test]
    fn appcontainer_prerequisites_are_available() {
        match check_windows_sandbox_available() {
            Ok(()) => {}
            Err(e) => panic!(
                "Windows sandbox prerequisites unmet on a windows-latest runner: {e}. \
                 Either the AppContainer API is missing (pre-Windows 8) or the ahma.exe \
                 launcher was not built alongside the test binary."
            ),
        }
    }

    /// The plan must point at the launcher and carry the real command behind the
    /// `--` separator; it must never hand back the raw program, which would be an
    /// unsandboxed spawn wearing a sandbox's name.
    #[test]
    fn a_spawn_plan_routes_through_the_launcher() {
        let scope = tempfile::tempdir().unwrap();
        let plan = plan_windows_sandboxed_spawn(
            "powershell",
            &["-Command".to_string(), "exit 0".to_string()],
            &[scope.path().to_path_buf()],
            &[],
        )
        .expect("planning a spawn inside a writable temp scope must succeed");

        assert!(
            plan.launcher
                .file_name()
                .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("ahma.exe")),
            "the sandboxed spawn must go through ahma.exe, got {:?}",
            plan.launcher
        );
        assert_eq!(
            plan.args.first().map(String::as_str),
            Some(ahma_mcp::sandbox::windows::LAUNCHER_ARGV0)
        );
        assert!(
            plan.args.iter().any(|a| a == "--"),
            "the command must be separated from launcher arguments: {:?}",
            plan.args
        );
        assert!(
            plan.env.iter().any(|(k, _)| k == "TEMP"),
            "%TEMP% must be redirected into the container folder, or every tool \
             that writes a temp file fails with an unexplained access denial"
        );

        cleanup_windows_sandbox();
    }

    /// Before the scope is locked there is nothing to build a container from, and
    /// running anyway would be running unconfined (R6.3.4 / R7).
    #[test]
    fn planning_without_a_locked_scope_fails_closed() {
        let result =
            plan_windows_sandboxed_spawn("powershell", &[], &[] as &[PathBuf], &[] as &[PathBuf]);
        assert!(
            result.is_err(),
            "a spawn with no write scope must be refused, never run unconfined"
        );
    }

    // -----------------------------------------------------------------------
    // The R6.3.3 diagnostic
    // -----------------------------------------------------------------------

    /// How long one child may run before it is killed and reported as timed out.
    /// Inside the container, PowerShell 5.1 was observed taking tens of seconds
    /// to fail to load its modules.
    fn child_timeout() -> std::time::Duration {
        TestTimeouts::scale_secs(15)
    }

    /// What one child process produced, with the probe report parsed out of its
    /// stdout.
    struct ChildRun {
        exit: Option<i32>,
        stdout: String,
        stderr: String,
        report: Vec<(String, String)>,
        /// Set when there is no exit code to report: the spawn failed, the plan
        /// failed, or the child timed out.
        failure: Option<String>,
    }

    impl ChildRun {
        fn failed(why: String) -> Self {
            Self {
                exit: None,
                stdout: String::new(),
                stderr: String::new(),
                report: Vec::new(),
                failure: Some(why),
            }
        }

        fn value(&self, key: &str) -> Option<&str> {
            self.report
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        }

        fn finished(&self) -> bool {
            self.value(PROBE_DONE_KEY).is_some()
        }

        fn status(&self) -> String {
            match (&self.failure, self.exit) {
                (Some(why), _) => why.clone(),
                (None, Some(code)) => format!("exit {code}"),
                (None, None) => "exit code unavailable".to_string(),
            }
        }
    }

    /// Run `program` directly, as a host process, killing it after
    /// [`child_timeout`]. `cwd: None` inherits the test's working directory.
    async fn run_child(
        program: &Path,
        args: &[String],
        env: &[(String, String)],
        cwd: Option<&Path>,
    ) -> ChildRun {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in env {
            cmd.env(key, value);
        }
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        match tokio::time::timeout(child_timeout(), cmd.output()).await {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                ChildRun {
                    exit: output.status.code(),
                    report: parse_probe_report(&stdout),
                    stdout,
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    failure: None,
                }
            }
            Ok(Err(e)) => ChildRun::failed(format!("spawn failed: {e}")),
            Err(_) => ChildRun::failed(format!("timed out after {:?}", child_timeout())),
        }
    }

    /// Run `program args` inside the AppContainer for `scope`, through the same
    /// `plan_windows_sandboxed_spawn` launcher plan `Sandbox::create_command`
    /// would use. Called directly because `create_command` consults
    /// `appcontainer_spawn_enabled()`, which is `false`, and would quietly run the
    /// Job-Object-only path instead.
    async fn run_contained(
        scope: &Path,
        program: &str,
        args: &[String],
        extra_env: &[(String, String)],
        cwd: Option<&Path>,
    ) -> ChildRun {
        let plan = match plan_windows_sandboxed_spawn(program, args, &[scope.to_path_buf()], &[]) {
            Ok(plan) => plan,
            Err(e) => return ChildRun::failed(format!("plan failed: {e:#}")),
        };
        let mut env = plan.env.clone();
        env.extend(extra_env.iter().cloned());
        run_child(&plan.launcher, &plan.args, &env, cwd).await
    }

    /// The result table, plus the harness expectations that failed.
    #[derive(Default)]
    struct Diagnostics {
        rows: Vec<[String; 4]>,
        failures: Vec<String>,
    }

    impl Diagnostics {
        fn row(&mut self, round: &str, probe: &str, key: &str, value: impl Into<String>) {
            self.rows.push([
                round.to_string(),
                probe.to_string(),
                key.to_string(),
                value.into(),
            ]);
        }

        /// Record a harness expectation. Collected rather than asserted on the
        /// spot, so one broken expectation never hides the rest of the table.
        fn expect(&mut self, ok: bool, what: impl Into<String>) {
            if !ok {
                self.failures.push(what.into());
            }
        }

        /// Print a child's raw output now, and add its report to the table.
        fn record(&mut self, round: &str, probe: &str, run: &ChildRun) {
            println!("---- [{round}] {probe}: {}", run.status());
            println!("stdout:\n{}", run.stdout);
            if !run.stderr.trim().is_empty() {
                println!("stderr:\n{}", run.stderr);
            }
            self.row(round, probe, "status", run.status());
            // The launcher's own failure (`ahma: could not launch ...`) is the
            // one line of stderr that belongs in the table.
            if let Some(line) = run.stderr.lines().find(|l| l.starts_with("ahma:")) {
                self.row(round, probe, "launcher", line.trim());
            }
            for (key, value) in &run.report {
                if key != PROBE_DONE_KEY {
                    self.row(round, probe, key, value.clone());
                }
            }
            let done = if run.finished() {
                "yes"
            } else {
                "NO: the probe did not run to the end"
            };
            self.row(round, probe, PROBE_DONE_KEY, done);
        }

        fn print_table(&self) {
            let mut width = ["round".len(), "probe".len(), "key".len()];
            for row in &self.rows {
                for (w, cell) in width.iter_mut().zip(row.iter()) {
                    *w = (*w).max(cell.len());
                }
            }
            println!("==== R6.3.3 AppContainer diagnostics: results ====");
            println!(
                "{:<w0$}  {:<w1$}  {:<w2$}  value",
                "round",
                "probe",
                "key",
                w0 = width[0],
                w1 = width[1],
                w2 = width[2]
            );
            for [round, probe, key, value] in &self.rows {
                println!(
                    "{round:<w0$}  {probe:<w1$}  {key:<w2$}  {value}",
                    w0 = width[0],
                    w1 = width[1],
                    w2 = width[2]
                );
            }
        }
    }

    /// The self-probe's cases. File names carry `tag`, so two probes in one round
    /// never see each other's files.
    fn self_probe_cases(
        scope: &Path,
        outside: &Path,
        container_temp: Option<&Path>,
        tag: &str,
    ) -> Vec<ProbeCase> {
        let mut cases = vec![
            ProbeCase::new(
                "in_write",
                ProbeOp::Write,
                scope.join(format!("in-{tag}.txt")),
            ),
            ProbeCase::new("in_read", ProbeOp::Read, scope.join("seed.txt")),
            ProbeCase::new("in_list", ProbeOp::List, scope),
            ProbeCase::new("in_canon", ProbeOp::Canonicalize, scope),
            ProbeCase::new(
                "out_write",
                ProbeOp::Write,
                outside.join(format!("out-{tag}.txt")),
            ),
            ProbeCase::new("out_read", ProbeOp::Read, outside.join("secret.txt")),
            ProbeCase::new("nul_write", ProbeOp::Write, r"\\.\NUL"),
        ];
        if let Some(temp) = container_temp {
            cases.push(ProbeCase::new(
                "temp_write",
                ProbeOp::Write,
                temp.join(format!("temp-{tag}.txt")),
            ));
        }
        if let Some(parent) = scope.parent() {
            cases.push(ProbeCase::new("parent_list", ProbeOp::List, parent));
        }
        // Each ancestor's own metadata: a lowbox token can open a full path
        // without traverse rights, but anything that stats an ancestor needs them.
        for (i, ancestor) in scope.ancestors().skip(1).enumerate() {
            cases.push(ProbeCase::new(
                format!("anc{i}_stat"),
                ProbeOp::Stat,
                ancestor,
            ));
        }
        cases
    }

    /// Every case the harness asked for must come back, and the token must be
    /// the one the harness meant to test. These are what the harness controls;
    /// the outcomes of the cases are the unknown, and are not asserted.
    fn expect_complete_probe(
        d: &mut Diagnostics,
        round: &str,
        probe: &str,
        run: &ChildRun,
        cases: &[ProbeCase],
        appcontainer: bool,
    ) {
        d.expect(
            run.finished(),
            format!(
                "[{round}] {probe}: the probe did not run to the end ({})",
                run.status()
            ),
        );
        for case in cases {
            let key = probe_case_key(&case.label);
            d.expect(
                run.value(&key).is_some(),
                format!("[{round}] {probe}: no report for {key}"),
            );
        }
        let want = if appcontainer { "true" } else { "false" };
        let got = run.value("token.is_appcontainer");
        d.expect(
            got == Some(want),
            format!("[{round}] {probe}: token.is_appcontainer was {got:?}, expected {want}"),
        );
    }

    /// `cmd.exe` redirect probe: needs no PowerShell module, only `cmd.exe` and
    /// the file system. Every case prints `AHMA-PROBE cmd.<label>=ok` or
    /// `=fail`, because a failed redirection triggers `||` without necessarily
    /// setting `ERRORLEVEL` (printed too, with delayed expansion).
    ///
    /// Each token is its own argument. `cmd.exe` reads the raw command line and
    /// does not understand `\"`, so the only thing `quote_windows_arg` may ever
    /// quote is a path with a space in it, which `cmd.exe` handles.
    fn cmd_probe_args(scope: &Path, outside: &Path, tag: &str) -> Vec<String> {
        let path = |p: PathBuf| p.to_string_lossy().into_owned();
        let redirect = |p: String| vec!["echo".to_string(), "x>".to_string(), p];
        let cases: Vec<(&str, Vec<String>)> = vec![
            (
                "in_write",
                redirect(path(scope.join(format!("cmd-in-{tag}.txt")))),
            ),
            (
                "in_read",
                vec!["type".to_string(), path(scope.join("seed.txt"))],
            ),
            (
                "out_write",
                redirect(path(outside.join(format!("cmd-out-{tag}.txt")))),
            ),
            (
                "out_read",
                vec!["type".to_string(), path(outside.join("secret.txt"))],
            ),
            ("nul_write", redirect("NUL".to_string())),
        ];
        let mut argv: Vec<String> = [
            "/d",
            "/v:on",
            "/c",
            "echo",
            "AHMA-PROBE",
            "cmd.cwd=!CD!",
            "&",
        ]
        .map(String::from)
        .to_vec();
        for (label, action) in cases {
            argv.push("(".to_string());
            argv.extend(action);
            argv.extend([")", "&&", "(", "echo", "AHMA-PROBE"].map(String::from));
            argv.push(format!("cmd.{label}=ok"));
            argv.extend([")", "||", "(", "echo", "AHMA-PROBE"].map(String::from));
            argv.push(format!("cmd.{label}=fail"));
            argv.extend(["errorlevel=!ERRORLEVEL!", ")", "&"].map(String::from));
        }
        argv.extend(["echo", "AHMA-PROBE", "done=1"].map(String::from));
        argv
    }

    /// Why PowerShell's modules fail inside the container. `Import-Module` is in
    /// `Microsoft.PowerShell.Core`, which is always loaded, and output goes
    /// through `[Console]::Out` because `Write-Output` is exactly what failed.
    /// No double quotes, so the argument survives `quote_windows_arg` untouched.
    const PS_MODULE_PROBE: &str = "$ErrorActionPreference = 'Continue'; \
        $o = [Console]::Out; \
        $o.WriteLine('AHMA-PROBE ps.version=' + $PSVersionTable.PSVersion); \
        $o.WriteLine('AHMA-PROBE ps.cwd=' + [Environment]::CurrentDirectory); \
        $o.WriteLine('AHMA-PROBE ps.PSModulePath=' + $env:PSModulePath); \
        $o.WriteLine('AHMA-PROBE ps.LOCALAPPDATA=' + $env:LOCALAPPDATA); \
        foreach ($m in 'Microsoft.PowerShell.Utility', 'Microsoft.PowerShell.Management') { \
          try { Import-Module $m -Verbose -ErrorAction Stop; $o.WriteLine('AHMA-PROBE ps.import.' + $m + '=ok') } \
          catch { $o.WriteLine('AHMA-PROBE ps.import.' + $m + '=fail ' + ($_.Exception.ToString() -replace '\\s+', ' ')) } \
        }; \
        $i = 0; foreach ($e in $Error) { $o.WriteLine('AHMA-PROBE ps.error' + $i + '=' + ($e.ToString() -replace '\\s+', ' ')); $i++ }; \
        $Error[0] | Format-List * -Force; \
        $o.WriteLine('AHMA-PROBE done=1')";

    fn ps_args(bypass: bool) -> Vec<String> {
        let mut args = vec!["-NoProfile".to_string(), "-NonInteractive".to_string()];
        if bypass {
            args.extend(["-ExecutionPolicy".to_string(), "Bypass".to_string()]);
        }
        args.extend(["-Command".to_string(), PS_MODULE_PROBE.to_string()]);
        args
    }

    /// Raw `icacls` output, printed (not tabled): the ACE on the scope, and in
    /// the first round what each ancestor grants.
    fn show_icacls(path: &Path) {
        match std::process::Command::new("icacls").arg(path).output() {
            Ok(o) => println!(
                "---- icacls {}\n{}",
                path.display(),
                String::from_utf8_lossy(&o.stdout)
            ),
            Err(e) => println!("---- icacls {}: could not run: {e}", path.display()),
        }
    }

    /// One round: a fresh scope and a sibling "outside" directory under `base`
    /// (`None`: `%TEMP%`), optionally with a Low integrity label on the scope,
    /// probed from inside the container. `full` adds the host control, the
    /// inherited-cwd variant, the PowerShell probes and the ancestor `icacls`.
    async fn diagnose_round(
        d: &mut Diagnostics,
        ahma: &Path,
        round: &str,
        base: Option<&Path>,
        low_integrity: bool,
        full: bool,
    ) {
        let make = || match base {
            Some(b) => tempfile::tempdir_in(b),
            None => tempfile::tempdir(),
        };
        let (scope, outside) = match (make(), make()) {
            (Ok(s), Ok(o)) => (s, o),
            (Err(e), _) | (_, Err(e)) => {
                d.row(round, "setup", "tempdir", format!("err:{e}"));
                d.expect(
                    base.is_some(),
                    format!("[{round}] could not create a temp scope: {e}"),
                );
                return;
            }
        };
        let scope_path = scope.path().to_path_buf();
        let outside_path = outside.path().to_path_buf();
        std::fs::write(scope_path.join("seed.txt"), b"seed").expect("seed the scope");
        std::fs::write(outside_path.join("secret.txt"), b"secret").expect("seed outside");
        d.row(round, "setup", "scope", scope_path.display().to_string());
        d.row(
            round,
            "setup",
            "outside",
            outside_path.display().to_string(),
        );

        if low_integrity {
            let summary = match std::process::Command::new("icacls")
                .arg(&scope_path)
                .args(["/setintegritylevel", "(OI)(CI)low"])
                .output()
            {
                Ok(o) => format!(
                    "exit {:?}: {}",
                    o.status.code(),
                    String::from_utf8_lossy(&o.stdout).trim()
                ),
                Err(e) => format!("could not run: {e}"),
            };
            d.row(round, "setup", "icacls_low_label", summary);
        }

        // Plan once up front: this creates the profile and writes the ACE. Every
        // later plan for the same scope reuses them.
        let prep = match plan_windows_sandboxed_spawn(
            "cmd.exe",
            &[],
            std::slice::from_ref(&scope_path),
            &[],
        ) {
            Ok(plan) => plan,
            Err(e) => {
                d.row(round, "setup", "plan", format!("FAILED: {e:#}"));
                d.expect(
                    false,
                    format!("[{round}] planning the AppContainer spawn failed: {e:#}"),
                );
                return;
            }
        };
        let container_temp = prep
            .env
            .iter()
            .find(|(k, _)| k == "TEMP")
            .map(|(_, v)| PathBuf::from(v));
        d.row(
            round,
            "setup",
            "container",
            appcontainer_name_for_scope(&scope_path),
        );
        d.row(
            round,
            "setup",
            "container_temp",
            container_temp
                .as_ref()
                .map_or_else(|| "<none>".to_string(), |p| p.display().to_string()),
        );
        if full {
            let mut chain: Vec<&Path> = scope_path.ancestors().collect();
            chain.reverse();
            for path in chain {
                show_icacls(path);
            }
        } else {
            show_icacls(&scope_path);
        }

        let ahma_str = ahma.to_string_lossy().into_owned();

        if full {
            // Control: the same probe on the host, outside any container. These
            // outcomes are known, so this is what proves the probe itself works.
            let cases = self_probe_cases(&scope_path, &outside_path, None, "host");
            let run = run_child(ahma, &probe_args(&cases), &[], Some(scope_path.as_path())).await;
            d.record(round, "probe(host)", &run);
            expect_complete_probe(d, round, "probe(host)", &run, &cases, false);
            for label in ["in_write", "in_read", "out_write", "out_read", "nul_write"] {
                let got = run.value(&probe_case_key(label));
                d.expect(
                    got.is_some_and(|v| v.starts_with("ok")),
                    format!(
                        "[{round}] probe(host): {label} must succeed outside any container, \
                         got {got:?}"
                    ),
                );
            }
        }

        // The evidence: the probe inside the container, in the scope as its
        // working directory, as `Sandbox::create_command` would run a tool.
        let cases = self_probe_cases(&scope_path, &outside_path, container_temp.as_deref(), "ac");
        let mut probe = "probe";
        let mut run = run_contained(
            &scope_path,
            &ahma_str,
            &probe_args(&cases),
            &[],
            Some(scope_path.as_path()),
        )
        .await;
        d.record(round, probe, &run);
        if !run.finished() {
            // The container may be unable to load the image itself. Retry from a
            // copy inside the scope, which inherits the scope's ACE.
            let copy = scope_path.join("ahma-probe.exe");
            match std::fs::copy(ahma, &copy) {
                Ok(_) => {
                    probe = "probe(copy in scope)";
                    run = run_contained(
                        &scope_path,
                        &copy.to_string_lossy(),
                        &probe_args(&cases),
                        &[],
                        Some(scope_path.as_path()),
                    )
                    .await;
                    d.record(round, probe, &run);
                }
                Err(e) => d.row(round, "probe(copy in scope)", "copy", format!("err:{e}")),
            }
        }
        expect_complete_probe(d, round, probe, &run, &cases, true);
        d.row(
            round,
            probe,
            "host_sees.in_write",
            scope_path.join("in-ac.txt").exists().to_string(),
        );
        d.row(
            round,
            probe,
            "host_sees.out_write",
            outside_path.join("out-ac.txt").exists().to_string(),
        );

        if full {
            // One factor changed: the working directory is inherited (the test's
            // own, outside every scope), as it was in the earlier PowerShell runs.
            let cases = self_probe_cases(&scope_path, &outside_path, None, "ac-cwd");
            let run = run_contained(&scope_path, &ahma_str, &probe_args(&cases), &[], None).await;
            d.record(round, "probe(cwd inherited)", &run);
        }

        let tag = round.replace(|c: char| !c.is_ascii_alphanumeric(), "-");
        let run = run_contained(
            &scope_path,
            "cmd.exe",
            &cmd_probe_args(&scope_path, &outside_path, &tag),
            &[],
            Some(scope_path.as_path()),
        )
        .await;
        d.record(round, "cmd", &run);
        d.row(
            round,
            "cmd",
            "host_sees.in_write",
            scope_path
                .join(format!("cmd-in-{tag}.txt"))
                .exists()
                .to_string(),
        );
        d.row(
            round,
            "cmd",
            "host_sees.out_write",
            outside_path
                .join(format!("cmd-out-{tag}.txt"))
                .exists()
                .to_string(),
        );

        if full {
            let run = run_contained(
                &scope_path,
                "powershell",
                &ps_args(false),
                &[],
                Some(scope_path.as_path()),
            )
            .await;
            d.record(round, "powershell", &run);

            // The candidate fix, measured rather than assumed: no execution-policy
            // lookup, and the module analysis cache inside the container folder.
            let cache_env: Vec<(String, String)> = container_temp
                .iter()
                .map(|t| {
                    (
                        "PSModuleAnalysisCachePath".to_string(),
                        t.join("ModuleAnalysisCache").to_string_lossy().into_owned(),
                    )
                })
                .collect();
            let run = run_contained(
                &scope_path,
                "powershell",
                &ps_args(true),
                &cache_env,
                Some(scope_path.as_path()),
            )
            .await;
            d.record(round, "powershell(bypass+cache)", &run);

            let run = run_contained(
                &scope_path,
                "pwsh",
                &ps_args(false),
                &[],
                Some(scope_path.as_path()),
            )
            .await;
            d.record(round, "pwsh", &run);
        }

        cleanup_windows_sandbox();
    }

    /// Not a gate: evidence for the open R6.3.3 question, from a harness that
    /// checks itself.
    ///
    /// The previous version of this test drove everything through Windows
    /// PowerShell 5.1, and on `windows-latest` (run 37103491457) PowerShell
    /// could not resolve `Write-Output`, `Set-Content` or `Test-Path` inside the
    /// container: its own modules failed to load, and `whoami /groups` printed
    /// nothing. So it never reached the write it was meant to observe, and it
    /// asserted nothing, so it passed anyway. What it did establish: the ACE is
    /// written and inheritable, and the scope's ancestors grant the container
    /// nothing.
    ///
    /// This version measures without a shell where it can:
    ///
    /// * **`ahma.exe`'s own probe** (`PROBE_ARGV0`), launched through the real
    ///   launcher into the real container. It opens in-scope, out-of-scope, `NUL`
    ///   and the redirected `%TEMP%` for write and read, stats each ancestor, and
    ///   reports the raw `GetLastError` code for each, plus the token it holds:
    ///   AppContainer flag, integrity level, groups and capabilities. If the image
    ///   cannot be loaded from `target\`, it retries from a copy inside the scope.
    /// * **A host control**: the same probe outside any container, whose outcomes
    ///   are known.
    /// * **`cmd.exe`** redirects, which need no PowerShell module.
    /// * **PowerShell** `Import-Module` with the exception text, plain and with
    ///   `-ExecutionPolicy Bypass` and a container-local module analysis cache,
    ///   and `pwsh` 7.
    ///
    /// It varies one factor per round: the scope under `%TEMP%`, the same with a
    /// Low integrity label on the scope, and the scope under `%RUNNER_TEMP%`
    /// (`D:\a\_temp` on GitHub runners) when that is set. One labelled result
    /// table is printed at the end.
    ///
    /// It asserts only what the harness controls: the plan builds, the host
    /// control succeeds where it must, and every probe runs to the end, reports
    /// every case and holds the token it was meant to (AppContainer inside, not
    /// outside). Whether any in-container case succeeds is the open question,
    /// and is reported, not asserted.
    ///
    /// Run by the `AppContainer diagnostics` step in build.yml, which is
    /// `continue-on-error`: a red run still prints the table.
    #[ignore = "diagnostic evidence for SPEC R6.3.3; run explicitly (build.yml's \
                AppContainer diagnostics step does)"]
    #[tokio::test]
    async fn appcontainer_dacl_diagnostics() {
        // The probe must be today's `ahma.exe`: an older one has no probe mode and
        // would hand the argv to clap. `build_binary_cached` rebuilds a stale one
        // (R-TEST-PATH.2). The launcher is resolved separately, exactly as a real
        // spawn resolves it, and only needs the unchanged launcher protocol.
        let ahma = build_binary_cached("ahma_bin", "ahma");
        let launcher = match resolve_launcher_exe() {
            Ok(path) => path,
            Err(e) => panic!("the ahma.exe launcher must be built alongside the test binary: {e}"),
        };
        println!("==== AppContainer diagnostics (SPEC R6.3.3) ====");
        println!("probe:    {}", ahma.display());
        println!("launcher: {}", launcher.display());

        let mut d = Diagnostics::default();
        d.row("all", "setup", "probe_exe", ahma.display().to_string());
        d.row(
            "all",
            "setup",
            "launcher_exe",
            launcher.display().to_string(),
        );
        diagnose_round(&mut d, &ahma, "temp", None, false, true).await;
        diagnose_round(&mut d, &ahma, "temp+low-label", None, true, false).await;
        match std::env::var_os("RUNNER_TEMP").map(PathBuf::from) {
            Some(base) if base.is_dir() => {
                diagnose_round(&mut d, &ahma, "runner_temp", Some(&base), false, false).await;
            }
            _ => d.row(
                "runner_temp",
                "setup",
                "skipped",
                "RUNNER_TEMP is not set (not a GitHub Actions runner)",
            ),
        }

        d.print_table();
        println!("==== end diagnostics ====");
        assert!(
            d.failures.is_empty(),
            "the diagnostic harness failed. The table above is evidence either way; these \
             are not about the boundary but about the harness:\n{}",
            d.failures.join("\n")
        );
    }

    /// **R6.3.3, the gate.** A shell redirect outside the scope must be stopped by
    /// the kernel, and the equivalent write inside the scope must still work — a
    /// sandbox that blocks everything proves nothing.
    ///
    /// **Failed on `windows-latest` CI** on the in-scope half, with PowerShell's
    /// `Access to the path '...' is denied`, which names no path. Whether that
    /// was the container denying the target or PowerShell failing on its own
    /// state is not known: in a later run PowerShell could not even load its own
    /// modules inside the container. The out-of-scope half passing proves
    /// nothing while the in-scope half fails, which is exactly the "blocks
    /// everything, proves nothing" case this test exists to rule out.
    /// `appcontainer_dacl_diagnostics` measures the boundary without depending
    /// on PowerShell; remove this `#[ignore]` once a `windows-latest` run
    /// demonstrates the in-scope write succeeding.
    #[ignore = "AppContainer isolation is off (SPEC R6.3.3 not yet proven): the in-scope \
                write failed on windows-latest, cause unknown (see appcontainer_dacl_diagnostics)"]
    #[tokio::test]
    async fn writes_outside_the_scope_are_blocked_and_inside_still_work() {
        let scope = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let forbidden = outside.path().join("pwned.txt");
        let permitted = scope.path().join("allowed.txt");

        let sandbox = Sandbox::new(
            vec![scope.path().to_path_buf()],
            SandboxMode::Strict,
            false,
            false,
            false,
        )
        .expect("strict sandbox over a temp scope");

        let escape = format!(
            "Set-Content -LiteralPath '{}' -Value 'hacked'",
            forbidden.display()
        );
        let mut cmd = sandbox
            .create_shell_command("powershell", &escape, scope.path())
            .expect("building a sandboxed shell command must succeed");
        let _ = cmd.output().await;

        assert!(
            !forbidden.exists(),
            "SECURITY: AppContainer did not block a write to {} (SPEC R6.3.3)",
            forbidden.display()
        );

        let allowed = format!(
            "Set-Content -LiteralPath '{}' -Value 'ok'",
            permitted.display()
        );
        let mut cmd = sandbox
            .create_shell_command("powershell", &allowed, scope.path())
            .expect("building a sandboxed shell command must succeed");
        let output = cmd.output().await.expect("in-scope command must run");
        assert!(
            permitted.exists(),
            "in-scope write must succeed under AppContainer; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        cleanup_windows_sandbox();
    }

    /// Windows reads are confined too — unlike macOS, where the platform forces a
    /// denylist (SPEC R6.2.2). An AppContainer that cannot open a file outside the
    /// scope is the difference R6.3.9 is waiting on.
    ///
    /// **Ignored, not passing for the wrong reason**: this used to pass on
    /// `windows-latest` CI while the in-scope write was failing too, so it
    /// proved nothing — see
    /// `writes_outside_the_scope_are_blocked_and_inside_still_work` above. Now
    /// that Windows spawns fall back to `base_command` (Job-Object-only, no
    /// path confinement per SPEC R6.3.9), this read genuinely succeeds and the
    /// assertion below is honestly false. Remove this `#[ignore]` alongside the
    /// other two once AppContainer is wired back in and proven.
    #[ignore = "SPEC R6.3.9 not yet satisfied: reads are not confined without AppContainer, \
                which is disabled until R6.3.3 is shown to hold (see sandbox::command)"]
    #[tokio::test]
    async fn reads_outside_the_scope_are_blocked() {
        let scope = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, b"classified").unwrap();

        let sandbox = Sandbox::new(
            vec![scope.path().to_path_buf()],
            SandboxMode::Strict,
            false,
            false,
            false,
        )
        .expect("strict sandbox over a temp scope");

        let read = format!("Get-Content -LiteralPath '{}'", secret.display());
        let mut cmd = sandbox
            .create_shell_command("powershell", &read, scope.path())
            .expect("building a sandboxed shell command must succeed");
        let output = cmd.output().await.expect("command must run");

        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("classified"),
            "SECURITY: AppContainer let a command read outside the scope (SPEC R6.3.9)"
        );

        cleanup_windows_sandbox();
    }

    /// Cleanup must give the user's ACLs back. Asserted through behaviour rather
    /// than by parsing a DACL: after cleanup the container has no access, so a
    /// second sandbox over the same scope has to re-grant from scratch and still
    /// work — which only holds if the revoke really happened and the grant is
    /// genuinely idempotent.
    ///
    /// **Failed on `windows-latest` CI** for the same reason as
    /// `writes_outside_the_scope_are_blocked_and_inside_still_work` above: the
    /// first session's in-scope write already fails, cause unknown. Remove this
    /// `#[ignore]` alongside that one.
    #[ignore = "AppContainer isolation is off (SPEC R6.3.3 not yet proven) — see \
                writes_outside_the_scope_are_blocked_and_inside_still_work"]
    #[tokio::test]
    async fn cleanup_revokes_and_a_fresh_session_regrants() {
        let scope = tempfile::tempdir().unwrap();
        let marker = scope.path().join("second-session.txt");

        for _ in 0..2 {
            let sandbox = Sandbox::new(
                vec![scope.path().to_path_buf()],
                SandboxMode::Strict,
                false,
                false,
                false,
            )
            .expect("strict sandbox over a temp scope");
            let write = format!(
                "Set-Content -LiteralPath '{}' -Value 'ok'",
                marker.display()
            );
            let mut cmd = sandbox
                .create_shell_command("powershell", &write, scope.path())
                .expect("building a sandboxed shell command must succeed");
            let output = cmd.output().await.expect("command must run");
            assert!(
                marker.exists(),
                "in-scope write must succeed on every session; stderr: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            std::fs::remove_file(&marker).unwrap();
            cleanup_windows_sandbox();
        }
    }
}
