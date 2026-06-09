/// Turn-based contextual skill and guidance injector.
/// Helps small models by injecting targeted tips only when relevant.
pub struct SkillInjector {
    read_file_triggered: bool,
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
            read_file_triggered: false,
            error_triggered: false,
        }
    }

    /// Inspects the last tool call and success/fail state, returning an optional guidance string.
    pub fn get_guidance_injection(&mut self, tool_name: &str, is_error: bool) -> Option<String> {
        if is_error && !self.error_triggered {
            self.error_triggered = true;
            return Some(
                "\n💡 [Harness Hint: The previous tool call failed. Carefully read the error output above. \
                 Ensure parameter values are correct, check for typo errors, and try a different approach.]"
                    .to_string(),
            );
        }

        if (tool_name == "read_file" || tool_name == "list_dir") && !self.read_file_triggered {
            self.read_file_triggered = true;
            return Some(
                "\n💡 [Harness Hint: When modifying files that already exist, you MUST use `replace_in_file` \
                 with exact old/new string matching. Avoid using `write_file` for existing files.]"
                    .to_string(),
            );
        }

        None
    }

    pub fn reset(&mut self) {
        self.read_file_triggered = false;
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
                .get_guidance_injection("read_file", false)
                .is_some()
        );
        // Next read is skipped
        assert!(
            injector
                .get_guidance_injection("read_file", false)
                .is_none()
        );

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
                .get_guidance_injection("read_file", false)
                .is_some()
        );
    }
}
