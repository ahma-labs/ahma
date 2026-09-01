/// Turn-based contextual skill and guidance injector.
/// Helps small models by injecting targeted tips only when relevant.
pub struct SkillInjector {
    error_triggered: bool,
}

impl Default for SkillInjector {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillInjector {
    pub fn new() -> Self {
        Self {
            error_triggered: false,
        }
    }

    /// Inspects the last tool call and success/fail state, returning an optional guidance string.
    ///
    /// `tool_name` is accepted for future per-tool guidance. It previously nudged
    /// `read_file`/`list_dir` callers toward `replace_in_file` over `write_file`
    /// for existing files; removed now that `write_file` creates-or-overwrites
    /// without complaint, matching its documented contract.
    pub fn get_guidance_injection(&mut self, _tool_name: &str, is_error: bool) -> Option<String> {
        if is_error && !self.error_triggered {
            self.error_triggered = true;
            return Some(
                "\n💡 [Harness Hint: The previous tool call failed. Carefully read the error output above. \
                 Ensure parameter values are correct, check for typo errors, and try a different approach.]"
                    .to_string(),
            );
        }

        None
    }

    pub fn reset(&mut self) {
        self.error_triggered = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_skill_injector() {
        let mut injector = SkillInjector::new();
        assert!(
            injector
                .get_guidance_injection("run_terminal_command", true)
                .is_some()
        );
        // Next error is skipped
        assert!(
            injector
                .get_guidance_injection("run_terminal_command", true)
                .is_none()
        );

        injector.reset();
        assert!(
            injector
                .get_guidance_injection("run_terminal_command", true)
                .is_some()
        );
    }
}
