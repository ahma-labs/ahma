use anyhow::Result;
use parking_lot::Mutex;
use rmcp::model::CallToolRequestParams;
use serde_json::json;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ahma_harness_tools::{DirEntryInfo, GrepMatch, WebFetchResult};
use ahma_mcp::operation_monitor::{MonitorConfig, OperationMonitor};
use ahma_mcp::sandbox::{Sandbox, SandboxMode};
use ahma_mcp::shell_pool::{ShellPoolConfig, ShellPoolManager};
use ahma_mcp::{
    Adapter, AhmaMcpService, CommandExecutor, DefaultCommandExecutor, FileOpsProvider,
    WebPageFetcher,
};
use rmcp::ServiceExt;
use rmcp::service::{RoleClient, RoleServer, RunningService};
use rmcp::transport::async_rw::AsyncRwTransport;

struct CustomInProcessMcp {
    pub client: RunningService<RoleClient, ()>,
    pub _server: RunningService<RoleServer, AhmaMcpService>,
}

// 1. Mock FileOpsProvider
#[derive(Debug, Clone)]
struct MockFileOpsProvider {
    custom_content: String,
}

#[async_trait::async_trait]
impl FileOpsProvider for MockFileOpsProvider {
    async fn read_file(
        &self,
        _scopes: &[PathBuf],
        path: &Path,
        _start_line: Option<usize>,
        _end_line: Option<usize>,
    ) -> Result<String> {
        if path.to_string_lossy().contains("mock_file.txt") {
            Ok(self.custom_content.clone())
        } else {
            Ok("Default fallback".to_string())
        }
    }

    async fn list_dir(&self, _scopes: &[PathBuf], _path: &Path) -> Result<Vec<DirEntryInfo>> {
        Ok(vec![])
    }

    async fn write_file(&self, _scopes: &[PathBuf], _path: &Path, _content: &str) -> Result<()> {
        Ok(())
    }

    async fn replace_in_file(
        &self,
        _scopes: &[PathBuf],
        _path: &Path,
        _old_str: &str,
        _new_str: &str,
    ) -> Result<usize> {
        Ok(0)
    }

    async fn file_search(
        &self,
        _scopes: &[PathBuf],
        _base_dir: &Path,
        _pattern: &str,
    ) -> Result<Vec<String>> {
        Ok(vec![])
    }

    async fn grep_search(
        &self,
        _scopes: &[PathBuf],
        _base_dir: &Path,
        _query: &str,
        _is_regex: bool,
        _include_pattern: Option<&str>,
        _max_results: Option<usize>,
    ) -> Result<Vec<GrepMatch>> {
        Ok(vec![])
    }
}

// 2. Mock CommandExecutor
#[derive(Debug, Clone)]
struct MockCommandExecutor {
    executed_commands: Arc<Mutex<Vec<String>>>,
}

impl CommandExecutor for MockCommandExecutor {
    fn build_command(
        &self,
        sandbox: &Sandbox,
        program: &str,
        args: &[String],
        working_dir: &Path,
    ) -> Result<tokio::process::Command> {
        let cmd_str = format!("{} {}", program, args.join(" "));
        self.executed_commands.lock().push(cmd_str);

        // Redirect execution to `echo 'Intercepted!'`
        DefaultCommandExecutor.build_command(
            sandbox,
            "echo",
            &["Intercepted!".to_string()],
            working_dir,
        )
    }
}

// 3. Mock WebPageFetcher
#[derive(Debug, Clone)]
struct MockWebPageFetcher {
    mock_title: String,
}

#[async_trait::async_trait]
impl WebPageFetcher for MockWebPageFetcher {
    async fn fetch(&self, _url: &str, _query: Option<&str>) -> Result<WebFetchResult> {
        Ok(WebFetchResult {
            url: "https://example.com".to_string(),
            title: Some(self.mock_title.clone()),
            text: "Mock Web Content".to_string(),
        })
    }
}

// Helper to setup test environment using custom components
async fn setup_custom_mcp(
    file_ops: MockFileOpsProvider,
    executor: MockCommandExecutor,
    fetcher: MockWebPageFetcher,
) -> Result<CustomInProcessMcp> {
    let mode = SandboxMode::Test;
    let sandbox = Sandbox::new(vec![std::env::current_dir()?], mode, false, false, false)?;

    let monitor_config = MonitorConfig::with_timeout(std::time::Duration::from_secs(30));
    let operation_monitor = Arc::new(OperationMonitor::new(monitor_config));
    let shell_pool = Arc::new(ShellPoolManager::new(ShellPoolConfig::default()));

    let adapter = Arc::new(
        Adapter::new(
            Arc::clone(&operation_monitor),
            shell_pool,
            Arc::new(sandbox),
        )?
        .with_command_executor(Arc::new(executor)),
    );

    let service = AhmaMcpService::new(
        adapter,
        operation_monitor,
        Arc::new(HashMap::new()), // No dynamic tools configs, we use built-ins
        Arc::new(None),
        false,
        false,
    )
    .await?
    .with_file_ops_provider(Arc::new(file_ops))
    .with_web_page_fetcher(Arc::new(fetcher));

    // Wire client and server through duplex channel
    let (client_stream, server_stream) = tokio::io::duplex(65536);
    let (client_read, client_write) = tokio::io::split(client_stream);
    let (server_read, server_write) = tokio::io::split(server_stream);

    let client_transport = AsyncRwTransport::new_client(client_read, client_write);
    let server_transport = AsyncRwTransport::new_server(server_read, server_write);

    let (client_result, server_result) =
        tokio::join!(().serve(client_transport), service.serve(server_transport),);

    Ok(CustomInProcessMcp {
        client: client_result?,
        _server: server_result?,
    })
}

#[tokio::test]
async fn test_custom_file_ops_provider_extensibility() -> Result<()> {
    let file_ops = MockFileOpsProvider {
        custom_content: "Intercepted File Content!".to_string(),
    };
    let executor = MockCommandExecutor {
        executed_commands: Arc::new(Mutex::new(vec![])),
    };
    let fetcher = MockWebPageFetcher {
        mock_title: "Mock Title".to_string(),
    };

    let mcp = setup_custom_mcp(file_ops, executor, fetcher).await?;
    let client = &mcp.client;

    let params = CallToolRequestParams::new(Cow::Borrowed("read_file")).with_arguments(
        json!({"path": "mock_file.txt"})
            .as_object()
            .unwrap()
            .clone(),
    );

    let result = client.call_tool(params).await?;
    assert!(!result.is_error.unwrap_or(false));

    let all_text: String = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect();

    assert_eq!(all_text.trim(), "Intercepted File Content!");

    let _ = mcp.client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn test_custom_command_executor_extensibility() -> Result<()> {
    let file_ops = MockFileOpsProvider {
        custom_content: "Content".to_string(),
    };
    let executed_commands = Arc::new(Mutex::new(vec![]));
    let executor = MockCommandExecutor {
        executed_commands: executed_commands.clone(),
    };
    let fetcher = MockWebPageFetcher {
        mock_title: "Mock Title".to_string(),
    };

    let mcp = setup_custom_mcp(file_ops, executor, fetcher).await?;
    let client = &mcp.client;

    let params = CallToolRequestParams::new(Cow::Borrowed("run_terminal_command")).with_arguments(
        json!({
            "command": "cargo --version",
            "execution_mode": "Synchronous"
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    let result = client.call_tool(params).await?;
    assert!(!result.is_error.unwrap_or(false));

    let all_text: String = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect();

    // Check that our mock command was built and run
    assert!(all_text.contains("Intercepted!"), "Result: {}", all_text);

    // Verify that the command string we intercepted was recorded
    {
        let recorded = executed_commands.lock();
        assert!(!recorded.is_empty());
        assert!(
            recorded[0].contains("cargo --version"),
            "Recorded: {:?}",
            recorded
        );
    }

    let _ = mcp.client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn test_custom_web_page_fetcher_extensibility() -> Result<()> {
    let file_ops = MockFileOpsProvider {
        custom_content: "Content".to_string(),
    };
    let executor = MockCommandExecutor {
        executed_commands: Arc::new(Mutex::new(vec![])),
    };
    let fetcher = MockWebPageFetcher {
        mock_title: "Custom Extensible Title".to_string(),
    };

    let mcp = setup_custom_mcp(file_ops, executor, fetcher).await?;
    let client = &mcp.client;

    let params = CallToolRequestParams::new(Cow::Borrowed("fetch_webpage")).with_arguments(
        json!({"url": "https://google.com"})
            .as_object()
            .unwrap()
            .clone(),
    );

    let result = client.call_tool(params).await?;
    assert!(!result.is_error.unwrap_or(false));

    let all_text: String = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect();

    assert!(
        all_text.contains("Custom Extensible Title"),
        "Result: {}",
        all_text
    );
    assert!(
        all_text.contains("Mock Web Content"),
        "Result: {}",
        all_text
    );

    let _ = mcp.client.cancel().await;
    Ok(())
}
