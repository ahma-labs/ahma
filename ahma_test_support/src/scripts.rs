//! One helper for tests that need to run a small script on every platform.
//!
//! A test that wants three lines of output, or a write to stderr, or a loop,
//! cannot express it as one command string that works in both `bash` and
//! PowerShell — and ahma runs commands through `platform_shell_program()`, so
//! Windows gets PowerShell. Historically the response was to write the `bash`
//! form and add `#[cfg(unix)]`, which trades the coverage away rather than
//! earning it: the behaviour under test (log chunking, stderr streaming, exit
//! codes) has nothing platform-specific about it, and Windows is the platform
//! where ahma's own sandbox story is weakest and most needs the coverage.
//!
//! SPEC R6.3.8 requires such tests to supply both forms "using a uniform helper
//! method … to ensure consistency and prevent platform-specific leaks". This is
//! that helper. It was previously a private copy inside
//! `ahma_mcp/tests/unit/log_monitor_integration_test.rs`, which is the shape the
//! requirement exists to prevent — one file obeying a rule that binds all of
//! them (AGENTS.md).
//!
//! Note the deliberate asymmetry with [`crate::path_helpers`]: this writes a
//! real file, so callers must pass a `tempfile::tempdir()` path, never a
//! location in the repo tree.

use std::path::Path;

/// Write a script in the current platform's language and return the command
/// string that runs it.
///
/// Both forms are always required, so a caller cannot quietly support one
/// platform: an empty `ps1` is a visible decision at the call site rather than
/// an invisible `#[cfg]` above it.
///
/// # Panics
///
/// If the script cannot be written — a test whose fixture cannot be created has
/// nothing left to assert.
pub fn write_cross_platform_script(dir: &Path, name_base: &str, bash: &str, ps1: &str) -> String {
    let (program, args) = cross_platform_script_command(dir, name_base, bash, ps1);
    std::iter::once(program)
        .chain(args)
        .collect::<Vec<_>>()
        .join(" ")
}

/// [`write_cross_platform_script`] split into program and argv.
///
/// Needed by callers that pass the two separately — an MTDF tool definition's
/// `command` / `args` pair, for instance — which cannot re-split a joined
/// string without guessing about spaces in the temp path.
pub fn cross_platform_script_command(
    dir: &Path,
    name_base: &str,
    bash: &str,
    ps1: &str,
) -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        let _ = bash;
        let path = dir.join(format!("{name_base}.ps1"));
        std::fs::write(&path, ps1)
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
        (
            "powershell".to_string(),
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-File".to_string(),
                path.to_string_lossy().into_owned(),
            ],
        )
    }
    #[cfg(not(windows))]
    {
        let _ = ps1;
        let path = dir.join(format!("{name_base}.sh"));
        std::fs::write(&path, bash)
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
        ("bash".to_string(), vec![path.display().to_string()])
    }
}

/// A script that prints `lines` distinct lines to stdout, one per line.
///
/// The single most common shape a test needs and the one that most often
/// produced a `#[cfg(unix)]`, because `printf 'a\nb\nc\n'` has no one-command
/// equivalent under PowerShell.
pub fn write_multiline_script(dir: &Path, name_base: &str, lines: &[&str]) -> String {
    let (program, args) = multiline_script_command(dir, name_base, lines);
    std::iter::once(program)
        .chain(args)
        .collect::<Vec<_>>()
        .join(" ")
}

/// [`write_multiline_script`] split into program and argv, for callers that pass
/// the two separately (an MTDF `command` / `args` pair, for instance).
pub fn multiline_script_command(
    dir: &Path,
    name_base: &str,
    lines: &[&str],
) -> (String, Vec<String>) {
    let bash = format!(
        "#!/bin/bash\n{}\n",
        lines
            .iter()
            .map(|l| format!("echo '{}'", l.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let ps1 = format!(
        "{}\n",
        lines
            .iter()
            .map(|l| format!("Write-Output '{}'", l.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join("\n")
    );
    cross_platform_script_command(dir, name_base, &bash, &ps1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn a_multiline_script_prints_one_line_per_entry_on_this_platform() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cmd = write_multiline_script(dir.path(), "three", &["err1", "err2", "err3"]);

        // Run it through the same shell ahma would use, so the assertion is
        // about what a test using this helper will actually observe.
        let output = if cfg!(windows) {
            Command::new("powershell")
                .args(["-NoProfile", "-Command", &cmd])
                .output()
        } else {
            Command::new("bash").args(["-c", &cmd]).output()
        }
        .expect("the shell under test is present on every supported platform");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(
            lines,
            vec!["err1", "err2", "err3"],
            "helper must produce exactly one line per entry; got {stdout:?}"
        );
    }
}
