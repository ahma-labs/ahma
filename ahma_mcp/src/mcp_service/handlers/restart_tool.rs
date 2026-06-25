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

#[cfg(test)]
mod tests {
    use super::restart_schema;
    use crate::shell::cli::AppConfig;
    use crate::test_utils::in_process::build_test_service;
    use serde_json::Map;
    use std::sync::Arc;

    /// `restart_schema()` returns an object schema with no properties and no required fields,
    /// since the restart tool takes no arguments.
    #[test]
    fn restart_schema_is_empty_object_schema() {
        let schema = restart_schema();
        assert_eq!(
            schema.get("type").and_then(|v| v.as_str()),
            Some("object"),
            "restart schema type must be 'object'"
        );
        let props = schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("restart schema must have a 'properties' key");
        assert!(
            props.is_empty(),
            "restart tool takes no arguments; 'properties' must be empty"
        );
        assert!(
            !schema.contains_key("required"),
            "restart tool has no required arguments; 'required' key must not exist"
        );
    }

    /// When `app_config` is `None` (the default for test services), `handle_restart` uses
    /// the hardcoded defaults for both socket path and HTTP URL, then calls
    /// `trigger_bridge_restart`.  The result is a valid restart message regardless of
    /// whether a bridge is actually listening on port 3000 — the important thing is that
    /// the `None` branches in both `RwLock` guards are executed and the function returns `Ok`.
    #[tokio::test]
    async fn handle_restart_no_app_config_executes_none_branches() {
        let (service, _temp_dir) = build_test_service().await.unwrap();
        // app_config is None by default:
        //   socket_path  → "/tmp/ahma.sock"     (the `None` arm on line 26)
        //   http_url     → "http://127.0.0.1:3000"  (the `None` arm on line 40)
        let result = service
            .handle_restart(Map::new())
            .await
            .expect("handle_restart must not return McpError");
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .unwrap_or("");
        // Accept either outcome: if nothing is on port 3000 we get the exit message;
        // if something happens to be listening we get the success message.
        assert!(
            text.contains("No running bridge") || text.contains("Restart request sent"),
            "expected a restart-related message, got: {text:?}"
        );
    }

    /// When `app_config` contains an **empty** `unix_socket_path`, `handle_restart`
    /// falls through to the `is_empty()` branch (defaulting to "/tmp/ahma.sock") and
    /// uses the HTTP URL derived from `http_host`/`http_port`.  A wiremock server
    /// responds 200 to the POST /restart call, so `triggered` is `true` and the handler
    /// returns the success message.
    ///
    /// Covers: `Some(config)` app_config branch (both locks), `is_empty()` true branch,
    /// `format!("http://…")` http_url branch, and `triggered == true` branch.
    #[tokio::test]
    async fn handle_restart_empty_socket_path_with_mock_bridge_returns_success() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/restart"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&mock_server)
            .await;

        let (service, _temp_dir) = build_test_service().await.unwrap();
        let port = mock_server.address().port();
        // Empty unix_socket_path → exercises the `is_empty()` → "/tmp/ahma.sock" branch
        service.set_app_config(Arc::new(AppConfig {
            http_host: "127.0.0.1".to_string(),
            http_port: port,
            unix_socket_path: String::new(),
            ..AppConfig::default()
        }));

        let result = service
            .handle_restart(Map::new())
            .await
            .expect("handle_restart must not return McpError");
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .unwrap_or("");
        assert!(
            text.contains("Restart request sent successfully"),
            "expected the 'triggered=true' success message, got: {text:?}"
        );
    }

    /// When `app_config` contains a **non-empty** `unix_socket_path` and no bridge is
    /// reachable, `handle_restart` takes the `config.unix_socket_path.clone()` branch,
    /// finds no bridge, and returns the "No running bridge" fallback message.
    ///
    /// The 100 ms delayed `std::process::exit(0)` spawned in the false branch is
    /// cancelled when the `#[tokio::test]` runtime is dropped after the test function
    /// returns — no process exit occurs.
    ///
    /// Covers: `Some(config)` branch (both locks), `!is_empty()` socket_path branch,
    /// and `triggered == false` branch.
    #[tokio::test]
    async fn handle_restart_nonempty_socket_path_no_bridge_returns_exit_message() {
        // Bind on port 0 to let the OS pick a free port, then drop the listener so
        // nothing is accepting connections.  reqwest will get ECONNREFUSED immediately.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind must succeed");
        let free_port = listener
            .local_addr()
            .expect("local_addr must succeed")
            .port();
        drop(listener); // free the port; any connect attempt will get ECONNREFUSED

        // Use the platform temp dir rather than a hardcoded "/tmp" path.
        let nonexistent_sock = std::env::temp_dir()
            .join("ahma_test_restart_handler_nonexistent.sock")
            .to_string_lossy()
            .to_string();

        let (service, _temp_dir) = build_test_service().await.unwrap();
        service.set_app_config(Arc::new(AppConfig {
            http_host: "127.0.0.1".to_string(),
            http_port: free_port,
            // Non-empty: exercises `config.unix_socket_path.clone()` on line 23
            unix_socket_path: nonexistent_sock,
            ..AppConfig::default()
        }));

        let result = service
            .handle_restart(Map::new())
            .await
            .expect("handle_restart must not return McpError");
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .unwrap_or("");
        assert!(
            text.contains("No running bridge"),
            "expected 'No running bridge' fallback message, got: {text:?}"
        );
    }

    /// When the `AHMA_UNIX_SOCKET` environment variable is set it takes precedence
    /// over `app_config.unix_socket_path`.  The socket pointed to does not exist, so
    /// the handler falls through to the HTTP trigger, finds no bridge, and returns the
    /// "No running bridge" message.
    ///
    /// Covers: the `Ok(path)` arm of `std::env::var("AHMA_UNIX_SOCKET")` on line 15-16.
    ///
    /// Safety: nextest runs each test in its own process, so setting an env var here
    /// cannot race with other tests in the same binary.
    #[tokio::test]
    async fn handle_restart_env_var_socket_overrides_app_config() {
        // Use a port where nothing is listening.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind must succeed");
        let free_port = listener.local_addr().expect("local_addr").port();
        drop(listener);

        let sock_path = std::env::temp_dir()
            .join("ahma_test_restart_env_var.sock")
            .to_string_lossy()
            .to_string();

        // SAFETY: nextest isolates each test in its own process.
        // The env var is set for this process only.
        unsafe {
            std::env::set_var("AHMA_UNIX_SOCKET", &sock_path);
        }

        let (service, _temp_dir) = build_test_service().await.unwrap();
        // Even though app_config is None, AHMA_UNIX_SOCKET provides the socket path.
        // We still need somewhere for the TCP fallback to fail:
        service.set_app_config(Arc::new(AppConfig {
            http_host: "127.0.0.1".to_string(),
            http_port: free_port,
            unix_socket_path: "/should/not/be/used".to_string(),
            ..AppConfig::default()
        }));

        let result = service.handle_restart(Map::new()).await;

        // Remove the env var before any assertions can fail and leave it set.
        unsafe {
            std::env::remove_var("AHMA_UNIX_SOCKET");
        }

        let result = result.expect("handle_restart must not return McpError");
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .unwrap_or("");
        assert!(
            text.contains("No running bridge"),
            "expected 'No running bridge' fallback message, got: {text:?}"
        );
    }
}
