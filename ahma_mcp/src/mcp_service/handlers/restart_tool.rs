//! Handler for the built-in `restart` MCP tool.

use super::common::text_result;
use crate::AhmaMcpService;
use rmcp::model::{CallToolResult, ErrorData as McpError};
use serde_json::{Map, Value};
use std::sync::Arc;

impl AhmaMcpService {
    /// `restart` — force stop and restart the background bridge.
    pub async fn handle_restart(
        &self,
        _args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let socket_path = if let Ok(path) = std::env::var("AHMA_UNIX_SOCKET") {
            path
        } else {
            let app_config_guard = self.app_config.read().unwrap();
            if let Some(ref config) = *app_config_guard {
                if config.unix_socket_path.is_empty() {
                    "/tmp/ahma.sock".to_string()
                } else {
                    config.unix_socket_path.clone()
                }
            } else {
                "/tmp/ahma.sock".to_string()
            }
        };
        let socket_path_opt = if cfg!(unix) {
            Some(socket_path.as_str())
        } else {
            None
        };

        let http_url = {
            let app_config_guard = self.app_config.read().unwrap();
            if let Some(ref config) = *app_config_guard {
                format!("http://{}:{}", config.http_host, config.http_port)
            } else {
                "http://127.0.0.1:3000".to_string()
            }
        };
        let http_url_opt = Some(http_url.as_str());

        tracing::info!("Tool 'restart' called. Triggering bridge server restart...");

        // Try to trigger restart on the bridge
        let triggered =
            crate::shell::modes::server::trigger_bridge_restart(socket_path_opt, http_url_opt)
                .await;

        if triggered {
            Ok(text_result(
                "Restart request sent successfully. The bridge and all clients will shut down and restart.",
            ))
        } else {
            // If no bridge was running or we couldn't contact it, we can still exit ourselves!
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                std::process::exit(0);
            });
            Ok(text_result(
                "No running bridge detected. Exiting current client process to force restart.",
            ))
        }
    }
}

pub fn restart_schema() -> Arc<Map<String, Value>> {
    crate::mcp_service::schema::object_input_schema(Map::new(), &[])
}
