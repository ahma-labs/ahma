use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use ahma_mcp::mcp_client::{McpServerConfig, McpServerKind, ToolInfo};
use ahma_mcp::test_utils::in_process::InProcessMcp;
use ahma_mcp::utils::logging::init_test_logging;
use rmcp::model::CallToolRequestParams;
use serde_json::json;
use std::borrow::Cow;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string_contains, method, path},
};

async fn create_test_mcp() -> anyhow::Result<InProcessMcp> {
    use ahma_mcp::adapter::Adapter;
    use ahma_mcp::mcp_service::{AhmaMcpService, GuidanceConfig};
    use ahma_mcp::operation_monitor::{MonitorConfig, OperationMonitor};
    use ahma_mcp::sandbox::{Sandbox, SandboxMode};
    use ahma_mcp::shell_pool::{ShellPoolConfig, ShellPoolManager};
    use rmcp::{ServiceExt, transport::async_rw::AsyncRwTransport};
    use std::collections::HashMap;
    use std::sync::Arc;

    // Use Test mode for the routing integration test to bypass platform-specific sandbox checks
    let mode = SandboxMode::Test;

    // Construct sandbox with explicit_scopes = true to skip roots/list config reload race
    let sandbox = Sandbox::new(vec![std::env::current_dir()?], mode, false, false, false)?
        .with_explicit_scopes(true);

    sandbox.set_roots_received(true);

    let monitor_config = MonitorConfig::with_timeout(std::time::Duration::from_secs(300));
    let operation_monitor = Arc::new(OperationMonitor::new(monitor_config));
    let shell_pool = Arc::new(ShellPoolManager::new(ShellPoolConfig::default()));
    let adapter = Arc::new(Adapter::new(
        Arc::clone(&operation_monitor),
        shell_pool,
        Arc::new(sandbox),
    )?);

    let service = AhmaMcpService::new(
        adapter.clone(),
        operation_monitor,
        Arc::new(HashMap::new()),
        Arc::new(None::<GuidanceConfig>),
        false, // force_synchronous
        false, // defer_sandbox
    )
    .await?;

    adapter.sandbox().set_roots_received(true);

    let (client_stream, server_stream) = tokio::io::duplex(65536);
    let (client_read, client_write) = tokio::io::split(client_stream);
    let (server_read, server_write) = tokio::io::split(server_stream);

    let client_transport = AsyncRwTransport::new_client(client_read, client_write);
    let server_transport = AsyncRwTransport::new_server(server_read, server_write);

    let (client_result, server_result) = tokio::join!(
        ().serve(client_transport),
        service.clone().serve(server_transport),
    );

    Ok(InProcessMcp {
        client: client_result?,
        service,
        _server: server_result?,
    })
}

#[tokio::test]
async fn test_external_mcp_tool_listing_and_routing() -> anyhow::Result<()> {
    init_test_logging();

    // 1. Set up a wiremock server to mock the external HTTP MCP server
    let mock_server = MockServer::start().await;

    // Mock initialize call
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("initialize"))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("mcp-session-id", "mock-sess-id-123")
                .set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "serverInfo": {
                            "name": "mock-external-mcp-server",
                            "version": "1.0.0"
                        }
                    }
                })),
        )
        .mount(&mock_server)
        .await;

    // Mock initialized notification
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("notifications/initialized"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock_server)
        .await;

    // Mock tools/call execution
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("tools/call"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "result": {
                "content": [
                    {
                        "type": "text",
                        "text": "routed response from external HTTP server!"
                    }
                ],
                "isError": false
            }
        })))
        .mount(&mock_server)
        .await;

    // 2. Set up the in-process MCP pair
    let mcp = create_test_mcp().await?;

    // 3. Inject our external server configuration into the daemon-side McpConnectionManager
    {
        let mut conn_mgr = mcp.service.mcp_connections.write().await;
        conn_mgr.servers.push(McpServerConfig {
            name: "mock-ext".to_string(),
            enabled: true,
            kind: McpServerKind::Http {
                url: mock_server.uri(),
            },
        });
        conn_mgr.tools_by_server.insert(
            "mock-ext".to_string(),
            vec![ToolInfo {
                name: "test_tool".to_string(),
                description: Some("Mocked external tool".to_string()),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "input": { "type": "string" }
                    }
                }),
            }],
        );
    }

    // 4. Verify list_tools aggregates the external tool
    let tools = tokio::time::timeout(
        TestTimeouts::get(TimeoutCategory::ToolCall),
        mcp.client.list_all_tools(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("list_all_tools timed out"))??;

    let external_tool = tools.iter().find(|t| t.name == "mock-ext::test_tool");
    assert!(
        external_tool.is_some(),
        "Aggregated external tool 'mock-ext::test_tool' should be listed. Found tools: {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    // 5. Verify call_tool routes to external server
    let params = CallToolRequestParams::new(Cow::Borrowed("mock-ext::test_tool"))
        .with_arguments(json!({"input": "ping"}).as_object().unwrap().clone());

    let result = tokio::time::timeout(
        TestTimeouts::get(TimeoutCategory::ToolCall),
        mcp.client.call_tool(params),
    )
    .await
    .map_err(|_| anyhow::anyhow!("call_tool timed out"))??;

    assert!(
        !result.is_error.unwrap_or(false),
        "Tool call to external routed tool should succeed"
    );

    let response_text: String = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        response_text.contains("routed response from external HTTP server!"),
        "Unexpected response from routed tool: {}",
        response_text
    );

    mcp.client.cancel().await?;
    Ok(())
}
