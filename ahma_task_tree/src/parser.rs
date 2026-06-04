use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct ParsedStep {
    pub task: String,
    pub r#type: String, // "shell_command", "llm_call", "planning"
    pub command: Option<String>,
    pub instructions: Option<String>,
    pub subgoal: Option<String>,
    pub sandbox_scopes: Option<Vec<String>>,
    pub allowed_tools: Option<Vec<String>>,
    pub allowed_domains: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct ParsedPlan {
    pub steps: Vec<ParsedStep>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RecoveryDecision {
    pub action: String, // "re_plan" or "fail"
    pub steps: Option<Vec<ParsedStep>>,
    pub reason: Option<String>,
}

/// Parses the LLM JSON response containing steps. Strips out code blocks if present.
pub fn parse_steps(response: &str) -> Result<Vec<ParsedStep>> {
    let cleaned = clean_json_response(response);
    let plan: ParsedPlan = serde_json::from_str(&cleaned)
        .with_context(|| format!("Failed to parse JSON plan from LLM response: {}", cleaned))?;
    Ok(plan.steps)
}

/// Parses the LLM JSON response for recovery decisions. Strips out code blocks if present.
pub fn parse_recovery_decision(response: &str) -> Result<RecoveryDecision> {
    let cleaned = clean_json_response(response);
    let decision: RecoveryDecision = serde_json::from_str(&cleaned).with_context(|| {
        format!(
            "Failed to parse JSON recovery decision from LLM response: {}",
            cleaned
        )
    })?;
    Ok(decision)
}

fn clean_json_response(raw: &str) -> String {
    let mut s = raw.trim();
    if let Some(stripped) = s.strip_prefix("```json") {
        s = stripped;
    } else if let Some(stripped) = s.strip_prefix("```") {
        s = stripped;
    }
    if let Some(stripped) = s.strip_suffix("```") {
        s = stripped;
    }
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_json() {
        let raw = r#"```json
        {
          "steps": [
            {
              "task": "Test cargo",
              "type": "shell_command",
              "command": "cargo test",
              "sandbox_scopes": ["src"],
              "allowed_tools": ["cargo"],
              "allowed_domains": []
            }
          ]
        }
        ```"#;
        let steps = parse_steps(raw).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].task, "Test cargo");
        assert_eq!(steps[0].r#type, "shell_command");
        assert_eq!(steps[0].command.as_deref(), Some("cargo test"));
        assert_eq!(steps[0].sandbox_scopes.as_ref().unwrap()[0], "src");
        assert_eq!(steps[0].allowed_tools.as_ref().unwrap()[0], "cargo");
    }
}
