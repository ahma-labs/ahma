//! Handler for the built-in `restart` MCP tool.
//!
//! # Why no `notifications/tools/list_changed` (SPEC R1.4)
//!
//! `restart` is a **process replacement**, not an in-session reload, on every transport:
//!
//! * In production `ahma serve stdio` never serves MCP itself — `run_server_mode`
//!   hands off to `run_as_frontend_and_proxy` unless the process is a server-child.
//!   So the process executing this handler is always a bridge *session subprocess*,
//!   for stdio, HTTP and Unix-socket clients alike.
//! * The handler asks the bridge to restart. The bridge's `POST /restart` calls
//!   `terminate_all(SessionTerminationReason::ClientRequested)` and then
//!   `std::process::exit(0)` — every session subprocess, including this one, dies.
//! * If no bridge answers, this process exits instead.
//!
//! Either way there is no surviving MCP session to notify, and the client learns the
//! new tool list from the fresh `initialize` it performs after reconnecting. Emitting
//! `tools/list_changed` just before the peer is torn down would be a no-op dressed up
//! as compliance.
//!
//! The notification *is* mandatory where the tool set changes **within** a live
//! session: `AhmaMcpService::update_tools` sends it, and that is the path a per-client
//! `.ahma/` overlay takes when the client's root arrives mid-handshake.

use super::common::text_result;
use crate::AhmaMcpService;
use crate::shell::modes::server::{global_mcp_socket_path, is_test_isolated};
use rmcp::model::{CallToolResult, ErrorData as McpError};
use serde_json::{Map, Value};
use std::sync::Arc;

impl AhmaMcpService {
    /// `restart` — force stop and restart the background bridge.
    ///
    /// See the module docs for why this deliberately sends no
    /// `notifications/tools/list_changed`.
    pub async fn handle_restart(
        &self,
        _args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        // R-CFG1.2: AHMA_UNIX_SOCKET is RETIRED — warn and ignore. It used to be read
        // here in preference to the resolved AppConfig, which meant an inherited
        // environment variable, not configuration, decided which socket received a
        // "shut yourself down" POST. `shell::modes::server::resolve_bridge_endpoints`
        // already retired the same read; this handler was the surface that kept it
        // alive.
        crate::warn_retired_env("AHMA_UNIX_SOCKET");

        let socket_path = {
            let app_config_guard = self.app_config.read();
            app_config_guard
                .as_ref()
                .map(|config| config.unix_socket_path.clone())
                .filter(|path| !path.is_empty())
                .unwrap_or_else(global_mcp_socket_path)
        };
        tracing::info!("Tool 'restart' called. Triggering bridge server restart...");

        // Try to trigger restart on the bridge
        let triggered = crate::shell::modes::server::trigger_bridge_restart(&socket_path).await;

        if triggered {
            Ok(text_result(
                "Restart request sent successfully. The bridge and all clients will shut down and restart.",
            ))
        } else if is_test_isolated() {
            // Never exit(0) from a test-isolated process. `trigger_bridge_restart`
            // deliberately strips the machine-global endpoints under test isolation, so
            // this branch is the *normal* outcome in the suite — and a bare
            // `std::process::exit(0)` there terminates the whole test binary, which the
            // runner then scores as a pass for every test that never got to run.
            Ok(text_result(
                "No running bridge detected. Process is test-isolated; not exiting.",
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A stand-in daemon on `sock` that answers every request `200 OK` and
    /// counts them, on every OS.
    fn serve_restart_ok(sock: &std::path::Path) -> (tokio::task::JoinHandle<()>, Arc<AtomicUsize>) {
        let listener = ahma_common::local_socket::LocalListener::bind(sock).expect("bind");
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_bg = hits.clone();
        let server = tokio::spawn(async move {
            while let Ok(mut conn) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                let _ = conn.read(&mut buf).await;
                hits_bg.fetch_add(1, Ordering::SeqCst);
                let _ = conn
                    .write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await;
                let _ = conn.shutdown().await;
            }
        });
        (server, hits)
    }

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
    /// the shared daemon socket, which a test-isolated process is refused (SPEC R-ISO.1),
    /// so the handler reports rather than restarting anything.
    #[tokio::test]
    async fn handle_restart_no_app_config_executes_none_branches() {
        let (service, _temp_dir) = build_test_service().await.unwrap();
        // app_config is None by default, so the socket is the shared one.
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
            text.contains("test-isolated"),
            "a test must never restart the shared daemon, got: {text:?}"
        );
    }

    /// A configured socket with a daemon on it is asked to restart, on every OS —
    /// the daemon's MCP endpoint is that socket everywhere (SPEC R-DAEMON.2).
    #[tokio::test]
    async fn handle_restart_reaches_the_configured_daemon_socket() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let sock = tmp.path().join("mcp.sock");
        let (server, hits) = serve_restart_ok(&sock);

        let (service, _temp_dir) = build_test_service().await.unwrap();
        service.set_app_config(Arc::new(AppConfig {
            unix_socket_path: sock.to_string_lossy().into_owned(),
            ..AppConfig::default()
        }));

        let result = service
            .handle_restart(Map::new())
            .await
            .expect("handle_restart must not return McpError");
        server.abort();
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
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// R-CFG1.2: `AHMA_UNIX_SOCKET` is retired. It must **not** override the resolved
    /// `app_config.unix_socket_path`.
    ///
    /// The env var names a live mock bridge; the config names a socket that does not
    /// exist. If the retired variable were still honored the mock would be hit and the
    /// handler would report success. It must instead take the configured (dead) socket
    /// and report no bridge.
    ///
    /// Safety: nextest runs each test in its own process, so setting an env var here
    /// cannot race with other tests in the same binary.

    #[tokio::test]
    async fn handle_restart_ignores_retired_unix_socket_env_var() {
        // A live bridge that answers POST /restart with 200. Nothing must reach it.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let live_sock = tmp.path().join("live.sock");
        let (server, hits) = serve_restart_ok(&live_sock);
        let live_sock = live_sock.to_string_lossy().into_owned();

        // SAFETY: nextest isolates each test in its own process.
        unsafe {
            std::env::set_var("AHMA_UNIX_SOCKET", &live_sock);
        }

        let (service, _temp_dir) = build_test_service().await.unwrap();
        service.set_app_config(Arc::new(AppConfig {
            unix_socket_path: tmp
                .path()
                .join("configured-but-absent.sock")
                .to_string_lossy()
                .into_owned(),
            ..AppConfig::default()
        }));

        let result = service.handle_restart(Map::new()).await;

        // Remove the env var before any assertion can fail and leave it set.
        unsafe {
            std::env::remove_var("AHMA_UNIX_SOCKET");
        }
        server.abort();

        let result = result.expect("handle_restart must not return McpError");
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .unwrap_or("");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "the socket named by the retired AHMA_UNIX_SOCKET must never be contacted"
        );
        assert!(
            !text.contains("Restart request sent"),
            "a retired env var must not be able to trigger a restart, got: {text:?}"
        );
    }

    /// R1.4 evidence: a `restart` is a process replacement, so the handler must not
    /// pretend to satisfy an in-session notification requirement. With no peer attached
    /// there is nothing to notify, and the handler must still succeed — the client's new
    /// tool list comes from the `initialize` it performs after reconnecting.
    ///
    /// The complementary half of the requirement — that a tool-set change *within* a
    /// live session does send `notifications/tools/list_changed` — is covered by
    /// `AhmaMcpService::update_tools` (see `tests/e2e/update_tools_unit_test.rs`).
    #[tokio::test]
    async fn handle_restart_sends_no_notification_and_does_not_exit_under_test_isolation() {
        let (service, _temp_dir) = build_test_service().await.unwrap();
        // Point at a socket nothing owns so `trigger_bridge_restart` reports false.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        service.set_app_config(Arc::new(AppConfig {
            unix_socket_path: tmp
                .path()
                .join("absent.sock")
                .to_string_lossy()
                .into_owned(),
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
            text.contains("test-isolated"),
            "a test-isolated process must report, not exit: {text:?}"
        );
        // Reaching this line at all proves the handler did not take the exit path.
        assert!(
            service.peer.read().is_none(),
            "no peer is attached, so there is no session a tools/list_changed could reach"
        );
    }
}
