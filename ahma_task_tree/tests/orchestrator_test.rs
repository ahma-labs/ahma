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
