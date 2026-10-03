//! A setuid program the sandbox cannot run (SPEC R6.2.8).
//!
//! `/bin/ps` is setuid root, and the kernel never lets a sandboxed process
//! exec a setuid binary, so `ps` fails with "Operation not permitted" inside
//! every command ahma runs. The agent cannot fix that and should not ask for a
//! grant: `ahma ps` prints the same information from inside the sandbox.

/// Explain a refused `ps`, if the output shows one.
pub fn setuid_denial_note(stderr: &str, stdout: &str) -> Option<String> {
    let hit = stderr
        .lines()
        .chain(stdout.lines())
        .any(looks_like_refused_ps);
    hit.then(|| {
        "`ps` cannot run inside the sandbox: it is setuid root, and no sandbox may run a setuid \
         program (SPEC R6.2.8). This is not a path to grant. Use `ahma ps` (pid, parent, start \
         time, whether it is sandboxed, and the command line), or `pgrep -fl <pattern>`; both \
         answer at once, even while the workspace is busy."
            .to_string()
    })
}

fn looks_like_refused_ps(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("operation not permitted")
        && (lower.contains("/bin/ps")
            || lower.trim_start().starts_with("ps:")
            || lower.contains(": ps:"))
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
}
