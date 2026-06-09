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
