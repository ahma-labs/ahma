//! What tools a model is offered, and how a small model asks for more
//! (SPEC R24.12.8).
//!
//! Every tool schema is part of the prompt the model re-reads on every turn.
//! A cloud model reads 60 of them in a blink; a 27B model on a laptop spent
//! most of a 13k-token first prompt — minutes of prefill — on tools the task
//! never touched. So when the small-model harness is on (always, for a model on
//! this machine), the model starts with the core tools and one more,
//! [`MORE_TOOLS`], which lists the other groups and opens one on request.
//!
//! Opening a group is not a permission: every tool still goes through the same
//! approval path. It only changes what the model is *offered*. A group stays
//! open for the folder for as long as this ahma process runs, so the next
//! message does not have to ask again.
//!
//! A tool that exists but has not been offered is refused with the group that
//! holds it, never silently run — the model would otherwise learn that the
//! menu does not matter.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};

use ahma_mcp::builtin_tool::BuiltinTool;
use ahma_mcp::mcp_client::ToolInfo;
use ahma_mcp::mcp_service::bundle_registry::BUNDLES;
use serde_json::{Value, json};

/// The agent-loop-only tool a small model calls to open a group.
pub const MORE_TOOLS: &str = "more_tools";

/// Groups opened per folder, for the life of this process.
static OPEN_GROUPS: LazyLock<Mutex<HashMap<PathBuf, BTreeSet<String>>>> =
    LazyLock::new(Default::default);

/// One group of tools a small model can open.
#[derive(Debug, Clone)]
struct Group {
    summary: String,
    tools: Vec<String>,
}

/// The tools for one agent run, and which of them the model is offered.
#[derive(Debug)]
pub struct ToolMenu {
    workspace: PathBuf,
    /// Every tool's definition, keyed by name, in the order they came.
    definitions: Vec<(String, Value)>,
    /// `None` when every tool is offered (not lean).
    groups: Option<BTreeMap<String, Group>>,
}

impl ToolMenu {
    /// Build the menu. `lean` offers the core set plus [`MORE_TOOLS`]; otherwise
    /// every tool is offered, as before.
    pub fn new(tools: Vec<ToolInfo>, lean: bool, workspace: &Path) -> Self {
        let mut groups: BTreeMap<String, Group> = BTreeMap::new();
        let mut definitions = Vec::with_capacity(tools.len());
        for tool in tools {
            if let Some(group) = group_of(&tool.name) {
                let entry = groups.entry(group.clone()).or_insert_with(|| Group {
                    summary: summary_of(&group),
                    tools: Vec::new(),
                });
                entry.tools.push(tool.name.clone());
            }
            definitions.push((tool.name.clone(), definition(tool)));
        }
        Self {
            workspace: workspace.to_path_buf(),
            definitions,
            groups: lean.then_some(groups),
        }
    }

    /// Whether the model starts with the core set only.
    pub fn is_lean(&self) -> bool {
        self.groups.is_some()
    }

    fn open_groups(&self) -> BTreeSet<String> {
        OPEN_GROUPS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&self.workspace)
            .cloned()
            .unwrap_or_default()
    }

    /// The group holding `tool`, when it is one the model has not been offered.
    fn closed_group_of(&self, tool: &str) -> Option<String> {
        let groups = self.groups.as_ref()?;
        let open = self.open_groups();
        groups
            .iter()
            .find(|(name, g)| !open.contains(*name) && g.tools.iter().any(|t| t == tool))
            .map(|(name, _)| name.clone())
    }

    /// The definitions to send with the next request.
    pub fn definitions(&self) -> Vec<Value> {
        let Some(groups) = &self.groups else {
            return self.definitions.iter().map(|(_, d)| d.clone()).collect();
        };
        let open = self.open_groups();
        let mut out: Vec<Value> = self
            .definitions
            .iter()
            .filter(|(name, _)| self.closed_group_of(name).is_none())
            .map(|(_, d)| d.clone())
            .collect();
        let closed: Vec<(&String, &Group)> =
            groups.iter().filter(|(g, _)| !open.contains(*g)).collect();
        if !closed.is_empty() {
            out.push(more_tools_definition(&closed));
        }
        out
    }

    /// A short label of what is offered: `all`, `core`, `core + git, web`.
    pub fn label(&self) -> String {
        if self.groups.is_none() {
            return "all".to_string();
        }
        let open = self.open_groups();
        if open.is_empty() {
            "core".to_string()
        } else {
            format!("core + {}", open.into_iter().collect::<Vec<_>>().join(", "))
        }
    }

    /// Answer a [`MORE_TOOLS`] call: open the group and say what it holds.
    pub fn open(&self, args: &Value) -> (String, bool) {
        let Some(groups) = &self.groups else {
            return ("Every tool is already available.".to_string(), false);
        };
        let requested = args
            .get("group")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        let Some(group) = groups.get(&requested) else {
            let names: Vec<&str> = groups.keys().map(String::as_str).collect();
            return (
                format!(
                    "Error: there is no tool group '{requested}'. Groups: {}.",
                    names.join(", ")
                ),
                true,
            );
        };
        OPEN_GROUPS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(self.workspace.clone())
            .or_default()
            .insert(requested.clone());
        (
            format!(
                "Opened '{requested}'. From your next step you can call: {}.",
                group.tools.join(", ")
            ),
            false,
        )
    }

    /// When `tool` exists but has not been offered, the refusal to return
    /// instead of running it.
    pub fn refusal_for(&self, tool: &str) -> Option<String> {
        let group = self.closed_group_of(tool)?;
        Some(format!(
            "Error: '{tool}' is not available yet. Call {MORE_TOOLS} with \
             {{\"group\": \"{group}\"}} first, then call it again."
        ))
    }
}

/// The group a small model finds `tool` in, or `None` for a core tool.
fn group_of(tool: &str) -> Option<String> {
    if let Some(builtin) = BuiltinTool::from_name(tool) {
        return builtin.small_model_group().map(str::to_string);
    }
    if let Some((server, _)) = tool.split_once("::") {
        return Some(server.to_lowercase());
    }
    let bundle = BUNDLES.iter().find(|b| {
        tool == b.config_tool_name
            || tool
                .strip_prefix(b.config_tool_name)
                .is_some_and(|rest| rest.starts_with('_'))
    });
    Some(bundle.map_or_else(|| "project".to_string(), |b| b.name.to_string()))
}

fn summary_of(group: &str) -> String {
    if let Some(b) = BUNDLES.iter().find(|b| b.name == group) {
        return b.description.to_string();
    }
    match group {
        "web" => "fetch a web page (asks before each new site)",
        "logs" => "read, search and watch ahma's logs",
        "sandbox" => "ask for access outside this folder; restart ahma",
        "edit" => "several edits to one file in a single call",
        "project" => "this project's own tools, from .ahma/*.json",
        "agent" => "run a sub-agent",
        _ => "tools from the MCP server of that name",
    }
    .to_string()
}

fn definition(tool: ToolInfo) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description.unwrap_or_else(|| "MCP tool callable from ahma".to_string()),
            "parameters": tool.input_schema
        }
    })
}

fn more_tools_definition(closed: &[(&String, &Group)]) -> Value {
    let lines: Vec<String> = closed
        .iter()
        .map(|(name, g)| format!("- {name} ({} tools): {}", g.tools.len(), g.summary))
        .collect();
    let names: Vec<&str> = closed.iter().map(|(n, _)| n.as_str()).collect();
    json!({
        "type": "function",
        "function": {
            "name": MORE_TOOLS,
            "description": format!(
                "Open a group of extra tools when the ones you have are not enough. \
                 Only open what the task needs.\n{}",
                lines.join("\n")
            ),
            "parameters": {
                "type": "object",
                "properties": {"group": {"type": "string", "enum": names}},
                "required": ["group"]
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> ToolInfo {
        ToolInfo {
            name: name.to_string(),
            description: Some(format!("{name} does things")),
            input_schema: json!({"type": "object"}),
        }
    }

    fn sample() -> Vec<ToolInfo> {
        [
            "read_file",
            "run_terminal_command",
            "fetch_webpage",
            "git_commit",
            "git_status",
            "gh_pr_create",
            "adb-devices_devices",
            "docs::search",
        ]
        .into_iter()
        .map(tool)
        .collect()
    }

    fn names(defs: &[Value]) -> Vec<String> {
        defs.iter()
            .map(|d| d["function"]["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn a_lean_menu_offers_the_core_and_a_way_to_ask_for_more() {
        let ws = tempfile::tempdir().unwrap();
        let menu = ToolMenu::new(sample(), true, ws.path());

        let offered = names(&menu.definitions());
        assert_eq!(
            offered,
            ["read_file", "run_terminal_command", MORE_TOOLS],
            "only the core, then more_tools"
        );
        assert_eq!(menu.label(), "core");

        let more = &menu.definitions()[2]["function"];
        let groups = more["parameters"]["properties"]["group"]["enum"]
            .as_array()
            .unwrap();
        for g in ["web", "git", "github", "project", "docs"] {
            assert!(groups.iter().any(|v| v == g), "{g} is listed: {groups:?}");
        }
    }

    #[test]
    fn opening_a_group_offers_it_from_the_next_request() {
        let ws = tempfile::tempdir().unwrap();
        let menu = ToolMenu::new(sample(), true, ws.path());
        assert!(menu.refusal_for("git_commit").is_some());

        let (text, failed) = menu.open(&json!({"group": "git"}));
        assert!(!failed, "{text}");
        assert!(text.contains("git_commit") && text.contains("git_status"));

        let offered = names(&menu.definitions());
        assert!(offered.contains(&"git_commit".to_string()));
        assert!(!offered.contains(&"gh_pr_create".to_string()));
        assert!(menu.refusal_for("git_commit").is_none());
        assert_eq!(menu.label(), "core + git");

        // The next run in the same folder starts with it open.
        let next = ToolMenu::new(sample(), true, ws.path());
        assert!(names(&next.definitions()).contains(&"git_status".to_string()));
    }

    #[test]
    fn an_unknown_group_is_an_error_that_lists_the_real_ones() {
        let ws = tempfile::tempdir().unwrap();
        let menu = ToolMenu::new(sample(), true, ws.path());
        let (text, failed) = menu.open(&json!({"group": "everything"}));
        assert!(failed);
        assert!(text.contains("git") && text.contains("web"), "{text}");
    }

    #[test]
    fn an_unoffered_tool_is_refused_with_its_group() {
        let ws = tempfile::tempdir().unwrap();
        let menu = ToolMenu::new(sample(), true, ws.path());
        let refusal = menu.refusal_for("fetch_webpage").unwrap();
        assert!(refusal.contains("\"web\""), "{refusal}");
        assert!(menu.refusal_for("read_file").is_none(), "core is offered");
        assert!(
            menu.refusal_for("no_such_tool").is_none(),
            "an unknown tool is the dispatcher's to report"
        );
    }

    #[test]
    fn a_full_menu_offers_everything_and_no_more_tools() {
        let ws = tempfile::tempdir().unwrap();
        let menu = ToolMenu::new(sample(), false, ws.path());
        let offered = names(&menu.definitions());
        assert_eq!(offered.len(), sample().len());
        assert!(!offered.contains(&MORE_TOOLS.to_string()));
        assert!(menu.refusal_for("fetch_webpage").is_none());
        assert_eq!(menu.label(), "all");
    }

    #[test]
    fn project_tools_group_by_bundle_or_as_project() {
        assert_eq!(group_of("git_commit").as_deref(), Some("git"));
        assert_eq!(group_of("gh_pr_create").as_deref(), Some("github"));
        assert_eq!(group_of("file-tools_ls").as_deref(), Some("fileutils"));
        assert_eq!(group_of("gitx_thing").as_deref(), Some("project"));
        assert_eq!(group_of("read_file"), None);
        assert_eq!(group_of("srv::x").as_deref(), Some("srv"));
    }
}
