//! A setuid program the sandbox cannot run (SPEC R6.2.8).
//!
//! `/bin/ps` is setuid root, and the kernel never lets a sandboxed process
//! exec a setuid binary, so `ps` fails with "Operation not permitted" inside
//! every command ahma runs. The same holds for every setuid or setgid program
//! (`top`, `sudo`, `su`, `login`, …). The agent cannot fix that and must not
//! be offered a grant: the shell's message names the program's path, and a
//! grant on it would change nothing while widening the sandbox for nothing.
//! [`refused_exec_program`] recognises that message so the denial scanner
//! skips it, and [`setuid_denial_note`] says what to do instead.

use std::path::{Path, PathBuf};

/// Explain a refused setuid program, if the output shows one.
pub fn setuid_denial_note(stderr: &str, stdout: &str) -> Option<String> {
    let mut lines = stderr.lines().chain(stdout.lines());
    if lines.clone().any(looks_like_refused_ps) {
        return Some(
            "`ps` cannot run inside the sandbox: it is setuid root, and no sandbox may run a \
             setuid program (SPEC R6.2.8). This is not a path to grant. Use `ahma ps` (pid, \
             parent, start time, whether it is sandboxed, and the command line), or `pgrep -fl \
             <pattern>`; both answer at once, even while the workspace is busy."
                .to_string(),
        );
    }
    let program = lines.find_map(refused_exec_program)?;
    Some(format!(
        "`{}` cannot run inside the sandbox: it is a setuid or setgid program, and no sandbox \
         may run one (SPEC R6.2.8). This is not a path to grant, and no grant would change it. \
         If its result is needed, ask the human to run it in their own terminal.",
        program.display()
    ))
}

fn looks_like_refused_ps(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("operation not permitted")
        && (lower.contains("/bin/ps")
            || lower.trim_start().starts_with("ps:")
            || lower.contains(": ps:"))
}

/// The setuid or setgid program a shell could not exec, from one line of its
/// output; `None` for any other line.
///
/// The shells' exec-failure shapes are `bash: /bin/ps: Operation not
/// permitted` (also `sh: line 1: …` and `/bin/bash: line 1: …`) and zsh's
/// `zsh: operation not permitted: ps`. A redirect the sandbox refused prints
/// the same shape (`bash: out.txt: Operation not permitted`), and that one
/// **is** a refused write the human may grant. What tells the two apart is the
/// file itself: only a setuid or setgid program is refused for being run.
pub fn refused_exec_program(line: &str) -> Option<PathBuf> {
    refused_exec_program_by(line, &is_setuid_or_setgid)
}

/// [`refused_exec_program`] with the setuid check supplied, so the shell
/// shapes can be tested without a setuid file: inside a sandbox, `chmod u+s`
/// succeeds and silently drops the bit.
fn refused_exec_program_by(line: &str, is_privileged: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    let named = shell_refused_path(line)?;
    let program = resolve_program(named)?;
    is_privileged(&program).then_some(program)
}

/// A setuid program present on this machine, for tests that need the real
/// check: a system one, since a sandboxed test cannot make its own.
#[cfg(all(test, unix))]
pub(crate) fn a_setuid_program() -> Option<PathBuf> {
    [
        "/bin/ps",
        "/usr/bin/passwd",
        "/usr/bin/su",
        "/bin/su",
        "/usr/bin/sudo",
        "/usr/bin/newgrp",
        "/usr/bin/chsh",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| is_setuid_or_setgid(p))
}

/// The path or command word in a shell's "Operation not permitted" line.
fn shell_refused_path(line: &str) -> Option<&str> {
    const SHELLS: &[&str] = &["bash", "sh", "zsh", "dash", "ksh", "mksh"];
    let parts: Vec<&str> = line.trim().split(": ").collect();
    let is_shell = |word: &str| {
        let name = word.rsplit('/').next().unwrap_or(word);
        SHELLS.contains(&name.trim_start_matches('-'))
    };
    let (first, rest) = parts.split_first()?;
    if !is_shell(first) {
        return None;
    }
    // `sh: line 1: …` and `bash: -c: line 0: …` carry a location first.
    let rest: Vec<&str> = rest
        .iter()
        .copied()
        .skip_while(|p| p.starts_with("line ") || *p == "-c")
        .collect();
    match rest.as_slice() {
        // bash, sh, dash, ksh: `<shell>: <path>: Operation not permitted`.
        [path, msg] if msg.eq_ignore_ascii_case("operation not permitted") => Some(path),
        // zsh: `zsh: operation not permitted: <path>`.
        [msg, path] if msg.eq_ignore_ascii_case("operation not permitted") => Some(path),
        _ => None,
    }
}

/// The file a command word names: itself when it has a path separator, else
/// the first match on `PATH`.
fn resolve_program(word: &str) -> Option<PathBuf> {
    let word = word.trim();
    if word.is_empty() {
        return None;
    }
    if word.contains('/') {
        return Some(PathBuf::from(word));
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(word))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(unix)]
fn is_setuid_or_setgid(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o6000 != 0)
}

/// Windows has no setuid bit; nothing is refused for being run that way.
#[cfg(not(unix))]
fn is_setuid_or_setgid(_path: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_ps_points_to_ahma_ps() {
        for line in [
            "bash: /bin/ps: Operation not permitted",
            "sh: line 1: /bin/ps: Operation not permitted",
        ] {
            let note = setuid_denial_note(line, "").unwrap_or_else(|| panic!("{line}"));
            assert!(note.contains("ahma ps") && note.contains("pgrep"), "{note}");
        }
        assert!(setuid_denial_note("cat: x: Operation not permitted", "").is_none());
        assert!(setuid_denial_note("ps: illegal option", "").is_none());
    }

    /// SPEC R6.2.8: every shell's way of saying it could not run a setuid
    /// program is recognised, wherever the program lives.
    #[test]
    fn every_shell_shape_of_a_refused_setuid_exec_is_recognised() {
        let prog = PathBuf::from("/opt/tools/privileged");
        let is_it = |p: &Path| p == prog;
        let p = prog.display();
        for line in [
            format!("bash: {p}: Operation not permitted"),
            format!("sh: line 1: {p}: Operation not permitted"),
            format!("/bin/bash: line 1: {p}: Operation not permitted"),
            format!("bash: -c: line 0: {p}: Operation not permitted"),
            format!("zsh: operation not permitted: {p}"),
        ] {
            assert_eq!(
                refused_exec_program_by(&line, &is_it),
                Some(prog.clone()),
                "{line}"
            );
        }
    }

    /// The note names the program and refuses the grant, for a real setuid
    /// program on this machine.
    #[cfg(unix)]
    #[test]
    fn a_refused_setuid_program_gets_a_note_not_a_grant() {
        let Some(prog) = a_setuid_program() else {
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            panic!("macOS and Linux always ship a setuid program");
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            return;
        };
        let line = format!("bash: {}: Operation not permitted", prog.display());
        assert_eq!(refused_exec_program(&line), Some(prog.clone()));
        let note = setuid_denial_note(&line, "").expect("a note");
        assert!(note.contains("not a path to grant"), "{note}");
    }

    /// The same message for a redirect into an ordinary file is a refused
    /// write the human may grant: it must not be mistaken for an exec.
    #[cfg(unix)]
    #[test]
    fn a_refused_redirect_into_an_ordinary_file_is_not_an_exec() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.txt");
        std::fs::write(&out, "").unwrap();
        let line = format!("bash: {}: Operation not permitted", out.display());
        assert_eq!(refused_exec_program(&line), None);
        assert_eq!(setuid_denial_note(&line, ""), None);
        let missing = dir.path().join("not-yet-created.txt");
        let line = format!("bash: {}: Operation not permitted", missing.display());
        assert_eq!(
            refused_exec_program(&line),
            None,
            "a file the redirect creates"
        );
    }

    /// Only a shell's own message counts: a tool that names a setuid file in
    /// its own error was not refused an exec by the shell.
    #[test]
    fn a_tool_naming_a_setuid_file_is_not_a_refused_exec() {
        let prog = PathBuf::from("/opt/tools/privileged");
        let is_it = |p: &Path| p == prog;
        for line in [
            format!("cp: {}: Operation not permitted", prog.display()),
            format!("bash: {}: Permission denied", prog.display()),
            format!("bash: {}: Operation not permitted extra", prog.display()),
        ] {
            assert_eq!(refused_exec_program_by(&line, &is_it), None, "{line}");
        }
    }
}
