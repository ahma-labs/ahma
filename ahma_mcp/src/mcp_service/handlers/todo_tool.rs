//! The `todo_write` tool: the agent's task plan / checklist.
//!
//! A multi-step task is easy to lose track of — especially for small models.
//! `todo_write` lets the agent record a plan and update it as it works: it
//! replaces the current checklist with the supplied items and echoes the
//! rendered list back, so the plan is re-grounded in the conversation every
//! turn. The list is also surfaced in the TUI (follow-up). One list per service
//! instance, which suits the single-user TUI.

use super::common::{mcp_invalid_params, text_result};
use crate::AhmaMcpService;
use crate::mcp_service::schema;
use rmcp::model::{CallToolResult, ErrorData as McpError};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// Status of a single plan item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    /// Checkbox glyph for the rendered list.
    fn glyph(self) -> char {
        match self {
            TodoStatus::Pending => ' ',
            TodoStatus::InProgress => '~',
            TodoStatus::Completed => 'x',
        }
    }
}

/// One item in the agent's task plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

/// Render a checklist as text with a `done/total` header, e.g.:
/// `Plan (1/3 done):\n[x] read SPEC\n[~] edit\n[ ] test`.
pub fn render_todos(todos: &[TodoItem]) -> String {
    if todos.is_empty() {
        return "Plan is empty.".to_string();
    }
    let done = todos
        .iter()
        .filter(|t| t.status == TodoStatus::Completed)
        .count();
    let mut out = format!("Plan ({done}/{} done):", todos.len());
    for t in todos {
        out.push_str(&format!("\n[{}] {}", t.status.glyph(), t.content));
    }
    out
}

impl AhmaMcpService {
    pub async fn handle_todo_write(
        &self,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let raw = args
            .get("todos")
            .and_then(Value::as_array)
            .ok_or_else(|| mcp_invalid_params("'todos' (array) is required"))?;

        let mut items = Vec::with_capacity(raw.len());
        for (i, entry) in raw.iter().enumerate() {
            let content = entry
                .get("content")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    mcp_invalid_params(format!("todos[{i}].content must be a non-empty string"))
                })?;
            // Status is optional; default to pending for a freshly listed step.
            let status = match entry.get("status").and_then(Value::as_str) {
                None | Some("pending") => TodoStatus::Pending,
                Some("in_progress") => TodoStatus::InProgress,
                Some("completed") => TodoStatus::Completed,
                Some(other) => {
                    return Err(mcp_invalid_params(format!(
                        "todos[{i}].status must be pending|in_progress|completed (got '{other}')"
                    )));
                }
            };
            items.push(TodoItem {
                content: content.to_string(),
                status,
            });
        }

        let rendered = render_todos(&items);
        *self.todo_list.lock().await = items;
        Ok(text_result(rendered))
    }
}

/// Input schema for the `todo_write` tool.
pub fn todo_write_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "todos".to_string(),
        json!({
            "type": "array",
            "description": "The full task plan — replaces the current checklist. List every step; mark exactly the one you are working on as in_progress and finished steps as completed.",
            "items": {
                "type": "object",
                "properties": {
                    "content": {"type": "string", "description": "Short imperative description of the step."},
                    "status": {
                        "type": "string",
                        "enum": ["pending", "in_progress", "completed"],
                        "description": "Step status. Defaults to pending."
                    }
                },
                "required": ["content"],
                "additionalProperties": false
            }
        }),
    );
    schema::object_input_schema(props, &["todos"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_shows_progress_and_glyphs() {
        let todos = vec![
            TodoItem {
                content: "read".into(),
                status: TodoStatus::Completed,
            },
            TodoItem {
                content: "edit".into(),
                status: TodoStatus::InProgress,
            },
            TodoItem {
                content: "test".into(),
                status: TodoStatus::Pending,
            },
        ];
        let out = render_todos(&todos);
        assert!(out.starts_with("Plan (1/3 done):"), "got: {out}");
        assert!(out.contains("[x] read"));
        assert!(out.contains("[~] edit"));
        assert!(out.contains("[ ] test"));
    }

    #[test]
    fn render_empty_plan() {
        assert_eq!(render_todos(&[]), "Plan is empty.");
    }
}
