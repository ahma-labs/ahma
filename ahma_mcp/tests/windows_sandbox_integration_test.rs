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
//!   succeed. It has never been executed — the implementation was written on a
//!   macOS host — so a failure here on the first `windows-latest` run is
//!   information, not a regression.
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
// NEVER EXECUTED as of the commit that added them — written on a macOS host.
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod appcontainer {
    use ahma_mcp::sandbox::windows::{
        check_windows_sandbox_available, cleanup_windows_sandbox, plan_windows_sandboxed_spawn,
    };
    use ahma_mcp::sandbox::{Sandbox, SandboxMode};
    use std::path::PathBuf;

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

    /// **R6.3.3, the gate.** A shell redirect outside the scope must be stopped by
    /// the kernel, and the equivalent write inside the scope must still work — a
    /// sandbox that blocks everything proves nothing.
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
