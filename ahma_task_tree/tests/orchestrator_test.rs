use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string_contains, method, path},
};

use ahma_mcp::Adapter;
use ahma_mcp::operation_monitor::{MonitorConfig, OperationMonitor};
use ahma_mcp::sandbox::{Sandbox, SandboxMode};
use ahma_mcp::shell_pool::{ShellPoolConfig, ShellPoolManager};

use ahma_task_tree::{LlmProviderConfig, TaskTreeConfig, TaskTreeOrchestrator};

async fn create_test_adapter(root_path: std::path::PathBuf) -> Arc<Adapter> {
    let monitor_config = MonitorConfig::with_timeout(Duration::from_secs(30));
    let monitor = Arc::new(OperationMonitor::new(monitor_config));
    let shell_pool_config = ShellPoolConfig::default();
    let shell_pool = Arc::new(ShellPoolManager::new(shell_pool_config));
    let sandbox =
        Arc::new(Sandbox::new(vec![root_path], SandboxMode::Test, false, false, false).unwrap());

    Arc::new(Adapter::new(monitor, shell_pool, sandbox).unwrap())
}

#[tokio::test]
async fn test_orchestrator_integration_successful_run() {
    let server = MockServer::start().await;
    let temp = tempdir().unwrap();
    let adapter = create_test_adapter(temp.path().to_path_buf()).await;

    // 1. Mock split/planning request
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("Expected JSON Schema:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": r#"{
                        "steps": [
                            {
                                "task": "Print hello",
                                "type": "shell_command",
                                "command": "echo 'hello'",
                                "sandbox_scopes": [],
                                "allowed_tools": ["echo"]
                            },
                            {
                                "task": "Check output",
                                "type": "llm_call",
                                "instructions": "Verify that output is hello"
                            }
                        ]
                    }"#
                }
            }]
        })))
        .mount(&server)
        .await;

    // 2. Mock reasoning step request
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("Verify that output is hello"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Verification succeeded: output is hello"
                }
            }]
        })))
        .mount(&server)
        .await;

    let config = TaskTreeConfig {
        llm_provider: LlmProviderConfig {
            base_url: server.uri(),
            model: "test-model".to_string(),
            api_key: None,
        },
        max_depth: Some(4),
        max_retries: Some(1),
        max_concurrent: Some(1),
        summarisation_threshold: Some(500),
        llm_timeout_seconds: Some(5),
    };

    let orchestrator = TaskTreeOrchestrator::new(config, adapter).unwrap();
    let result = orchestrator.execute("Print hello and verify it").await;

    assert!(result.is_ok());
    let node_result = result.unwrap();
    assert!(node_result.success);
    assert!(
        node_result
            .summary
            .contains("Verification succeeded: output is hello")
    );
}

#[tokio::test]
async fn test_orchestrator_recovery_backtracking() {
    let server = MockServer::start().await;
    let temp = tempdir().unwrap();
    let adapter = create_test_adapter(temp.path().to_path_buf()).await;

    // 1. Mock first plan request (yields a failing command and a second step)
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("Expected JSON Schema:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": r#"{
                        "steps": [
                            {
                                "task": "Failing step",
                                "type": "shell_command",
                                "command": "false",
                                "sandbox_scopes": [],
                                "allowed_tools": ["false"]
                            },
                            {
                                "task": "Verify step",
                                "type": "llm_call",
                                "instructions": "Verify output"
                            }
                        ]
                    }"#
                }
            }]
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    // 2. Mock recovery decision request (returns re_plan with a correct echo step)
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains(
            "You handle subtask failures and decide how to recover.",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": r#"{
                        "action": "re_plan",
                        "steps": [
                            {
                                "task": "Echo recovery command",
                                "type": "shell_command",
                                "command": "echo 'recovered'",
                                "sandbox_scopes": [],
                                "allowed_tools": ["echo"]
                            }
                        ]
                    }"#
                }
            }]
        })))
        .mount(&server)
        .await;

    let config = TaskTreeConfig {
        llm_provider: LlmProviderConfig {
            base_url: server.uri(),
            model: "test-model".to_string(),
            api_key: None,
        },
        max_depth: Some(4),
        max_retries: Some(0), // No retries to immediately trigger recovery
        max_concurrent: Some(1),
        summarisation_threshold: Some(500),
        llm_timeout_seconds: Some(5),
    };

    let orchestrator = TaskTreeOrchestrator::new(config, adapter).unwrap();
    let result = orchestrator.execute("Run failing task and recover").await;

    assert!(result.is_ok());
    let node_result = result.unwrap();
    assert!(node_result.success);
    assert!(node_result.summary.contains("recovered"));
}

#[tokio::test]
async fn test_task_tree_extension_handler_integration() {
    use ahma_mcp::test_utils::in_process::create_in_process_mcp_from_dir;
    use rmcp::model::CallToolRequestParams;

    let server = MockServer::start().await;
    let temp = tempdir().unwrap();
    let tools_dir = temp.path().join(".ahma");
    std::fs::create_dir_all(&tools_dir).unwrap();

    // Register extension handler globally
    ahma_mcp::register_global_extension_handler(
        "task_tree".to_string(),
        std::sync::Arc::new(ahma_task_tree::TaskTreeExtensionHandler),
    );

    // 1. Success case setup
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("Expected JSON Schema:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": r#"{"steps": []}"#
                }
            }]
        })))
        .mount(&server)
        .await;

    // Create a tool definition config JSON
    let tool_def = json!({
        "name": "task_tree",
        "description": "Run task tree",
        "command": "task_tree",
        "enabled": true,
        "tool_type": "extension",
        "task_tree": {
            "llm_provider": {
                "base_url": server.uri(),
                "model": "test-model"
            }
        }
    });
    std::fs::write(tools_dir.join("task_tree.json"), serde_json::to_string(&tool_def).unwrap()).unwrap();

    let mcp = create_in_process_mcp_from_dir(&tools_dir).await.unwrap();

    // Scenario A: Successful run using 'goal' parameter
    let params = CallToolRequestParams::new(std::borrow::Cow::Borrowed("task_tree"))
        .with_arguments(json!({"goal": "Test goal A"}).as_object().unwrap().clone());
    let result = mcp.client.call_tool(params).await.unwrap();
    assert!(!result.is_error.unwrap_or(false));
    let text = result.content[0].as_text().unwrap().text.clone();
    assert!(text.contains("Goal: Test goal A"));
    assert!(text.contains("Result: SUCCESS"));

    // Scenario B: Successful run using 'query' parameter
    let params = CallToolRequestParams::new(std::borrow::Cow::Borrowed("task_tree"))
        .with_arguments(json!({"query": "Test query B"}).as_object().unwrap().clone());
    let result = mcp.client.call_tool(params).await.unwrap();
    assert!(!result.is_error.unwrap_or(false));
    let text = result.content[0].as_text().unwrap().text.clone();
    assert!(text.contains("Goal: Test query B"));

    // Scenario C: Successful run using 'instructions' parameter
    let params = CallToolRequestParams::new(std::borrow::Cow::Borrowed("task_tree"))
        .with_arguments(json!({"instructions": "Test instructions C"}).as_object().unwrap().clone());
    let result = mcp.client.call_tool(params).await.unwrap();
    assert!(!result.is_error.unwrap_or(false));
    let text = result.content[0].as_text().unwrap().text.clone();
    assert!(text.contains("Goal: Test instructions C"));

    // Scenario D: Missing required parameter (goal/query/instructions)
    let params = CallToolRequestParams::new(std::borrow::Cow::Borrowed("task_tree"))
        .with_arguments(json!({"other_arg": "value"}).as_object().unwrap().clone());
    let result = mcp.client.call_tool(params).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(format!("{:?}", err).contains("Missing required parameter"));

    // Scenario E: Missing arguments payload entirely
    let params = CallToolRequestParams::new(std::borrow::Cow::Borrowed("task_tree"));
    let result = mcp.client.call_tool(params).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(format!("{:?}", err).contains("Missing arguments payload"));
}

#[tokio::test]
async fn test_task_tree_extension_handler_bad_config() {
    use ahma_mcp::test_utils::in_process::create_in_process_mcp_from_dir;
    use rmcp::model::CallToolRequestParams;

    let temp = tempdir().unwrap();
    let tools_dir = temp.path().join(".ahma");
    std::fs::create_dir_all(&tools_dir).unwrap();

    // Register extension handler globally for tool
    ahma_mcp::register_global_extension_handler(
        "task_tree".to_string(),
        std::sync::Arc::new(ahma_task_tree::TaskTreeExtensionHandler),
    );

    // Create a tool def config JSON with invalid task_tree payload (String instead of object)
    let tool_def = json!({
        "name": "task_tree_bad_config",
        "description": "Run task tree with bad config",
        "command": "task_tree_bad_config",
        "enabled": true,
        "tool_type": "extension",
        "task_tree": "invalid_config_payload"
    });
    std::fs::write(tools_dir.join("task_tree_bad_config.json"), serde_json::to_string(&tool_def).unwrap()).unwrap();

    let mcp = create_in_process_mcp_from_dir(&tools_dir).await.unwrap();

    let params = CallToolRequestParams::new(std::borrow::Cow::Borrowed("task_tree_bad_config"))
        .with_arguments(json!({"goal": "Test goal"}).as_object().unwrap().clone());
    let result = mcp.client.call_tool(params).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(format!("{:?}", err).contains("Invalid task tree config"));
}

#[tokio::test]
async fn test_task_tree_extension_handler_fail() {
    use ahma_mcp::test_utils::in_process::create_in_process_mcp_from_dir;
    use rmcp::model::CallToolRequestParams;

    let server = MockServer::start().await;
    let temp = tempdir().unwrap();
    let tools_dir = temp.path().join(".ahma");
    std::fs::create_dir_all(&tools_dir).unwrap();

    // Register extension handler globally for tool
    ahma_mcp::register_global_extension_handler(
        "task_tree".to_string(),
        std::sync::Arc::new(ahma_task_tree::TaskTreeExtensionHandler),
    );

    // Mock completion endpoint to return 500 Internal Server Error (forcing orchestrator execution failure)
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    // Create a tool definition config JSON
    let tool_def = json!({
        "name": "task_tree_fail",
        "description": "Run task tree fail",
        "command": "task_tree_fail",
        "enabled": true,
        "tool_type": "extension",
        "task_tree": {
            "llm_provider": {
                "base_url": server.uri(),
                "model": "test-model"
            }
        }
    });
    std::fs::write(tools_dir.join("task_tree_fail.json"), serde_json::to_string(&tool_def).unwrap()).unwrap();

    let mcp = create_in_process_mcp_from_dir(&tools_dir).await.unwrap();

    let params = CallToolRequestParams::new(std::borrow::Cow::Borrowed("task_tree_fail"))
        .with_arguments(json!({"goal": "Test goal"}).as_object().unwrap().clone());
    let result = mcp.client.call_tool(params).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(format!("{:?}", err).contains("Task tree execution failed"));
}

