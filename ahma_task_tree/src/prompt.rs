/// Build the system and user prompt for planning / decomposition.
use ahma_common::prompts::AhmaPrompts;

pub fn build_planning_prompt(
    goal: &str,
    task_desc: &str,
    branch_context: &str,
    max_subtasks: usize,
) -> String {
    let prompts = AhmaPrompts::load();
    prompts
        .planning_prompt()
        .replace("{goal}", goal)
        .replace("{task_desc}", task_desc)
        .replace("{branch_context}", branch_context)
        .replace("{max_subtasks}", &max_subtasks.to_string())
}

/// Build the prompt for summarizing lengthy tool outputs.
pub fn build_summarisation_prompt(stdout: &str, stderr: &str) -> String {
    let prompts = AhmaPrompts::load();
    prompts
        .summarisation_prompt()
        .replace("{stdout}", stdout)
        .replace("{stderr}", stderr)
}

/// Build the prompt for recovering from a subtask failure.
pub fn build_recovery_prompt(
    goal: &str,
    task_desc: &str,
    branch_context: &str,
    failed_step_desc: &str,
    failed_step_error: &str,
    remaining_steps: &[String],
    max_subtasks: usize,
) -> String {
    let remaining_str = if remaining_steps.is_empty() {
        "None (this was the last step).".to_string()
    } else {
        remaining_steps
            .iter()
            .enumerate()
            .map(|(i, s)| format!("{}. {}", i + 1, s))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let prompts = AhmaPrompts::load();
    prompts
        .recovery_prompt()
        .replace("{goal}", goal)
        .replace("{task_desc}", task_desc)
        .replace("{branch_context}", branch_context)
        .replace("{failed_step_desc}", failed_step_desc)
        .replace("{failed_step_error}", failed_step_error)
        .replace("{remaining_str}", &remaining_str)
        .replace("{max_subtasks}", &max_subtasks.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_planning_prompt() {
        let prompt = build_planning_prompt("Do X", "Step 1", "Completed step 0", 3);
        assert!(prompt.contains("expert task planner"));
        assert!(prompt.contains("at most 3 sequential steps"));
        assert!(prompt.contains("Overall Goal: Do X"));
        assert!(prompt.contains("Current Task: Step 1"));
        assert!(prompt.contains("Context (Previous branch outcomes):\nCompleted step 0"));
    }

    #[test]
    fn test_build_summarisation_prompt() {
        let prompt = build_summarisation_prompt("success", "no error");
        assert!(prompt.contains("concise, high-density summary"));
        assert!(prompt.contains("STDOUT:\nsuccess"));
        assert!(prompt.contains("STDERR:\nno error"));
    }

    #[test]
    fn test_build_recovery_prompt_with_remaining_steps() {
        let remaining = vec!["Step A".to_string(), "Step B".to_string()];
        let prompt = build_recovery_prompt(
            "Goal Y",
            "Task 2",
            "Context info",
            "Failed task",
            "Error log",
            &remaining,
            2,
        );
        assert!(prompt.contains("Failed Step Description: Failed task"));
        assert!(prompt.contains("Failed Step Error/Outcome:\nError log"));
        assert!(prompt.contains("Remaining Unexecuted Steps:\n1. Step A\n2. Step B"));
        assert!(prompt.contains("at most 2 steps"));
    }

    #[test]
    fn test_build_recovery_prompt_without_remaining_steps() {
        let prompt = build_recovery_prompt(
            "Goal Y",
            "Task 2",
            "Context info",
            "Failed task",
            "Error log",
            &[],
            2,
        );
        assert!(prompt.contains("Remaining Unexecuted Steps:\nNone (this was the last step)."));
    }
}
