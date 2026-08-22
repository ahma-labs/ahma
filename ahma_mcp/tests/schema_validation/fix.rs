//! Regression test for the VSCode GitHub Copilot Chat catastrophic failure:
//! "tool parameters array type must have items".
//!
//! Every array-typed parameter in a generated tool schema MUST carry an
//! `items` object with a `type` (strings, for CLI tools), and every schema
//! must be a well-formed `type: "object"` schema. A single tool violating
//! this made VSCode reject ahma's *entire* tool list.
//!
//! # Design Note
//!
//! Uses the in-memory API (`load_tool_configs` + `generate_schema_for_tool_config`)
//! instead of spawning a subprocess MCP server — the schemas under test are pure
//! functions of static configuration, so there is nothing a subprocess would add
//! except OS-scheduling jitter. See `schema_validation_test.rs` for the pattern.

use ahma_mcp::config::load_tool_configs;
use ahma_mcp::mcp_service::schema::generate_schema_for_tool_config;
use ahma_mcp::shell::cli::AppConfig;
use ahma_mcp::utils::logging::init_test_logging;
use serde_json::Value;
use std::path::Path;

/// Validate all tools generated from the real `.ahma/` configs (plus the
/// synthetic `run_terminal_command`): schemas are objects, and every
/// array parameter has a string-typed `items` property.
#[tokio::test]
async fn test_all_tool_array_parameters_have_items() -> anyhow::Result<()> {
    init_test_logging();

    // Anchor at the workspace root so the test works under both `cargo test`
    // (package-root cwd) and `cargo nextest` (workspace-root cwd).
    let tools_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate dir has a parent")
        .join(".ahma");

    let config = AppConfig::default();
    let tools = load_tool_configs(&config, Some(tools_dir.as_path()))
        .await
        .expect("Failed to load tool configs from .ahma/");

    assert!(!tools.is_empty(), "No tool configs loaded from .ahma/");

    let mut total_array_params = 0;

    for (tool_name, tool_config) in &tools {
        let schema = generate_schema_for_tool_config(tool_config);

        // Every generated schema must be a well-formed object schema.
        assert_eq!(
            schema.get("type"),
            Some(&Value::String("object".to_string())),
            "Tool '{}': schema top-level type must be \"object\"",
            tool_name
        );
        let properties = schema
            .get("properties")
            .unwrap_or_else(|| panic!("Tool '{}': schema must have properties", tool_name))
            .as_object()
            .unwrap_or_else(|| panic!("Tool '{}': properties must be an object", tool_name));

        for (param_name, param_schema) in properties {
            let Some(param_obj) = param_schema.as_object() else {
                panic!(
                    "Tool '{}': property '{}' schema must be an object",
                    tool_name, param_name
                );
            };
            if param_obj.get("type") != Some(&Value::String("array".to_string())) {
                continue;
            }
            total_array_params += 1;

            // THE CRITICAL CHECK: array parameters must have an `items`
            // property, or VSCode rejects the whole tool list with
            // "tool parameters array type must have items".
            let items = param_obj
                .get("items")
                .unwrap_or_else(|| {
                    panic!(
                        "CRITICAL: Array parameter '{}' in tool '{}' MUST have 'items' \
                         property! This caused VSCode GitHub Copilot Chat to fail with: \
                         'tool parameters array type must have items'",
                        param_name, tool_name
                    )
                })
                .as_object()
                .unwrap_or_else(|| {
                    panic!(
                        "Items of array parameter '{}' in tool '{}' must be an object",
                        param_name, tool_name
                    )
                });

            // For command-line tools, array items are strings.
            assert_eq!(
                items.get("type"),
                Some(&Value::String("string".to_string())),
                "Array items must have type \"string\" for CLI parameter '{}' in tool '{}'",
                param_name,
                tool_name
            );
        }
    }

    // Guard against vacuous success: the repo's .ahma/ configs contain
    // array-typed options (e.g. file-tools, python, simplify), so at least
    // one array parameter must have been validated.
    assert!(
        total_array_params >= 1,
        "Expected at least one array parameter across .ahma/ tools; \
         validated {} — the test would otherwise be vacuous",
        total_array_params
    );

    Ok(())
}
