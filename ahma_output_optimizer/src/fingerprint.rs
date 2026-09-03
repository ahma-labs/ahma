use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Tracks command outputs and detects if they are identical to previous executions.
#[derive(Debug)]
pub struct OutputFingerprinter {
    last_outputs: HashMap<String, u64>,
}

impl Default for OutputFingerprinter {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputFingerprinter {
    pub fn new() -> Self {
        Self {
            last_outputs: HashMap::new(),
        }
    }

    /// Checks if the command output has changed.
    /// Returns Some(warning_message) if it is identical, otherwise None and stores the new hash.
    pub fn check_change(&mut self, command: &str, output: &str) -> Option<String> {
        let mut hasher = DefaultHasher::new();
        output.hash(&mut hasher);
        let hash = hasher.finish();

        if let Some(&prev_hash) = self.last_outputs.get(command)
            && prev_hash == hash
        {
            return Some(format!(
                "⚠️ [Output identical to the previous execution of this command (hash: {:x})]",
                hash
            ));
        }

        self.last_outputs.insert(command.to_string(), hash);
        None
    }

    pub fn clear(&mut self) {
        self.last_outputs.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fingerprinter() {
        let mut printer = OutputFingerprinter::new();
        let cmd = "cargo build";
        let out1 = "error in line 12";
        let out2 = "error in line 12";
        let out3 = "error fixed";

        assert_eq!(printer.check_change(cmd, out1), None);
        assert!(printer.check_change(cmd, out2).is_some());
        assert_eq!(printer.check_change(cmd, out3), None);
    }
}
