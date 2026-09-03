//! Subcommand resolution for MCP tools.
//!
//! Contains functions for finding and resolving subcommand configurations.

use crate::config::{SubcommandConfig, ToolConfig};

/// Finds the configuration for a subcommand from the tool arguments.
pub fn find_subcommand_config_from_args(
    tool_config: &ToolConfig,
    subcommand_name: Option<String>,
) -> Option<(&SubcommandConfig, Vec<String>)> {
    if !tool_config.enabled {
        tracing::warn!(
            "Attempted to resolve subcommand on disabled tool '{}'",
            tool_config.name
        );
        return None;
    }

    let subcommand_path = subcommand_name.unwrap_or_else(|| "default".to_string());
    let top_level = tool_config.subcommand.as_ref()?;

    tracing::debug!(
        "Finding subcommand for tool '{}': path='{}', has_subcommands=true",
        tool_config.name,
        subcommand_path,
    );

    let (found, mut name_parts) = find_subcommand_in_level(top_level, &subcommand_path)?;

    let mut command_parts = vec![tool_config.command.clone()];
    command_parts.append(&mut name_parts);

    Some((found, command_parts))
}

/// Resolves `remaining` against one level of sibling `subcommands`.
///
/// Tries the **whole** remaining path as a single leaf name first. This is what makes
/// a flat config authored with pre-joined names work — `.ahma/gh.json`'s 13
/// subcommands are a single-level list named `"pr_create"`, `"run_watch"`,
/// `"workflow_view"`, … (no nested `"pr"`/`"run"`/`"workflow"` parents), and the
/// schema/advertisement path (`schema.rs`) already treats each as one opaque leaf, so
/// dispatch must match it the same way rather than assuming every `_` is a nesting
/// boundary. Only when the whole-path match fails does this split off the first
/// `_`-delimited token and descend into a nested `subcommand` list, for genuinely
/// hierarchical configs (e.g. `cargo`'s `nextest_run`, a `nextest` parent with a
/// nested `run`).
fn find_subcommand_in_level<'a>(
    subcommands: &'a [SubcommandConfig],
    remaining: &str,
) -> Option<(&'a SubcommandConfig, Vec<String>)> {
    if let Some(sub) = subcommands
        .iter()
        .find(|s| s.name == remaining && s.enabled)
    {
        let parts = if sub.name == "default" {
            Vec::new()
        } else {
            vec![sub.name.clone()]
        };
        return Some((sub, parts));
    }

    let (first, rest) = remaining.split_once('_')?;
    let sub = subcommands.iter().find(|s| s.name == first && s.enabled)?;
    let nested = sub.subcommand.as_ref()?;
    let (leaf, mut nested_parts) = find_subcommand_in_level(nested, rest)?;

    let mut parts = vec![sub.name.clone()];
    parts.append(&mut nested_parts);
    Some((leaf, parts))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tool_config(name: &str, subcommands: Option<Vec<SubcommandConfig>>) -> ToolConfig {
        ToolConfig {
            name: name.to_string(),
            description: format!("{} tool", name),
            command: name.to_string(),
            subcommand: subcommands,
            input_schema: None,
            timeout_seconds: None,
            synchronous: None,
            hints: Default::default(),
            enabled: true,
            guidance_key: None,
            sequence: None,
            step_delay_ms: None,
            availability_check: None,
            install_instructions: None,
            monitor_level: None,
            monitor_stream: None,
            tool_type: None,
            livelog: None,
            ..Default::default()
        }
    }

    fn make_subcommand(name: &str, enabled: bool) -> SubcommandConfig {
        SubcommandConfig {
            extra: Default::default(),
            name: name.to_string(),
            description: format!("{} subcommand", name),
            enabled,
            ..Default::default()
        }
    }

    fn make_subcommand_with_nested(
        name: &str,
        enabled: bool,
        nested: Vec<SubcommandConfig>,
    ) -> SubcommandConfig {
        SubcommandConfig {
            extra: Default::default(),
            name: name.to_string(),
            description: format!("{} subcommand", name),
            subcommand: Some(nested),
            enabled,
            ..Default::default()
        }
    }

    #[test]
    fn test_find_subcommand_default() {
        let subcommands = vec![make_subcommand("default", true)];
        let config = make_tool_config("test", Some(subcommands));

        let result = find_subcommand_config_from_args(&config, None);
        assert!(result.is_some());
        let (sub, parts) = result.unwrap();
        assert_eq!(sub.name, "default");
        assert_eq!(parts, vec!["test"]);
    }

    #[test]
    fn test_find_subcommand_explicit_name() {
        let subcommands = vec![
            make_subcommand("build", true),
            make_subcommand("test", true),
        ];
        let config = make_tool_config("cargo", Some(subcommands));

        let result = find_subcommand_config_from_args(&config, Some("build".to_string()));
        assert!(result.is_some());
        let (sub, parts) = result.unwrap();
        assert_eq!(sub.name, "build");
        assert_eq!(parts, vec!["cargo", "build"]);
    }

    #[test]
    fn test_find_subcommand_nested() {
        let nested = vec![make_subcommand("run", true)];
        let subcommands = vec![make_subcommand_with_nested("nextest", true, nested)];
        let config = make_tool_config("cargo", Some(subcommands));

        let result = find_subcommand_config_from_args(&config, Some("nextest_run".to_string()));
        assert!(result.is_some());
        let (sub, parts) = result.unwrap();
        assert_eq!(sub.name, "run");
        assert_eq!(parts, vec!["cargo", "nextest", "run"]);
    }

    #[test]
    fn test_find_subcommand_disabled_returns_none() {
        let subcommands = vec![make_subcommand("build", false)];
        let config = make_tool_config("cargo", Some(subcommands));

        let result = find_subcommand_config_from_args(&config, Some("build".to_string()));
        assert!(result.is_none());
    }

    #[test]
    fn test_find_subcommand_disabled_tool_returns_none() {
        let subcommands = vec![make_subcommand("build", true)];
        let mut config = make_tool_config("cargo", Some(subcommands));
        config.enabled = false;

        let result = find_subcommand_config_from_args(&config, Some("build".to_string()));
        assert!(result.is_none());
    }

    #[test]
    fn test_find_subcommand_not_found() {
        let subcommands = vec![make_subcommand("build", true)];
        let config = make_tool_config("cargo", Some(subcommands));

        let result = find_subcommand_config_from_args(&config, Some("nonexistent".to_string()));
        assert!(result.is_none());
    }

    #[test]
    fn test_find_subcommand_no_subcommands() {
        let config = make_tool_config("simple", None);

        let result = find_subcommand_config_from_args(&config, Some("anything".to_string()));
        assert!(result.is_none());
    }

    #[test]
    fn test_find_subcommand_deeply_nested() {
        let level3 = vec![make_subcommand("leaf", true)];
        let level2 = vec![make_subcommand_with_nested("mid", true, level3)];
        let level1 = vec![make_subcommand_with_nested("top", true, level2)];
        let config = make_tool_config("tool", Some(level1));

        let result = find_subcommand_config_from_args(&config, Some("top_mid_leaf".to_string()));
        assert!(result.is_some());
        let (sub, parts) = result.unwrap();
        assert_eq!(sub.name, "leaf");
        assert_eq!(parts, vec!["tool", "top", "mid", "leaf"]);
    }

    /// Reproduces a real dispatch failure: `.ahma/gh.json` authors its 13 subcommands
    /// as a *flat* list whose `name` already contains an underscore (`"pr_create"`,
    /// `"run_watch"`, `"workflow_view"`, …) rather than genuine nested levels (a `"pr"`
    /// parent with `"create"` as a nested child). The schema/advertisement path
    /// (`schema.rs`) treats `"pr_create"` as one opaque leaf name and happily
    /// advertises `gh_pr_create` as a tool, but this function used to split every
    /// `_` unconditionally and look for a sibling literally named `"pr"` — which does
    /// not exist — so every `gh_*` tool call failed with "Subcommand ... not found or
    /// invalid" even though the config plainly lists it. The whole remaining path must
    /// be tried as a single leaf name before splitting on `_`.
    #[test]
    fn test_find_subcommand_flat_underscore_name() {
        let subcommands = vec![
            make_subcommand("pr_create", true),
            make_subcommand("run_watch", true),
        ];
        let config = make_tool_config("gh", Some(subcommands));

        let result = find_subcommand_config_from_args(&config, Some("pr_create".to_string()));
        assert!(
            result.is_some(),
            "a flat, underscore-containing subcommand name must resolve as one leaf"
        );
        let (sub, parts) = result.unwrap();
        assert_eq!(sub.name, "pr_create");
        assert_eq!(parts, vec!["gh", "pr_create"]);
    }

    #[test]
    fn test_find_subcommand_partial_path_no_nested() {
        // If we try top_mid_nonexistent but mid has no nested subcommands
        let level2 = vec![make_subcommand("mid", true)]; // No nested
        let level1 = vec![make_subcommand_with_nested("top", true, level2)];
        let config = make_tool_config("tool", Some(level1));

        // "top_mid" should work (stops at mid)
        let result = find_subcommand_config_from_args(&config, Some("top_mid".to_string()));
        assert!(result.is_some());

        // "top_mid_extra" should fail (mid has no nested subcommands)
        let result2 = find_subcommand_config_from_args(&config, Some("top_mid_extra".to_string()));
        assert!(result2.is_none());
    }
}
