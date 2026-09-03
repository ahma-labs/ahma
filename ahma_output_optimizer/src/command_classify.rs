use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputSemantics {
    /// Output is informational build noise; safe to compress/truncate aggressively on success
    BuildLike,
    /// Output IS the main result of the command (e.g. cat, ls, grep); always preserve full output
    ReadLike,
    /// Output contains warnings/actionable lints; keep warnings/output even on success
    LintLike,
}

/// Classify command type by program basename to guide compression heuristics.
pub fn classify_command(program: &str) -> OutputSemantics {
    let basename = Path::new(program)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or(program);

    match basename {
        "cargo" | "make" | "gcc" | "g++" | "rustc" | "javac" | "go" | "npm" | "yarn" | "pnpm"
        | "poetry" | "pip" => OutputSemantics::BuildLike,
        "cat" | "head" | "tail" | "grep" | "rg" | "find" | "ls" | "tree" | "git" | "echo"
        | "curl" | "wget" | "diff" => OutputSemantics::ReadLike,
        "clippy" | "eslint" | "pylint" | "mypy" | "shellcheck" | "cargo-clippy" | "flake8" => {
            OutputSemantics::LintLike
        }
        _ => OutputSemantics::BuildLike, // Default to build-like (safe aggressive compression)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_command() {
        assert_eq!(classify_command("cargo"), OutputSemantics::BuildLike);
        assert_eq!(classify_command("/usr/bin/git"), OutputSemantics::ReadLike);
        assert_eq!(classify_command("clippy"), OutputSemantics::LintLike);
        assert_eq!(
            classify_command("unknown_program"),
            OutputSemantics::BuildLike
        );
    }
}
