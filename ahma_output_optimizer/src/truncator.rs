use super::command_classify::{OutputSemantics, classify_command};

pub struct CompressedOutput {
    pub text: String,
    pub tokens_saved_estimate: usize,
}

/// Perform head+tail truncation: keep first `head_lines` and last `tail_lines` of the output.
pub fn head_tail_truncate(text: &str, head_lines: usize, tail_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= head_lines + tail_lines + 5 {
        return text.to_string();
    }

    let head = lines[..head_lines].join("\n");
    let omitted = lines.len() - head_lines - tail_lines;
    let tail = lines[lines.len() - tail_lines..].join("\n");

    let mut result = format!("{}\n[... {} lines omitted ...]\n{}", head, omitted, tail);
    if text.ends_with('\n') && !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

/// Summarizes build and command execution output based on exit code status.
pub fn compress_by_exit_code(
    program: &str,
    exit_code: i32,
    stdout: &str,
    stderr: &str,
    tail_lines: usize,
) -> CompressedOutput {
    let semantics = classify_command(program);
    let original_len = stdout.len() + stderr.len();

    // If it's read-like (e.g., cat, grep, ls), preserve full output even on success
    if semantics == OutputSemantics::ReadLike {
        let combined = if stderr.is_empty() {
            stdout.to_string()
        } else if stdout.is_empty() {
            stderr.to_string()
        } else {
            format!("{}\n{}", stdout, stderr)
        };
        return CompressedOutput {
            text: combined,
            tokens_saved_estimate: 0,
        };
    }

    if exit_code == 0 {
        let summary = format!(
            "✅ Command succeeded (exit 0). [stdout: {} lines, stderr: {} lines]",
            stdout.lines().count(),
            stderr.lines().count()
        );
        // Lint-like commands get a slightly longer success tail to read warnings.
        let tail_count = if semantics == OutputSemantics::LintLike {
            20
        } else {
            5
        };

        let tail: String = stdout
            .lines()
            .rev()
            .take(tail_count)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");

        let text = if tail.is_empty() {
            summary
        } else {
            format!("{}\n{}", summary, tail)
        };

        CompressedOutput {
            tokens_saved_estimate: original_len.saturating_sub(text.len()),
            text,
        }
    } else {
        // Command failed: keep only the error tail.
        let combined = if stderr.is_empty() {
            stdout.to_string()
        } else if stdout.is_empty() {
            stderr.to_string()
        } else {
            format!("{}\n{}", stdout, stderr)
        };

        let error_tail: String = combined
            .lines()
            .rev()
            .take(tail_lines)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");

        let text = format!(
            "❌ Command failed (exit {}). Last {} lines:\n{}",
            exit_code, tail_lines, error_tail
        );

        CompressedOutput {
            tokens_saved_estimate: original_len.saturating_sub(text.len()),
            text,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_head_tail_truncate() {
        let mut lines = Vec::new();
        for i in 1..=100 {
            lines.push(format!("line {i}"));
        }
        let text = lines.join("\n");
        let truncated = head_tail_truncate(&text, 10, 10);
        assert!(truncated.contains("[... 80 lines omitted ...]"));
        assert!(truncated.starts_with("line 1\nline 2"));
        assert!(truncated.ends_with("line 99\nline 100"));
    }

    #[test]
    fn test_compress_by_exit_code_success() {
        let stdout = "compiling file 1...\ncompiling file 2...\ncompiling file 3...\ncompiling file 4...\ncompiling file 5...\nbuild success!\nFinished release target(s)";
        let result = compress_by_exit_code("cargo", 0, stdout, "", 10);
        assert!(result.text.contains("✅ Command succeeded"));
        assert!(result.text.contains("Finished release target(s)"));
        assert!(!result.text.contains("compiling file 1..."));
    }
}
