//! `sudo`'s password, asked for in a dialog (SPEC R-ESCAPE.4 (b)).
//!
//! An agent's shell has no terminal: Claude Code's Bash tool and its `!` prefix alike answer
//! `sudo` with "a terminal is required to read the password". When the human approves running a
//! `sudo` command once outside the sandbox (R-ESCAPE), the hook gives each leading `sudo` `-A`
//! ([`ahma_common::harness_asks::with_sudo_askpass`]) and points `SUDO_ASKPASS` at a launcher for
//! `ahma askpass`, which shows the macOS password dialog naming the command and prints the answer
//! to sudo through the pipe sudo gave it. ahma never logs, stores or sees the password beyond that.
//!
//! macOS only: elsewhere no rewrite is made and the run fails as before, with the advice to run the
//! command in the human's own terminal.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The first argument that makes `ahma` the askpass helper, dispatched before anything else
/// parses arguments or reads settings: a password prompt must not depend on either.
pub const SUBCOMMAND: &str = "askpass";

/// The environment variable that carries the command being run, for the dialog's text.
pub const COMMAND_ENV: &str = "AHMA_ASKPASS_COMMAND";

/// The AppleScript, one `-e` per line. The text arrives as `item 1 of argv`, never interpolated,
/// so nothing in a command (a quote, a backslash) can change the script.
const SCRIPT: [&str; 4] = [
    "on run argv",
    "set answer to text returned of (display dialog (item 1 of argv) default answer \"\" \
     with hidden answer with title \"ahma\" buttons {\"Cancel\", \"OK\"} default button \"OK\" \
     with icon caution)",
    "return answer",
    "end run",
];

/// What the dialog says: what was approved, and whose password is asked for.
#[must_use]
pub fn dialog_text(prompt: Option<&str>, command: Option<&str>) -> String {
    let what = command.map_or_else(
        || "a command".to_string(),
        |c| format!("this command:\n\n{}", c.trim()),
    );
    let sudo = prompt
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or("Password:");
    format!(
        "You approved running {what}\n\nonce outside ahma's sandbox. sudo asks for your Mac \
         password.\n\n{sudo}"
    )
}

/// The `osascript` arguments that show [`dialog_text`].
#[must_use]
pub fn osascript_args(text: &str) -> Vec<String> {
    let mut args = Vec::new();
    for line in SCRIPT {
        args.push("-e".to_string());
        args.push(line.to_string());
    }
    args.push(text.to_string());
    args
}

/// Show the dialog and print the answer for sudo. Exit status 0 with the password on stdout,
/// 1 when cancelled or when no dialog can be shown (sudo then fails as for a wrong password).
pub fn run(prompt: Option<String>) -> i32 {
    if !cfg!(target_os = "macos") {
        eprintln!(
            "ahma askpass: no password dialog on this platform; run the command in a terminal"
        );
        return 1;
    }
    let command = std::env::var(COMMAND_ENV).ok();
    let text = dialog_text(prompt.as_deref(), command.as_deref());
    let out = match std::process::Command::new("/usr/bin/osascript")
        .args(osascript_args(&text))
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(out) if out.status.success() => out,
        _ => return 1,
    };
    let answer = String::from_utf8_lossy(&out.stdout);
    let answer = answer.strip_suffix('\n').unwrap_or(&answer);
    println!("{answer}");
    0
}

/// The script sudo runs as `SUDO_ASKPASS`: sudo runs it as a bare path with the prompt as its
/// only argument, so it cannot be `ahma askpass` itself.
#[must_use]
pub fn launcher_script(ahma: &Path) -> String {
    let quoted = ahma.display().to_string().replace('\'', r"'\''");
    format!("#!/bin/sh\nexec '{quoted}' {SUBCOMMAND} \"$@\"\n")
}

/// Write [`launcher_script`] owner-only under `dir` and return its path.
pub fn write_launcher(dir: &Path, ahma: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let file = dir.join("ahma-askpass");
    let tmp = dir.join("ahma-askpass.tmp");
    std::fs::write(&tmp, launcher_script(ahma))
        .with_context(|| format!("write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700))?;
    }
    std::fs::rename(&tmp, &file).with_context(|| format!("rename to {}", file.display()))?;
    Ok(file)
}

/// The command an approved run outside the sandbox executes, and whether it needs the askpass
/// launcher: on macOS a command that runs `sudo` gets `-A` on each leading `sudo`.
#[must_use]
pub fn for_unsandboxed_run(command: &str) -> (String, bool) {
    if cfg!(target_os = "macos") && ahma_common::harness_asks::runs_sudo(command) {
        (ahma_common::harness_asks::with_sudo_askpass(command), true)
    } else {
        (command.to_string(), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command is shown, never run as script: a quote or a backslash in it is only text.
    #[test]
    fn the_command_reaches_the_dialog_as_an_argument_only() {
        let command = r#"sudo log collect --start "x" \ ; end run"#;
        let text = dialog_text(Some("Password:"), Some(command));
        assert!(text.contains(command), "{text}");
        let args = osascript_args(&text);
        assert_eq!(args.last(), Some(&text));
        for script_line in args.iter().take(args.len() - 1) {
            assert!(!script_line.contains("log collect"), "{script_line}");
        }
    }

    #[test]
    fn the_launcher_quotes_the_path_it_runs() {
        let script = launcher_script(Path::new("/Users/o'neil/bin/ahma"));
        assert_eq!(
            script,
            "#!/bin/sh\nexec '/Users/o'\\''neil/bin/ahma' askpass \"$@\"\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_launcher_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = write_launcher(
            &dir.path().join("askpass"),
            Path::new("/usr/local/bin/ahma"),
        )
        .unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn only_a_sudo_command_is_rewritten_and_only_on_macos() {
        let (cmd, askpass) = for_unsandboxed_run("cd d && sudo log collect");
        if cfg!(target_os = "macos") {
            assert_eq!(cmd, "cd d && sudo -A log collect");
            assert!(askpass);
        } else {
            assert_eq!(cmd, "cd d && sudo log collect");
            assert!(!askpass);
        }
        assert_eq!(
            for_unsandboxed_run("log collect"),
            ("log collect".to_string(), false)
        );
    }
}
