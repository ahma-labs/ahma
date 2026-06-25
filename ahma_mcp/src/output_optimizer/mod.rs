pub mod ansi_strip;
pub mod command_classify;
pub mod deduplicator;
pub mod fingerprint;
pub mod pressure;
pub mod truncator;

pub use ansi_strip::strip_ansi_and_carriage_returns;
pub use command_classify::{OutputSemantics, classify_command};
pub use deduplicator::LineDeduplicator;
pub use fingerprint::OutputFingerprinter;
pub use pressure::{PressureGovernor, PressureLevel, estimate_tokens};
pub use truncator::{compress_by_exit_code, head_tail_truncate};

#[derive(Debug)]
pub struct OutputOptimizer {
    pub enabled: bool,
    pub deduplicator: LineDeduplicator,
    pub fingerprinter: OutputFingerprinter,
    pub governor: PressureGovernor,
    pub session_tokens_used: usize,
}

impl OutputOptimizer {
    pub fn new(enabled: bool, context_window: Option<usize>) -> Self {
        Self {
            enabled,
            deduplicator: LineDeduplicator::new(),
            fingerprinter: OutputFingerprinter::new(),
            governor: PressureGovernor::new(context_window),
            session_tokens_used: 0,
        }
    }

    /// Process a streaming line: strip ANSI and run line deduplication.
    pub fn process_streaming_line(&mut self, line: &str) -> Vec<String> {
        if !self.enabled {
            return vec![line.to_string()];
        }
        let clean = strip_ansi_and_carriage_returns(line);
        if clean.is_empty() {
            return vec![];
        }
        self.deduplicator.process(&clean)
    }

    /// Process final completed output and summarize.
    pub fn finalize_output(
        &mut self,
        program: &str,
        exit_code: i32,
        stdout: &str,
        stderr: &str,
    ) -> String {
        let clean_stdout = if self.enabled {
            strip_ansi_and_carriage_returns(stdout)
        } else {
            stdout.to_string()
        };
        let clean_stderr = if self.enabled {
            strip_ansi_and_carriage_returns(stderr)
        } else {
            stderr.to_string()
        };

        if !self.enabled {
            if clean_stderr.is_empty() {
                return clean_stdout;
            } else if clean_stdout.is_empty() {
                return clean_stderr;
            } else {
                return format!("{}\n{}", clean_stdout, clean_stderr);
            }
        }

        // 1. Exit-code aware compression & command semantics
        let pressure = self.governor.get_pressure_level(self.session_tokens_used);
        let tail_lines = match pressure {
            PressureLevel::Relaxed => 100,
            PressureLevel::Moderate => 50,
            PressureLevel::Elevated => 25,
            PressureLevel::Critical => 10,
        };

        let compressed =
            compress_by_exit_code(program, exit_code, &clean_stdout, &clean_stderr, tail_lines);

        // 2. Head+tail truncation for very long output
        let (head_limit, tail_limit) = match pressure {
            PressureLevel::Relaxed => (30, 70),
            PressureLevel::Moderate => (20, 50),
            PressureLevel::Elevated => (10, 30),
            PressureLevel::Critical => (5, 15),
        };
        let truncated = head_tail_truncate(&compressed.text, head_limit, tail_limit);

        // 3. Fingerprinting / Change detection
        let final_text =
            if let Some(fingerprint_msg) = self.fingerprinter.check_change(program, &truncated) {
                fingerprint_msg
            } else {
                truncated
            };

        // 4. Update session token usage estimate
        let turn_tokens = estimate_tokens(&final_text);
        self.session_tokens_used += turn_tokens;

        final_text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── OutputOptimizer::new ───────────────────────────────────────────────

    #[test]
    fn new_disabled_uses_default_context_window() {
        let opt = OutputOptimizer::new(false, None);
        assert!(!opt.enabled);
        assert_eq!(opt.session_tokens_used, 0);
        assert_eq!(opt.governor.context_window_size, 32768);
    }

    #[test]
    fn new_enabled_with_custom_context_window() {
        let opt = OutputOptimizer::new(true, Some(65536));
        assert!(opt.enabled);
        assert_eq!(opt.governor.context_window_size, 65536);
    }

    // ── process_streaming_line ─────────────────────────────────────────────

    #[test]
    fn process_streaming_line_disabled_returns_original_line_with_ansi() {
        let mut opt = OutputOptimizer::new(false, None);
        let raw = "hello \x1b[31mworld\x1b[0m";
        // Disabled: line is returned as-is, ANSI codes NOT stripped
        let result = opt.process_streaming_line(raw);
        assert_eq!(result, vec![raw.to_string()]);
    }

    #[test]
    fn process_streaming_line_enabled_strips_ansi_sequences() {
        let mut opt = OutputOptimizer::new(true, None);
        let result = opt.process_streaming_line("hello \x1b[31mworld\x1b[0m");
        assert_eq!(result, vec!["hello world".to_string()]);
    }

    #[test]
    fn process_streaming_line_enabled_empty_string_returns_empty_vec() {
        let mut opt = OutputOptimizer::new(true, None);
        // Empty string strips to empty → early-return empty branch
        let result = opt.process_streaming_line("");
        assert_eq!(result, Vec::<String>::new());
    }

    #[test]
    fn process_streaming_line_enabled_pure_ansi_becomes_empty_returns_empty_vec() {
        let mut opt = OutputOptimizer::new(true, None);
        // Pure ANSI with no visible chars → stripped to "" → early-return empty branch
        let result = opt.process_streaming_line("\x1b[31m\x1b[0m");
        assert_eq!(result, Vec::<String>::new());
    }

    #[test]
    fn process_streaming_line_enabled_deduplicates_consecutive_repeats() {
        let mut opt = OutputOptimizer::new(true, None);
        let r1 = opt.process_streaming_line("repeated line");
        let r2 = opt.process_streaming_line("repeated line");
        assert_eq!(r1, vec!["repeated line".to_string()]);
        assert!(r2.is_empty(), "consecutive duplicate should be suppressed");
    }

    #[test]
    fn process_streaming_line_enabled_non_duplicate_passes_through() {
        let mut opt = OutputOptimizer::new(true, None);
        let r1 = opt.process_streaming_line("line A");
        let r2 = opt.process_streaming_line("line B");
        assert_eq!(r1, vec!["line A".to_string()]);
        assert_eq!(r2, vec!["line B".to_string()]);
    }

    // ── finalize_output – disabled path ───────────────────────────────────

    #[test]
    fn finalize_output_disabled_stdout_only_returns_stdout() {
        let mut opt = OutputOptimizer::new(false, None);
        let result = opt.finalize_output("cargo", 0, "build ok", "");
        assert_eq!(result, "build ok");
    }

    #[test]
    fn finalize_output_disabled_stderr_only_returns_stderr() {
        let mut opt = OutputOptimizer::new(false, None);
        let result = opt.finalize_output("cargo", 1, "", "fatal error");
        assert_eq!(result, "fatal error");
    }

    #[test]
    fn finalize_output_disabled_both_non_empty_returns_combined() {
        let mut opt = OutputOptimizer::new(false, None);
        let result = opt.finalize_output("cargo", 1, "stdout text", "stderr text");
        assert_eq!(result, "stdout text\nstderr text");
    }

    #[test]
    fn finalize_output_disabled_preserves_ansi_codes() {
        let mut opt = OutputOptimizer::new(false, None);
        let ansi_out = "\x1b[32msuccess\x1b[0m";
        // Disabled: no stripping occurs, ANSI codes survive
        let result = opt.finalize_output("cargo", 0, ansi_out, "");
        assert!(
            result.contains("\x1b["),
            "ANSI codes should not be stripped when disabled"
        );
    }

    // ── finalize_output – enabled, all four pressure levels ───────────────

    #[test]
    fn finalize_output_relaxed_pressure_succeeds_and_accumulates_tokens() {
        // session_tokens_used = 0, huge context → Relaxed (< 40 %)
        let mut opt = OutputOptimizer::new(true, Some(1_000_000));
        let result = opt.finalize_output("cargo", 0, "build success line", "");
        assert!(result.contains("✅"), "relaxed success should contain ✅");
        assert!(opt.session_tokens_used > 0, "token counter should increase");
    }

    #[test]
    fn finalize_output_moderate_pressure_succeeds() {
        // 500 / 1000 = 50 % → Moderate
        let mut opt = OutputOptimizer::new(true, Some(1000));
        opt.session_tokens_used = 500;
        let result = opt.finalize_output("cargo", 0, "build success", "");
        assert!(
            result.contains("✅"),
            "moderate-pressure success should contain ✅"
        );
    }

    #[test]
    fn finalize_output_elevated_pressure_succeeds() {
        // 750 / 1000 = 75 % → Elevated
        let mut opt = OutputOptimizer::new(true, Some(1000));
        opt.session_tokens_used = 750;
        let result = opt.finalize_output("cargo", 0, "build success", "");
        assert!(
            result.contains("✅"),
            "elevated-pressure success should contain ✅"
        );
    }

    #[test]
    fn finalize_output_critical_pressure_succeeds() {
        // 900 / 1000 = 90 % → Critical
        let mut opt = OutputOptimizer::new(true, Some(1000));
        opt.session_tokens_used = 900;
        let result = opt.finalize_output("cargo", 0, "build success", "");
        assert!(
            result.contains("✅"),
            "critical-pressure success should contain ✅"
        );
    }

    #[test]
    fn finalize_output_failed_command_returns_error_tail() {
        let mut opt = OutputOptimizer::new(true, None);
        let result = opt.finalize_output("cargo", 2, "", "error: something failed");
        assert!(result.contains("❌"), "failed command should contain ❌");
        assert!(
            result.contains("something failed"),
            "error tail should be present in output"
        );
    }

    // ── finalize_output – ANSI stripping when enabled ─────────────────────

    #[test]
    fn finalize_output_enabled_strips_ansi_from_stdout_and_stderr() {
        let mut opt = OutputOptimizer::new(true, None);
        let colored_out = "\x1b[32mbuild done\x1b[0m";
        let colored_err = "\x1b[31mfatal\x1b[0m";
        // Failed command so that content from both streams ends up in the output
        let result = opt.finalize_output("cargo", 1, colored_out, colored_err);
        assert!(
            !result.contains("\x1b["),
            "ANSI codes must be stripped when enabled"
        );
        assert!(
            result.contains("build done") || result.contains("fatal"),
            "visible content should survive stripping"
        );
    }

    // ── finalize_output – fingerprinting ──────────────────────────────────

    #[test]
    fn finalize_output_first_call_produces_no_fingerprint_warning() {
        let mut opt = OutputOptimizer::new(true, Some(1_000_000));
        let result = opt.finalize_output("cargo", 0, "output text", "");
        assert!(
            !result.contains("⚠️"),
            "first call should not produce a duplicate warning"
        );
    }

    #[test]
    fn finalize_output_identical_repeated_output_triggers_fingerprint_warning() {
        // Huge context window keeps pressure at Relaxed across both calls, so the
        // compressed+truncated text is byte-for-byte identical, triggering the fingerprinter.
        let mut opt = OutputOptimizer::new(true, Some(1_000_000_000));
        let stdout = "constant output";
        let r1 = opt.finalize_output("cargo", 0, stdout, "");
        let r2 = opt.finalize_output("cargo", 0, stdout, "");
        assert!(!r1.contains("⚠️"), "first call should not warn");
        assert!(
            r2.contains("⚠️") || r2.contains("identical"),
            "second identical call should contain a fingerprint warning; got: {r2}"
        );
    }

    #[test]
    fn finalize_output_changed_output_does_not_trigger_fingerprint_warning() {
        let mut opt = OutputOptimizer::new(true, Some(1_000_000_000));
        opt.finalize_output("cargo", 0, "output A", "");
        // Different content → different hash → no warning
        let r2 = opt.finalize_output("cargo", 0, "output B – completely different text", "");
        assert!(
            !r2.contains("⚠️"),
            "different output should not produce a fingerprint warning"
        );
    }

    // ── finalize_output – session token accumulation ──────────────────────

    #[test]
    fn finalize_output_accumulates_session_tokens_across_calls() {
        let mut opt = OutputOptimizer::new(true, None);
        assert_eq!(opt.session_tokens_used, 0);
        opt.finalize_output("cargo", 0, "first output with some text content here", "");
        let after_first = opt.session_tokens_used;
        assert!(
            after_first > 0,
            "token counter should increase after first call"
        );
        opt.finalize_output(
            "cargo",
            0,
            "second output with completely different text content",
            "",
        );
        assert!(
            opt.session_tokens_used > after_first,
            "token counter should accumulate across calls"
        );
    }
}
