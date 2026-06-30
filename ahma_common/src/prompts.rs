//! LLM prompt management and configuration for `ahma`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_PLANNING_PROMPT: &str = r#"You are an expert task planner and knowledge worker. Your job is to decompose the current task into at most {max_subtasks} sequential steps.

System Instructions:
1. Break the task down into distinct, logical, self-contained sub-tasks.
2. Each step must be one of the following types:
   - "shell_command": Running a sandboxed terminal command.
   - "llm_call": A subtask that requires reasoning or parsing text.
   - "planning": A subtask that requires further planning/decomposition.
3. Security Scoping Down Rules:
   - If a step operates on a subdirectory (e.g. "src"), you may narrow down filesystem access by setting `sandbox_scopes` (e.g. `["src"]`).
   - You may restrict the allowed command prefixes in `allowed_tools` (e.g. `["cargo build", "cargo clippy"]` or `["git diff"]`).
   - You may restrict outbound network access by setting `allowed_domains` (e.g. `["crates.io", "api.github.com"]`).
4. Output format: You must output ONLY a valid JSON object matching the schema below. No other markdown formatting except optional json code blocks.

Expected JSON Schema:
```json
{
  "steps": [
    {
      "task": "description of this step",
      "type": "shell_command",
      "command": "cargo check --workspace",
      "sandbox_scopes": ["optional_subdir"],
      "allowed_tools": ["cargo"],
      "allowed_domains": []
    },
    {
      "task": "evaluate errors",
      "type": "llm_call",
      "instructions": "Look at the check output and identify compile errors."
    }
  ]
}
```

Overall Goal: {goal}

Current Task: {task_desc}

Context (Previous branch outcomes):
{branch_context}

Decompose this task and output the JSON steps now:"#;

pub const DEFAULT_SUMMARISATION_PROMPT: &str = r#"Please provide a concise, high-density summary of the following tool execution output in 3 sentences or less.
Ensure you preserve key details, including:
1. Whether the operation succeeded or failed (compile errors, tests failed, exit codes).
2. Specific warning/error messages or file paths mentioned.
3. Quantifiable numbers or statistics (e.g. '3 tests failed', 'build took 24s').
If there was no output, reply with 'CLEAN'.

STDOUT:
{stdout}

STDERR:
{stderr}

Concise summary:"#;

pub const DEFAULT_RECOVERY_PROMPT: &str = r#"You are an expert task planner and knowledge worker. A subtask has failed, and you need to decide whether to re-plan or fail/escalate the task.

System Instructions:
1. Evaluate the error and output a JSON decision.
2. If you choose to re-plan, set `action` to "re_plan" and specify a list of replacement `steps` (at most {max_subtasks} steps) to execute instead of the failed step and any remaining steps. This can include corrective measures (e.g. installing a dependency, fixing a file, running a different command).
3. If the failure is unrecoverable, set `action` to "fail" and specify a `reason`.
4. Security Scoping Down Rules: Any new steps must respect the sandbox scopes, allowed tools, and allowed domains of the parent task.
5. Output format: You must output ONLY a valid JSON object matching the schema below. No other markdown formatting except optional json code blocks.

Expected JSON Schema:
```json
{
  "action": "re_plan",
  "steps": [
    {
      "task": "new task description",
      "type": "shell_command",
      "command": "cargo build --release",
      "sandbox_scopes": [],
      "allowed_tools": ["cargo"],
      "allowed_domains": []
    }
  ]
}
```
Or:
```json
{
  "action": "fail",
  "reason": "Detailed description of why we cannot proceed."
}
```

Overall Goal: {goal}

Current Task: {task_desc}

Context (Previous branch outcomes):
{branch_context}

Failed Step Description: {failed_step_desc}

Failed Step Error/Outcome:
{failed_step_error}

Remaining Unexecuted Steps:
{remaining_str}

Decide the recovery action and output the JSON now:"#;

pub const DEFAULT_SPLIT_PROMPT: &str = r#"Break the following question into at most {max} distinct, self-contained sub-questions that together cover the full answer. Return ONLY the sub-questions, one per line, numbered like '1. ...' No other text.

Question: {question}"#;

/// Default system prompt for the interactive `ahma tui` chat agent (and the
/// shared agent loop used by the MCP sub-agent). This is the scaffolding that
/// turns a bare "helpful assistant" into a task-completing agent: it tells the
/// model to keep working across turns, the file-editing tool contract, and the
/// single rule that actually ends the agent loop — finish by replying with no
/// tool call. Users override it via the `[agent] system` key in prompts.toml.
pub const DEFAULT_AGENT_SYSTEM_PROMPT: &str = r#"You are Ahma, an autonomous coding and knowledge-work assistant operating inside a sandboxed workspace. You complete tasks end-to-end by calling tools, not by describing what you would do.

Operating rules:
- Work autonomously across multiple turns. After each tool result, decide the next action and keep going until the task is fully done. Do NOT stop after a single tool call.
- Planning: for any task with more than a couple of steps, call todo_write first to record a short checklist, then keep it current — mark a step in_progress before you start it and completed when it's done. Re-read your plan to stay on track. Skip the plan for trivial one-step tasks.
- Inspect before you act: use read_file, list_dir, grep_search, and file_search to understand the workspace before you change it.
- Editing files: use write_file ONLY to create a new file; use replace_in_file (with exact old/new text) to modify a file that already exists. Never blind-overwrite an existing file.
- Running commands: use run_terminal_command for builds, tests, scripts, and data processing. It is sandboxed to the workspace.
- Verify your work: after making changes, re-read the file or run the relevant build/test/command to confirm the result before you declare success.
- Producing artifacts: when asked for a table, chart, report, or data file, write it to a file (Markdown, CSV, or SVG) with write_file and tell the user the path.

Finishing:
- The task is complete only when the user's request is fully satisfied. When it is, reply with a short plain-text summary of what you did and the outcome, and make NO tool call — replying with no tool call is what ends your turn.
- If you are genuinely blocked (missing information, permission, or an unrecoverable error), stop and clearly explain what is blocking you and what you need to proceed.

Be concise and direct. Prefer doing over explaining."#;

/// Path to the global prompts config file.
///
/// Uses [`crate::config::ahma_home_dir`] for home resolution so that tests can
/// redirect it cross-platform (see that function for why `HOME` alone does not
/// work on Windows).
pub fn global_prompts_path() -> Option<PathBuf> {
    crate::config::ahma_home_dir().map(|h| h.join(".ahma").join("prompts.toml"))
}

/// Structure representing prompts for the task tree module.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct TaskTreePrompts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summarisation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery: Option<String>,
}

/// Structure representing prompts for the decompose module.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct DecomposePrompts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub split: Option<String>,
}

/// Structure representing prompts for the interactive chat agent (`ahma tui`
/// and the shared MCP sub-agent loop).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct AgentPrompts {
    /// System prompt prepended to every agent conversation. Overrides
    /// [`DEFAULT_AGENT_SYSTEM_PROMPT`] when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
}

/// Main prompt configuration loaded from prompts.toml.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct AhmaPrompts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_tree: Option<TaskTreePrompts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decompose: Option<DecomposePrompts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentPrompts>,
}

impl AhmaPrompts {
    /// Load prompts from global defaults and the current directory project override.
    pub fn load() -> Self {
        Self::load_from_dir(Path::new("."))
    }

    /// Load prompts from global defaults and a specific project directory.
    pub fn load_from_dir(project_dir: &Path) -> Self {
        let mut prompts = Self::default();

        // 1. Load global defaults (~/.ahma/prompts.toml)
        if let Some(path) = global_prompts_path().filter(|p| p.exists()) {
            match std::fs::read_to_string(&path) {
                Ok(content) => match toml::from_str::<Self>(&content) {
                    Ok(parsed) => {
                        prompts.merge(parsed);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to parse global prompts at {}: {e}", path.display());
                    }
                },
                Err(e) => {
                    tracing::warn!("Failed to read global prompts at {}: {e}", path.display());
                }
            }
        }

        // 2. Load project-local override (<project_dir>/.ahma/prompts.toml)
        let local_path = project_dir.join(".ahma").join("prompts.toml");
        if local_path.exists() {
            match std::fs::read_to_string(&local_path) {
                Ok(content) => match toml::from_str::<Self>(&content) {
                    Ok(parsed) => {
                        prompts.merge(parsed);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to parse project prompts at {}: {e}",
                            local_path.display()
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        "Failed to read project prompts at {}: {e}",
                        local_path.display()
                    );
                }
            }
        }

        prompts
    }

    /// Merge another prompts structure into this one (overwriting Some values).
    pub fn merge(&mut self, other: Self) {
        if let Some(other_tt) = other.task_tree {
            let self_tt = self.task_tree.get_or_insert_with(TaskTreePrompts::default);
            if let Some(p) = other_tt.planning {
                self_tt.planning = Some(p);
            }
            if let Some(s) = other_tt.summarisation {
                self_tt.summarisation = Some(s);
            }
            if let Some(r) = other_tt.recovery {
                self_tt.recovery = Some(r);
            }
        }

        if let Some(other_dec) = other.decompose {
            let self_dec = self.decompose.get_or_insert_with(DecomposePrompts::default);
            if let Some(s) = other_dec.split {
                self_dec.split = Some(s);
            }
        }

        if let Some(other_agent) = other.agent {
            let self_agent = self.agent.get_or_insert_with(AgentPrompts::default);
            if let Some(s) = other_agent.system {
                self_agent.system = Some(s);
            }
        }
    }

    /// Get planning prompt (global or project override, or compiled-in default).
    pub fn planning_prompt(&self) -> String {
        self.task_tree
            .as_ref()
            .and_then(|tt| tt.planning.clone())
            .unwrap_or_else(|| DEFAULT_PLANNING_PROMPT.to_string())
    }

    /// Get summarisation prompt.
    pub fn summarisation_prompt(&self) -> String {
        self.task_tree
            .as_ref()
            .and_then(|tt| tt.summarisation.clone())
            .unwrap_or_else(|| DEFAULT_SUMMARISATION_PROMPT.to_string())
    }

    /// Get recovery prompt.
    pub fn recovery_prompt(&self) -> String {
        self.task_tree
            .as_ref()
            .and_then(|tt| tt.recovery.clone())
            .unwrap_or_else(|| DEFAULT_RECOVERY_PROMPT.to_string())
    }

    /// Get decompose split prompt.
    pub fn split_prompt(&self) -> String {
        self.decompose
            .as_ref()
            .and_then(|dec| dec.split.clone())
            .unwrap_or_else(|| DEFAULT_SPLIT_PROMPT.to_string())
    }

    /// Get the interactive agent system prompt (global or project override, or
    /// the compiled-in [`DEFAULT_AGENT_SYSTEM_PROMPT`]).
    pub fn agent_system_prompt(&self) -> String {
        self.agent
            .as_ref()
            .and_then(|a| a.system.clone())
            .unwrap_or_else(|| DEFAULT_AGENT_SYSTEM_PROMPT.to_string())
    }

    /// Generate the fully commented-out defaults template to be written to prompts.toml.
    pub fn generate_template() -> String {
        format!(
            r#"# ~/.ahma/prompts.toml — Ahma LLM prompts configuration
#
# All prompts are commented out. Uncomment and edit any value to override
# the compiled-in default. 
#
# Placeholders:
#   planning: {{goal}}, {{task_desc}}, {{branch_context}}, {{max_subtasks}}
#   summarisation: {{stdout}}, {{stderr}}
#   recovery: {{goal}}, {{task_desc}}, {{branch_context}}, {{failed_step_desc}}, {{failed_step_error}}, {{remaining_str}}, {{max_subtasks}}
#   split: {{max}}, {{question}}
#   agent: (no placeholders — free-form system prompt for the `ahma tui` chat agent)

# [agent]
# system = """
# {}
# """

# [task_tree]
# planning = """
# {}
# """
#
# summarisation = """
# {}
# """
#
# recovery = """
# {}
# """

# [decompose]
# split = """
# {}
# """
"#,
            DEFAULT_AGENT_SYSTEM_PROMPT.replace('\n', "\n# "),
            DEFAULT_PLANNING_PROMPT.replace('\n', "\n# "),
            DEFAULT_SUMMARISATION_PROMPT.replace('\n', "\n# "),
            DEFAULT_RECOVERY_PROMPT.replace('\n', "\n# "),
            DEFAULT_SPLIT_PROMPT.replace('\n', "\n# ")
        )
    }

    /// Validate the currently configured prompts, returning warnings if any placeholders are missing
    /// or if instructions are likely malformed.
    pub fn validate(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        let planning = self.planning_prompt();
        let plan_placeholders = ["goal", "task_desc", "branch_context", "max_subtasks"];
        for ph in &plan_placeholders {
            if !planning.contains(&format!("{{{}}}", ph)) {
                warnings.push(format!(
                    "Planning prompt is missing required placeholder: {{{}}}",
                    ph
                ));
            }
        }
        if !planning.to_lowercase().contains("json") {
            warnings.push(
                "Planning prompt may be missing instructions to return JSON data format."
                    .to_string(),
            );
        }

        let summarisation = self.summarisation_prompt();
        let sum_placeholders = ["stdout", "stderr"];
        for ph in &sum_placeholders {
            if !summarisation.contains(&format!("{{{}}}", ph)) {
                warnings.push(format!(
                    "Summarisation prompt is missing required placeholder: {{{}}}",
                    ph
                ));
            }
        }

        let recovery = self.recovery_prompt();
        let rec_placeholders = [
            "goal",
            "task_desc",
            "branch_context",
            "failed_step_desc",
            "failed_step_error",
            "remaining_str",
            "max_subtasks",
        ];
        for ph in &rec_placeholders {
            if !recovery.contains(&format!("{{{}}}", ph)) {
                warnings.push(format!(
                    "Recovery prompt is missing required placeholder: {{{}}}",
                    ph
                ));
            }
        }
        if !recovery.to_lowercase().contains("json") {
            warnings.push(
                "Recovery prompt may be missing instructions to return JSON data format."
                    .to_string(),
            );
        }

        let split = self.split_prompt();
        let split_placeholders = ["max", "question"];
        for ph in &split_placeholders {
            if !split.contains(&format!("{{{}}}", ph)) {
                warnings.push(format!(
                    "Decompose split prompt is missing required placeholder: {{{}}}",
                    ph
                ));
            }
        }

        warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_merge_prompts() {
        let mut base = AhmaPrompts::default();
        let other = AhmaPrompts {
            task_tree: Some(TaskTreePrompts {
                planning: Some("Custom planning".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        base.merge(other);
        assert_eq!(base.planning_prompt(), "Custom planning");
        assert_eq!(base.summarisation_prompt(), DEFAULT_SUMMARISATION_PROMPT);
    }

    #[test]
    fn test_validate_default_clean() {
        let prompts = AhmaPrompts::default();
        let warnings = prompts.validate();
        assert!(
            warnings.is_empty(),
            "Defaults should have no validation warnings: {:?}",
            warnings
        );
    }

    #[test]
    fn test_validate_missing_placeholder() {
        let prompts = AhmaPrompts {
            task_tree: Some(TaskTreePrompts {
                planning: Some("No placeholders here".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let warnings = prompts.validate();
        assert!(!warnings.is_empty());
        assert!(warnings.iter().any(|w| w.contains("goal")));
    }

    #[test]
    fn test_agent_system_prompt_default() {
        let prompts = AhmaPrompts::default();
        assert_eq!(prompts.agent_system_prompt(), DEFAULT_AGENT_SYSTEM_PROMPT);
        // The default must carry the loop-ending contract, or the agent never
        // stops requesting tools and hits the turn limit.
        assert!(prompts.agent_system_prompt().contains("no tool call"));
    }

    #[test]
    fn test_agent_system_prompt_override() {
        let mut base = AhmaPrompts::default();
        base.merge(AhmaPrompts {
            agent: Some(AgentPrompts {
                system: Some("Custom agent prompt".to_string()),
            }),
            ..Default::default()
        });
        assert_eq!(base.agent_system_prompt(), "Custom agent prompt");
        // Overriding the agent prompt must not disturb the other sections.
        assert_eq!(base.planning_prompt(), DEFAULT_PLANNING_PROMPT);
    }

    #[test]
    fn test_load_agent_prompt_from_dir() {
        let dir = tempdir().unwrap();
        let local_ahma = dir.path().join(".ahma");
        std::fs::create_dir_all(&local_ahma).unwrap();
        std::fs::write(
            local_ahma.join("prompts.toml"),
            "[agent]\nsystem = \"Project agent prompt\"\n",
        )
        .unwrap();

        let loaded = AhmaPrompts::load_from_dir(dir.path());
        assert_eq!(loaded.agent_system_prompt(), "Project agent prompt");
    }

    #[test]
    fn test_generate_template_includes_agent_section() {
        let template = AhmaPrompts::generate_template();
        assert!(template.contains("[agent]"));
        assert!(template.contains("# system = "));
    }

    #[test]
    fn test_load_from_dir() {
        let dir = tempdir().unwrap();
        let local_ahma = dir.path().join(".ahma");
        std::fs::create_dir_all(&local_ahma).unwrap();

        let toml_content = r#"
[task_tree]
planning = "Dir planning {goal} {task_desc} {branch_context} {max_subtasks}"
"#;
        std::fs::write(local_ahma.join("prompts.toml"), toml_content).unwrap();

        let loaded = AhmaPrompts::load_from_dir(dir.path());
        assert_eq!(
            loaded.planning_prompt(),
            "Dir planning {goal} {task_desc} {branch_context} {max_subtasks}"
        );
    }
}
