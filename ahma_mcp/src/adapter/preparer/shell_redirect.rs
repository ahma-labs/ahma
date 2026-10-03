pub(super) fn maybe_append_shell_redirect(program: &str, args: &mut Vec<String>) {
    if let Some(idx) = shell_script_index(program, args.as_slice())
        && let Some(script) = args.get_mut(idx)
    {
        if is_posix_shell(program) {
            group_with_stderr(script);
        } else {
            ensure_shell_redirect(script);
        }
    }
}

/// Merge stderr into stdout for a whole POSIX shell script: `{ script` +
/// newline + `} 2>&1`. Appending ` 2>&1` to the text instead redirected only
/// the last command, was swallowed by a trailing `# comment`, and broke a
/// heredoc whose terminator ended the script (`EOF 2>&1` terminates
/// nothing). The group runs in the same shell, so the exit status is the
/// script's.
fn group_with_stderr(script: &mut String) {
    if script.starts_with("{ ") && script.ends_with("\n} 2>&1") {
        return;
    }
    let body = std::mem::take(script);
    *script = format!("{{ {}\n}} 2>&1", body.trim_end_matches(['\n', '\r']));
}

/// `sh`-family shells, which take a `{ …; }` group. fish, PowerShell and
/// cmd keep the plain suffix.
fn is_posix_shell(program: &str) -> bool {
    matches!(shell_name(program), "sh" | "bash" | "zsh" | "ksh" | "dash")
}

fn shell_script_index(program: &str, args: &[String]) -> Option<usize> {
    if !is_shell_program(program) {
        return None;
    }
    // Unix shells use `-c <script>`; PowerShell uses `-Command <script>`.
    let command_idx = args
        .iter()
        .position(|a| a == "-c" || a.eq_ignore_ascii_case("-command"))?;
    let script_idx = command_idx + 1;
    if script_idx < args.len() {
        Some(script_idx)
    } else {
        None
    }
}

fn ensure_shell_redirect(script: &mut String) {
    if script.trim_end().ends_with("2>&1") {
        return;
    }

    let needs_space = script
        .chars()
        .last()
        .map(|c| !c.is_whitespace())
        .unwrap_or(false);

    if needs_space {
        script.push(' ');
    }
    script.push_str("2>&1");
}

fn is_shell_program(program: &str) -> bool {
    matches!(
        shell_name(program),
        "sh" | "bash" | "zsh" | "fish" | "ksh" | "dash" | "pwsh" | "powershell" | "cmd"
    )
}

/// The program's file name without directory or `.exe`.
fn shell_name(program: &str) -> &str {
    // Strip optional `.exe` suffix (Windows) for comparison.
    let base = program
        .rsplit_once('.')
        .filter(|(_, ext)| ext.eq_ignore_ascii_case("exe"))
        .map(|(stem, _)| stem)
        .unwrap_or(program);
    // Last path component only (handles `/bin/bash`, `C:\Windows\pwsh`, etc.)
    base.rsplit_once(['/', '\\'])
        .map(|(_, n)| n)
        .unwrap_or(base)
}

#[cfg(test)]
mod tests {
    use super::maybe_append_shell_redirect;

    fn run_maybe_append(program: &str, args: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        maybe_append_shell_redirect(program, &mut v);
        v
    }

    /// Run `script` the way ahma prepares it, through a real `sh`.
    #[cfg(unix)]
    fn run_prepared(script: &str) -> (String, bool) {
        let args = run_maybe_append("sh", &["-c", script]);
        let out = std::process::Command::new("sh")
            .args(&args)
            .output()
            .expect("run sh");
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            out.status.success(),
        )
    }

    /// A heredoc whose terminator ends the script still terminates: the
    /// redirect must not land on the terminator line (`EOF 2>&1`), which the
    /// shell does not recognise as one.
    #[cfg(unix)]
    #[test]
    fn a_heredoc_that_ends_the_script_still_terminates() {
        let (out, ok) = run_prepared("cat <<'EOF'\nhello from a heredoc\nEOF");
        assert!(ok, "{out}");
        assert_eq!(out, "hello from a heredoc\n");
    }

    /// Every command's stderr is merged, not only the last one's, and a
    /// trailing comment does not swallow the redirect.
    #[cfg(unix)]
    #[test]
    fn every_command_s_stderr_is_merged() {
        let (out, ok) = run_prepared("echo first >&2; echo second # a comment");
        assert!(ok);
        assert!(out.contains("first") && out.contains("second"), "{out:?}");
    }

    /// The script's own exit status is the command's.
    #[cfg(unix)]
    #[test]
    fn the_exit_status_is_the_script_s() {
        let (_, ok) = run_prepared("true; false");
        assert!(!ok);
    }

    #[test]
    fn non_shell_program_unchanged() {
        let args = run_maybe_append("git", &["status"]);
        assert_eq!(args, vec!["status"]);
    }

    #[test]
    fn shell_sh_adds_redirect() {
        let args = run_maybe_append("/bin/sh", &["-c", "echo hi"]);
        assert_eq!(args, vec!["-c", "{ echo hi\n} 2>&1"]);
    }

    #[test]
    fn shell_bash_adds_redirect() {
        let args = run_maybe_append("/usr/bin/bash", &["-c", "ls"]);
        assert_eq!(args, vec!["-c", "{ ls\n} 2>&1"]);
    }

    #[test]
    fn shell_zsh_adds_redirect() {
        let args = run_maybe_append("zsh", &["-c", "echo test"]);
        assert_eq!(args, vec!["-c", "{ echo test\n} 2>&1"]);
    }

    #[test]
    fn shell_fish_adds_redirect() {
        let args = run_maybe_append("fish", &["-c", "pwd"]);
        assert_eq!(args, vec!["-c", "pwd 2>&1"]);
    }

    #[test]
    fn shell_ksh_adds_redirect() {
        let args = run_maybe_append("ksh", &["-c", "echo ksh"]);
        assert_eq!(args, vec!["-c", "{ echo ksh\n} 2>&1"]);
    }

    #[test]
    fn an_already_grouped_script_is_unchanged() {
        let args = run_maybe_append("sh", &["-c", "{ echo hi\n} 2>&1"]);
        assert_eq!(
            args,
            vec!["-c", "{ echo hi\n} 2>&1"],
            "already grouped: unchanged"
        );
    }

    #[test]
    fn a_trailing_newline_is_not_doubled() {
        let args = run_maybe_append("sh", &["-c", "echo hi\n"]);
        assert_eq!(args, vec!["-c", "{ echo hi\n} 2>&1"]);
    }

    #[test]
    fn powershell_command_flag_adds_redirect() {
        let args = run_maybe_append("pwsh", &["-Command", "Write-Output x"]);
        assert_eq!(args, vec!["-Command", "Write-Output x 2>&1"]);
    }

    #[test]
    fn powershell_command_case_insensitive() {
        let args = run_maybe_append("powershell", &["-command", "Write-Host y"]);
        assert_eq!(args, vec!["-command", "Write-Host y 2>&1"]);
    }

    #[test]
    fn windows_exe_suffix_still_recognized() {
        let args = run_maybe_append("bash.exe", &["-c", "echo win"]);
        assert_eq!(args, vec!["-c", "{ echo win\n} 2>&1"]);
    }

    #[test]
    fn windows_path_with_backslash() {
        let args = run_maybe_append("C:\\Windows\\pwsh.exe", &["-Command", "echo z"]);
        assert_eq!(args, vec!["-Command", "echo z 2>&1"]);
    }

    #[test]
    fn cmd_exe_recognized() {
        let args = run_maybe_append("cmd.exe", &["/c", "echo cmd"]);
        assert_eq!(args, vec!["/c", "echo cmd"]);
    }

    #[test]
    fn no_script_after_minus_c_unchanged() {
        let args = run_maybe_append("sh", &["-c"]);
        assert_eq!(args, vec!["-c"]);
    }

    #[test]
    fn no_minus_c_unchanged() {
        let args = run_maybe_append("sh", &["-e", "script"]);
        assert_eq!(args, vec!["-e", "script"]);
    }
}
