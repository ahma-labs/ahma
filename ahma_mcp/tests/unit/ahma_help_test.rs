//! SPEC R-HELP: `ahma_help` answers before the sandbox scope settles, and what it
//! says about temp space is this session's, not a recital.

use std::collections::HashMap;

use ahma_mcp::test_utils::in_process::create_in_process_mcp_with_client;
use ahma_mcp::test_utils::recording_client::RecordingClient;
use anyhow::Result;
use rmcp::model::CallToolRequestParams;
use serde_json::json;

/// A server whose scope never settles, the state in which an agent most needs help.
async fn unsettled() -> Result<ahma_mcp::test_utils::in_process::InProcessMcp<RecordingClient>> {
    create_in_process_mcp_with_client(
        RecordingClient::new("unrecognized-test-client"),
        HashMap::new(),
        Vec::new(),
    )
    .await
}

async fn ask(
    mcp: &ahma_mcp::test_utils::in_process::InProcessMcp<RecordingClient>,
    topic: Option<&str>,
) -> Result<String> {
    let mut params = CallToolRequestParams::new("ahma_help");
    if let Some(t) = topic {
        params = params.with_arguments(json!({ "topic": t }).as_object().unwrap().clone());
    }
    let result = mcp.client.call_tool(params).await?;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    Ok(result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect())
}

#[tokio::test]
async fn ahma_help_is_advertised_with_one_optional_topic() -> Result<()> {
    let mcp = unsettled().await?;
    let tools = mcp.client.list_all_tools().await?;
    let tool = tools
        .iter()
        .find(|t| t.name == "ahma_help")
        .expect("ahma_help is advertised");
    let schema = serde_json::Value::Object((*tool.input_schema).clone());
    assert!(schema["properties"]["topic"].is_object(), "{schema}");
    assert!(
        schema
            .get("required")
            .is_none_or(|r| r.as_array().is_some_and(|a| a.is_empty())),
        "topic is optional: {schema}"
    );
    Ok(())
}

#[tokio::test]
async fn ahma_help_answers_while_the_sandbox_scope_is_unsettled() -> Result<()> {
    let mcp = unsettled().await?;
    let text = ask(&mcp, None).await?;
    assert!(text.contains("ahma_help topics"), "{text}");
    Ok(())
}

#[tokio::test]
async fn the_temp_topic_reports_this_sessions_real_temp_directory() -> Result<()> {
    let mcp = unsettled().await?;
    let text = ask(&mcp, Some("temp")).await?;
    let temp = std::env::temp_dir();
    let temp = dunce::canonicalize(&temp).unwrap_or(temp);
    assert!(text.contains("AVAILABLE"), "{text}");
    assert!(
        text.contains(&temp.display().to_string()),
        "names {temp:?}: {text}"
    );
    Ok(())
}

#[tokio::test]
async fn an_unknown_topic_is_an_answer_not_an_error() -> Result<()> {
    let mcp = unsettled().await?;
    let text = ask(&mcp, Some("nonsense")).await?;
    assert!(text.starts_with("Unknown topic 'nonsense'"), "{text}");
    Ok(())
}

/// The refusal an agent meets first names the tool that explains it.
#[tokio::test]
async fn the_no_scope_refusal_points_at_the_roots_topic() -> Result<()> {
    let mcp = unsettled().await?;
    let err = mcp
        .client
        .call_tool(
            CallToolRequestParams::new("run_terminal_command")
                .with_arguments(json!({ "command": "true" }).as_object().unwrap().clone()),
        )
        .await
        .expect_err("no scope, no command");
    let text = err.to_string();
    assert!(text.contains("ahma_help topic=roots"), "{text}");
    Ok(())
}
