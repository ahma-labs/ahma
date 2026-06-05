/// Build the system and user prompt for planning / decomposition.
pub fn build_planning_prompt(
    goal: &str,
    task_desc: &str,
    branch_context: &str,
    max_subtasks: usize,
) -> String {
    format!(
        "You are an expert task planner and knowledge worker. Your job is to decompose the current task into at most {max_subtasks} sequential steps.\n\n\
        System Instructions:\n\
        1. Break the task down into distinct, logical, self-contained sub-tasks.\n\
        2. Each step must be one of the following types:\n\
           - \"shell_command\": Running a sandboxed terminal command.\n\
           - \"llm_call\": A subtask that requires reasoning or parsing text.\n\
           - \"planning\": A subtask that requires further planning/decomposition.\n\
        3. Security Scoping Down Rules:\n\
           - If a step operates on a subdirectory (e.g. \"src\"), you may narrow down filesystem access by setting `sandbox_scopes` (e.g. `[\"src\"]`).\n\
           - You may restrict the allowed command prefixes in `allowed_tools` (e.g. `[\"cargo build\", \"cargo clippy\"]` or `[\"git diff\"]`).\n\
           - You may restrict outbound network access by setting `allowed_domains` (e.g. `[\"crates.io\", \"api.github.com\"]`).\n\
        4. Output format: You must output ONLY a valid JSON object matching the schema below. No other markdown formatting except optional json code blocks.\n\n\
        Expected JSON Schema:\n\
        ```json\n\
        {{\n\
          \"steps\": [\n\
            {{\n\
              \"task\": \"description of this step\",\n\
              \"type\": \"shell_command\",\n\
              \"command\": \"cargo check --workspace\",\n\
              \"sandbox_scopes\": [\"optional_subdir\"],\n\
              \"allowed_tools\": [\"cargo\"],\n\
              \"allowed_domains\": []\n\
            }},\n\
            {{\n\
              \"task\": \"evaluate errors\",\n\
              \"type\": \"llm_call\",\n\
              \"instructions\": \"Look at the check output and identify compile errors.\"\n\
            }}\n\
          ]\n\
        }}\n\
        ```\n\n\
        Overall Goal: {goal}\n\n\
        Current Task: {task_desc}\n\n\
        Context (Previous branch outcomes):\n\
        {branch_context}\n\n\
        Decompose this task and output the JSON steps now:"
    )
}

/// Build the prompt for summarizing lengthy tool outputs.
pub fn build_summarisation_prompt(stdout: &str, stderr: &str) -> String {
    format!(
        "Please provide a concise, high-density summary of the following tool execution output in 3 sentences or less.\n\
        Ensure you preserve key details, including:\n\
        1. Whether the operation succeeded or failed (compile errors, tests failed, exit codes).\n\
        2. Specific warning/error messages or file paths mentioned.\n\
        3. Quantifiable numbers or statistics (e.g. '3 tests failed', 'build took 24s').\n\
        If there was no output, reply with 'CLEAN'.\n\n\
        STDOUT:\n\
        {stdout}\n\n\
        STDERR:\n\
        {stderr}\n\n\
        Concise summary:"
    )
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

    format!(
        "You are an expert task planner and knowledge worker. A subtask has failed, and you need to decide whether to re-plan or fail/escalate the task.\n\n\
        System Instructions:\n\
        1. Evaluate the error and output a JSON decision.\n\
        2. If you choose to re-plan, set `action` to \"re_plan\" and specify a list of replacement `steps` (at most {max_subtasks} steps) to execute instead of the failed step and any remaining steps. This can include corrective measures (e.g. installing a dependency, fixing a file, running a different command).\n\
        3. If the failure is unrecoverable, set `action` to \"fail\" and specify a `reason`.\n\
        4. Security Scoping Down Rules: Any new steps must respect the sandbox scopes, allowed tools, and allowed domains of the parent task.\n\
        5. Output format: You must output ONLY a valid JSON object matching the schema below. No other markdown formatting except optional json code blocks.\n\n\
        Expected JSON Schema:\n\
        ```json\n\
        {{\n\
          \"action\": \"re_plan\",\n\
          \"steps\": [\n\
            {{\n\
              \"task\": \"new task description\",\n\
              \"type\": \"shell_command\",\n\
              \"command\": \"cargo build --release\",\n\
              \"sandbox_scopes\": [],\n\
              \"allowed_tools\": [\"cargo\"],\n\
              \"allowed_domains\": []\n\
            }}\n\
          ]\n\
        }}\n\
        ```\n\
        Or:\n\
        ```json\n\
        {{\n\
          \"action\": \"fail\",\n\
          \"reason\": \"Detailed description of why we cannot proceed.\"\n\
        }}\n\
        ```\n\n\
        Overall Goal: {goal}\n\n\
        Current Task: {task_desc}\n\n\
        Context (Previous branch outcomes):\n\
        {branch_context}\n\n\
        Failed Step Description: {failed_step_desc}\n\n\
        Failed Step Error/Outcome:\n\
        {failed_step_error}\n\n\
        Remaining Unexecuted Steps:\n\
        {remaining_str}\n\n\
        Decide the recovery action and output the JSON now:"
    )
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

