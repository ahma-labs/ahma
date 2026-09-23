//! LLM prompt management and configuration for `ahma`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
- Editing files: read_file first (its lines are numbered "  N<tab>text"; the number is not part of the file). Then change an existing file with replace_in_file (old_str must match exactly once — include surrounding lines to make it unique), multi_edit for several changes to one file, or apply_patch for changes across files. Use write_file only to create a file, or to replace one you have read. Edits to a file you have not read, or that changed since, are refused — read it again.
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
        if let Some(path) = global_prompts_path() {
            prompts.load_and_merge(&path);
        }

        // 2. Load project-local override (<project_dir>/.ahma/prompts.toml)
        prompts.load_and_merge(&project_dir.join(".ahma").join("prompts.toml"));

        prompts
    }

    /// Read and parse `path` if it exists, merging it into `self`. Missing
    /// files are silent (both tiers are optional); read/parse failures are
    /// logged and otherwise ignored, since a broken prompts override must
    /// never prevent the process from starting.
    fn load_and_merge(&mut self, path: &Path) {
        if !path.exists() {
            return;
        }
        match std::fs::read_to_string(path) {
            Ok(content) => match toml::from_str::<Self>(&content) {
                Ok(parsed) => self.merge(parsed),
                Err(e) => tracing::warn!("Failed to parse prompts at {}: {e}", path.display()),
            },
            Err(e) => tracing::warn!("Failed to read prompts at {}: {e}", path.display()),
        }
    }

    /// Merge another prompts structure into this one (overwriting Some values).
    pub fn merge(&mut self, other: Self) {
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
#   split: {{max}}, {{question}}
#   agent: (no placeholders — free-form system prompt for the `ahma tui` chat agent)

# [agent]
# system = """
# {}
# """

# [decompose]
# split = """
# {}
# """
"#,
            DEFAULT_AGENT_SYSTEM_PROMPT.replace('\n', "\n# "),
            DEFAULT_SPLIT_PROMPT.replace('\n', "\n# ")
        )
    }

    /// Validate the currently configured prompts, returning warnings if any placeholders are missing
    /// or if instructions are likely malformed.
    pub fn validate(&self) -> Vec<String> {
        let mut warnings = Vec::new();

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
            decompose: Some(DecomposePrompts {
                split: Some("Custom split".to_string()),
            }),
            ..Default::default()
        };
        base.merge(other);
        assert_eq!(base.split_prompt(), "Custom split");
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
            decompose: Some(DecomposePrompts {
                split: Some("No placeholders here".to_string()),
            }),
            ..Default::default()
        };
        let warnings = prompts.validate();
        assert!(!warnings.is_empty());
        assert!(warnings.iter().any(|w| w.contains("max")));
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
        assert_eq!(base.split_prompt(), DEFAULT_SPLIT_PROMPT);
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
[decompose]
split = "Dir split {max} {question}"
"#;
        std::fs::write(local_ahma.join("prompts.toml"), toml_content).unwrap();

        let loaded = AhmaPrompts::load_from_dir(dir.path());
        assert_eq!(loaded.split_prompt(), "Dir split {max} {question}");
    }
}
