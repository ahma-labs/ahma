use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Tool-call failure loop detector.
/// Prevents the LLM from repeatedly making the exact same failing tool call.
pub struct LoopDetector {
    fail_counts: HashMap<u64, u32>,
    max_retries: u32,
}

impl LoopDetector {
    pub fn new(max_retries: u32) -> Self {
        Self {
            fail_counts: HashMap::new(),
            max_retries,
        }
    }

    /// Hashes the tool name and arguments to uniquely identify the call.
    fn hash_call(&self, tool_name: &str, args: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        tool_name.hash(&mut hasher);
        args.hash(&mut hasher);
        hasher.finish()
    }

    /// Checks if this call is stuck in a failure loop.
    pub fn is_loop(&self, tool_name: &str, args: &str) -> bool {
        let hash = self.hash_call(tool_name, args);
        if let Some(&count) = self.fail_counts.get(&hash) {
            count >= self.max_retries
        } else {
            false
        }
    }

    /// Records a failure for a specific tool call. Returns true if loop is detected.
    pub fn record_failure(&mut self, tool_name: &str, args: &str) -> bool {
        let hash = self.hash_call(tool_name, args);
        let count = self.fail_counts.entry(hash).or_insert(0);
        *count += 1;
        *count >= self.max_retries
    }

    /// Clears all failure counts when any tool call succeeds.
    pub fn record_success(&mut self) {
        self.fail_counts.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_loop_detector() {
        let mut detector = LoopDetector::new(3);
        let name = "run_terminal_command";
        let args = "{\"args\": [\"cargo test\"]}";

        assert!(!detector.is_loop(name, args));
        detector.record_failure(name, args);
        detector.record_failure(name, args);
        assert!(!detector.is_loop(name, args));
        detector.record_failure(name, args);
        assert!(detector.is_loop(name, args));

        // Success clears the detector
        detector.record_success();
        assert!(!detector.is_loop(name, args));
    }
}
