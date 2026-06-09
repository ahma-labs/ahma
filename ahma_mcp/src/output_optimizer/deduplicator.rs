/// Consecutive line deduplicator.
/// Sits in the streaming output collector pipeline.
#[derive(Default, Debug)]
pub struct LineDeduplicator {
    prev_line: Option<String>,
    repeat_count: u64,
}

impl LineDeduplicator {
    pub fn new() -> Self {
        Self {
            prev_line: None,
            repeat_count: 0,
        }
    }

    /// Processes a streamed line. Returns empty vector if line is a consecutive duplicate.
    /// Otherwise, flushes any accumulated repetition count first, then returns the new line.
    pub fn process(&mut self, line: &str) -> Vec<String> {
        let trimmed = line.trim_end();
        if Some(trimmed) == self.prev_line.as_deref() {
            self.repeat_count += 1;
            return vec![];
        }

        let mut output = Vec::with_capacity(2);
        if self.repeat_count > 0 {
            output.push(format!("[... repeated {} more times]", self.repeat_count));
        }
        self.repeat_count = 0;
        self.prev_line = Some(trimmed.to_string());
        output.push(line.to_string());
        output
    }

    /// Flush any pending repeat count at end of execution.
    pub fn flush(&mut self) -> Option<String> {
        if self.repeat_count > 0 {
            let annotation = format!("[... repeated {} more times]", self.repeat_count);
            self.repeat_count = 0;
            Some(annotation)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dedup_flow() {
        let mut dedup = LineDeduplicator::new();
        assert_eq!(dedup.process("line 1"), vec!["line 1".to_string()]);
        assert_eq!(dedup.process("line 2"), vec!["line 2".to_string()]);
        assert_eq!(dedup.process("line 2"), Vec::<String>::new());
        assert_eq!(dedup.process("line 2"), Vec::<String>::new());
        assert_eq!(
            dedup.process("line 3"),
            vec![
                "[... repeated 2 more times]".to_string(),
                "line 3".to_string()
            ]
        );
        assert_eq!(dedup.flush(), None);
    }

    #[test]
    fn test_dedup_flush() {
        let mut dedup = LineDeduplicator::new();
        dedup.process("dup");
        dedup.process("dup");
        dedup.process("dup");
        assert_eq!(
            dedup.flush(),
            Some("[... repeated 2 more times]".to_string())
        );
    }
}
