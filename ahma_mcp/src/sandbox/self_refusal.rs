//! A program that refuses to run inside any sandbox, by its own check (SPEC R-ESCAPE.1).
//!
//! Some macOS tools look for a sandbox themselves and stop before doing anything: Apple's `log`
//! prints `log: Cannot run while sandboxed` and exits, for `log collect` and `log show` alike. No
//! kernel record names a refusal, because the kernel refused nothing, and no grant or setting
//! changes the tool's mind. Field case, 2026-11-13: an agent needed an iPhone's unified log from an
//! hour before (`log collect --device-udid … --start …`), the command came back bare, and the
//! agent fell back to handing the human a `! sudo …` line, which cannot ask for a password.
//!
//! The line is the only evidence, as for a refused Mach registration (`super::mach`): a program
//! that prints it falsely gains nothing but a question to the human, who sees the command.

/// The phrase such a tool prints after its name.
const PHRASE: &str = ": Cannot run while sandboxed";

/// The program that refused to run inside a sandbox, from its output: a line that is exactly
/// `<program>: Cannot run while sandboxed` (surrounding whitespace aside). A line that merely
/// mentions the phrase, or names no program, is none.
pub fn refused_inside_sandbox(stderr: &str, stdout: &str) -> Option<String> {
    stderr.lines().chain(stdout.lines()).find_map(|line| {
        let line = line.trim();
        let program = line.strip_suffix(PHRASE)?.trim();
        let plausible =
            !program.is_empty() && !program.contains(char::is_whitespace) && program.len() <= 64;
        plausible.then(|| program.to_string())
    })
}

/// The first line of the [`super::SandboxError::Unfixable`] it becomes. The hook quotes the
/// `Blocked: ` line in the escape dialog (SPEC R-ESCAPE.1).
pub fn blocked_line(program: &str) -> String {
    format!(
        "Blocked: `{program}` refuses to run inside any sandbox: it checks for one itself and \
         stops before doing anything."
    )
}

/// The explanation that follows [`blocked_line`]: why no grant helps, and what to do instead of
/// handing the human a script or a `!` line.
pub fn remediation(program: &str) -> String {
    format!(
        "{}\n\nNo grant or setting changes this: the refusal is the program's own, not the \
         kernel's. Do not hand the human a script or a `!` line: run this exact command again \
         and, in Claude Code, ahma asks the human once whether to run it outside the sandbox. \
         If the command needs root, write it with `sudo` as usual: ahma then asks for the \
         password in a dialog (macOS).",
        blocked_line(program)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Apple's `log`, verbatim from the field case.
    #[test]
    fn logs_own_refusal_names_the_program() {
        assert_eq!(
            refused_inside_sandbox("log: Cannot run while sandboxed\n", ""),
            Some("log".to_string())
        );
        assert_eq!(
            refused_inside_sandbox("", "  log: Cannot run while sandboxed  "),
            Some("log".to_string())
        );
    }

    #[test]
    fn a_line_that_only_mentions_the_phrase_is_no_refusal() {
        for text in [
            "note: the tool says log: Cannot run while sandboxed sometimes",
            ": Cannot run while sandboxed",
            "error in some thing: Cannot run while sandboxed",
            "Cannot run while sandboxed",
        ] {
            assert_eq!(refused_inside_sandbox(text, ""), None, "{text}");
        }
    }

    #[test]
    fn the_blocked_line_names_the_program_and_the_way_on() {
        let text = remediation("log");
        assert!(text.starts_with("Blocked: `log` refuses"), "{text}");
        assert!(text.contains("run this exact command again"), "{text}");
        assert!(!text.contains("sudo log"), "{text}");
    }
}
