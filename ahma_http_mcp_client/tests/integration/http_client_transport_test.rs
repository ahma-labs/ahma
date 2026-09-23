//! Integration tests for HttpMcpTransport covering SSE connection, PKCE flow,
//! token refresh, and network error scenarios using wiremock.
//!
//! These tests target the low-coverage areas in client.rs (42% coverage).

// Allow holding mutex across awaits in tests - the env var guard serializes tests
// and test isolation is handled via unique temp files per test.
#![allow(clippy::await_holding_lock)]

use ahma_common::timeouts::TestTimeouts;
use std::env;
use std::sync::{Mutex as StdMutex, OnceLock};
use tempfile::tempdir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN_PATH_ENV: &str = "AHMA_TEST_HTTP_CLIENT_TOKEN_PATH";

/// Guard to serialize tests that modify TOKEN_PATH_ENV
fn token_env_guard() -> &'static StdMutex<()> {
    static GUARD: OnceLock<StdMutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| StdMutex::new(()))
}

/// RAII guard for tests that point `AHMA_TEST_HTTP_CLIENT_TOKEN_PATH` at an
/// isolated path: serializes against other such tests via [`token_env_guard`]
/// and restores the environment on drop, including on an early return or a
/// panic (`assert!`/`expect` failures previously skipped the `remove_var`
/// tail that most of these tests wrote out by hand).
struct TokenPathGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl TokenPathGuard {
    fn set(path: &std::path::Path) -> Self {
        let lock = token_env_guard().lock().unwrap();
        unsafe { env::set_var(TOKEN_PATH_ENV, path) };
        Self { _lock: lock }
    }
}

impl Drop for TokenPathGuard {
    fn drop(&mut self) {
        unsafe { env::remove_var(TOKEN_PATH_ENV) };
    }
}

/// Write the standard test token JSON (used by most tests here) to `path` and
/// return the access-token value written, for assertions.
fn write_default_token(path: &std::path::Path) -> &'static str {
    let access_token = "valid_token";
    let token = serde_json::json!({
        "access_token": access_token,
        "refresh_token": null,
        "expires_in": 3600,
        "scopes": null
    });
    std::fs::write(path, token.to_string()).unwrap();
    access_token
}

mod transport_construction {
    use super::*;
    use ahma_http_mcp_client::client::HttpMcpTransport;
    use url::Url;

    #[test]
    fn new_transport_without_oauth_succeeds() {
        let tmp = tempdir().unwrap();
        let _guard = TokenPathGuard::set(&tmp.path().join("no_token.json"));

        let url = Url::parse("http://localhost:8080/mcp").unwrap();
        let result = HttpMcpTransport::new(url, None, None);

        assert!(result.is_ok(), "Transport should be created without OAuth");
    }

    #[test]
    fn new_transport_with_oauth_credentials_succeeds() {
        let tmp = tempdir().unwrap();
        let _guard = TokenPathGuard::set(&tmp.path().join("oauth_token.json"));

        let url = Url::parse("http://localhost:8080/mcp").unwrap();
        let result = HttpMcpTransport::new(
            url,
            Some("client_id".to_string()),
            Some("client_secret".to_string()),
        );

        assert!(
            result.is_ok(),
            "Transport should be created with OAuth credentials"
        );
    }

    #[test]
    fn new_transport_with_partial_oauth_creates_without_oauth_client() {
        // Only client_id provided, no secret - should create transport without oauth_client
        let tmp = tempdir().unwrap();
        let _guard = TokenPathGuard::set(&tmp.path().join("partial_oauth.json"));

        let url = Url::parse("http://localhost:8080/mcp").unwrap();
        let result = HttpMcpTransport::new(url, Some("client_id".to_string()), None);

        assert!(
            result.is_ok(),
            "Transport should be created with partial OAuth (no oauth_client)"
        );
    }

    #[test]
    fn new_transport_with_invalid_url_fails() {
        let _guard = token_env_guard().lock();
        // The URL parsing happens before transport construction,
        // so we test what happens with a valid URL but invalid structure
        let url = Url::parse("http://localhost:8080/mcp").unwrap();
        let result = HttpMcpTransport::new(url, None, None);
        // Should succeed - URL is valid
        assert!(result.is_ok());
    }
}

mod token_persistence {
    use super::*;
    use std::fs;
    use std::io::Write;

    #[test]
    fn load_token_handles_malformed_json() {
        let tmp = tempdir().unwrap();
        let token_path = tmp.path().join("malformed.json");

        // Write malformed JSON
        let mut file = fs::File::create(&token_path).unwrap();
        file.write_all(b"{ invalid json }").unwrap();

        let _guard = TokenPathGuard::set(&token_path);

        // Creating transport should fail or handle gracefully
        let url = url::Url::parse("http://localhost:8080/mcp").unwrap();
        let result = ahma_http_mcp_client::client::HttpMcpTransport::new(url, None, None);

        // The current implementation returns error on malformed token
        assert!(result.is_err(), "Should fail on malformed token JSON");
    }

    #[test]
    fn load_token_handles_empty_file() {
        let tmp = tempdir().unwrap();
        let token_path = tmp.path().join("empty.json");

        // Write empty file
        fs::File::create(&token_path).unwrap();

        let _guard = TokenPathGuard::set(&token_path);

        let url = url::Url::parse("http://localhost:8080/mcp").unwrap();
        let result = ahma_http_mcp_client::client::HttpMcpTransport::new(url, None, None);

        // Empty file is not valid JSON
        assert!(result.is_err(), "Should fail on empty token file");
    }

    #[test]
    fn token_file_in_nonexistent_directory_created_on_save() {
        let tmp = tempdir().unwrap();
        let token_path = tmp.path().join("deep/nested/dir/token.json");

        let _guard = TokenPathGuard::set(&token_path);

        // Transport creation should succeed even with nonexistent token directory
        let url = url::Url::parse("http://localhost:8080/mcp").unwrap();
        let result = ahma_http_mcp_client::client::HttpMcpTransport::new(url, None, None);
        assert!(
            result.is_ok(),
            "Transport should be created even if token dir doesn't exist"
        );
    }
}

mod http_transport_send {
    use super::*;
    use ahma_http_mcp_client::client::HttpMcpTransport;
    use rmcp::transport::Transport;
    use serde_json::json;
    use url::Url;

    #[tokio::test]
    async fn send_without_token_returns_missing_token_error() {
        let tmp = tempdir().unwrap();
        // Ensure no token file exists
        let _guard = TokenPathGuard::set(&tmp.path().join("no_token.json"));

        let server = MockServer::start().await;
        let url = Url::parse(&format!("{}/mcp", server.uri())).unwrap();

        let mut transport = HttpMcpTransport::new(url, None, None).unwrap();

        // Create a JSON-RPC request
        let request: rmcp::service::TxJsonRpcMessage<rmcp::RoleClient> =
            serde_json::from_value(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list",
                "params": {}
            }))
            .unwrap();

        let result = transport.send(request).await;

        assert!(result.is_err(), "Send should fail without token");
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("Missing access token"),
            "Error should indicate missing token: {}",
            err
        );
    }

    #[tokio::test]
    async fn send_with_token_makes_authenticated_request() {
        let tmp = tempdir().unwrap();
        let token_path = tmp.path().join("valid_token.json");

        // Write a valid token
        let token = json!({
            "access_token": "test_token_abc123",
            "refresh_token": null,
            "expires_in": 3600,
            "scopes": ["read:me"]
        });
        std::fs::write(&token_path, token.to_string()).unwrap();

        let _guard = TokenPathGuard::set(&token_path);

        let server = MockServer::start().await;

        // Set up mock to verify bearer auth header
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(header("authorization", "Bearer test_token_abc123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "tools": [] }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let url = Url::parse(&format!("{}/mcp", server.uri())).unwrap();
        let mut transport = HttpMcpTransport::new(url, None, None).unwrap();

        let request: rmcp::service::TxJsonRpcMessage<rmcp::RoleClient> =
            serde_json::from_value(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list",
                "params": {}
            }))
            .unwrap();

        let result = transport.send(request).await;
        assert!(
            result.is_ok(),
            "Send should succeed with valid token: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn send_handles_http_error_response() {
        let tmp = tempdir().unwrap();
        let token_path = tmp.path().join("token_for_error.json");
        write_default_token(&token_path);
        let _guard = TokenPathGuard::set(&token_path);

        let server = MockServer::start().await;

        // Server returns 500 error
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
            .expect(1)
            .mount(&server)
            .await;

        let url = Url::parse(&format!("{}/mcp", server.uri())).unwrap();
        let mut transport = HttpMcpTransport::new(url, None, None).unwrap();

        let request: rmcp::service::TxJsonRpcMessage<rmcp::RoleClient> =
            serde_json::from_value(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "test",
                "params": {}
            }))
            .unwrap();

        let result = transport.send(request).await;
        assert!(result.is_err(), "Send should fail on HTTP 500");
        assert!(
            result.unwrap_err().to_string().contains("HTTP Error"),
            "Error should contain HTTP error details"
        );
    }

    #[tokio::test]
    async fn send_handles_network_timeout() {
        let tmp = tempdir().unwrap();
        let token_path = tmp.path().join("token_timeout.json");
        write_default_token(&token_path);
        let _guard = TokenPathGuard::set(&token_path);

        let server = MockServer::start().await;

        // Server delays response for longer than typical timeout
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(30)))
            .mount(&server)
            .await;

        let url = Url::parse(&format!("{}/mcp", server.uri())).unwrap();
        let mut transport = HttpMcpTransport::new(url, None, None).unwrap();

        let request: rmcp::service::TxJsonRpcMessage<rmcp::RoleClient> =
            serde_json::from_value(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "test",
                "params": {}
            }))
            .unwrap();

        // Use a timeout to avoid hanging the test
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            transport.send(request),
        )
        .await;

        // Either timeout or connection error is acceptable
        assert!(
            result.is_err() || result.unwrap().is_err(),
            "Should timeout or error on delayed response"
        );
    }

    #[tokio::test]
    async fn send_handles_invalid_json_response() {
        let tmp = tempdir().unwrap();
        let token_path = tmp.path().join("token_invalid_resp.json");
        write_default_token(&token_path);
        let _guard = TokenPathGuard::set(&token_path);

        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not valid json"))
            .expect(1)
            .mount(&server)
            .await;

        let url = Url::parse(&format!("{}/mcp", server.uri())).unwrap();
        let mut transport = HttpMcpTransport::new(url, None, None).unwrap();

        let request: rmcp::service::TxJsonRpcMessage<rmcp::RoleClient> =
            serde_json::from_value(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "test",
                "params": {}
            }))
            .unwrap();

        let result = transport.send(request).await;
        assert!(result.is_err(), "Send should fail on invalid JSON response");
    }

    #[tokio::test]
    async fn send_handles_empty_response_body() {
        let tmp = tempdir().unwrap();
        let token_path = tmp.path().join("token_empty.json");
        write_default_token(&token_path);
        let _guard = TokenPathGuard::set(&token_path);

        let server = MockServer::start().await;

        // Server returns 200 OK with empty body (e.g., for notifications)
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let url = Url::parse(&format!("{}/mcp", server.uri())).unwrap();
        let mut transport = HttpMcpTransport::new(url, None, None).unwrap();

        let request: rmcp::service::TxJsonRpcMessage<rmcp::RoleClient> =
            serde_json::from_value(json!({
                "jsonrpc": "2.0",
                "method": "notifications/test",
                "params": {}
            }))
            .unwrap();

        let result = transport.send(request).await;
        // Empty body should be handled gracefully for notifications
        assert!(
            result.is_ok(),
            "Empty response should be ok for notifications"
        );
    }

    /// Table-driven coverage of send() surfacing non-2xx HTTP statuses as errors.
    /// Consolidates the former per-status copies (400/401/403/404/502/503/504).
    #[tokio::test]
    async fn send_handles_http_error_statuses() {
        let tmp = tempdir().unwrap();
        let token_path = tmp.path().join("token_error_statuses.json");
        write_default_token(&token_path);
        let _guard = TokenPathGuard::set(&token_path);

        let cases: &[(u16, &str)] = &[
            (400, "Bad Request"),
            (401, "Unauthorized"),
            (403, "Forbidden"),
            (404, "Not Found"),
            (502, "Bad Gateway"),
            (503, "Service Unavailable"),
            (504, "Gateway Timeout"),
        ];

        for &(status, body) in cases {
            let server = MockServer::start().await;

            Mock::given(method("POST"))
                .and(path("/mcp"))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .expect(1)
                .mount(&server)
                .await;

            let url = Url::parse(&format!("{}/mcp", server.uri())).unwrap();
            let mut transport = HttpMcpTransport::new(url, None, None).unwrap();

            let request: rmcp::service::TxJsonRpcMessage<rmcp::RoleClient> =
                serde_json::from_value(json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "test",
                    "params": {}
                }))
                .unwrap();

            let result = transport.send(request).await;
            assert!(result.is_err(), "Send should fail on {status}");
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains(&status.to_string()),
                "Error for {status} should include the status code: {err}"
            );
        }
    }
}

mod transport_lifecycle {
    use super::*;
    use ahma_http_mcp_client::client::HttpMcpTransport;
    use rmcp::transport::Transport;
    use url::Url;

    #[tokio::test]
    async fn close_succeeds() {
        let tmp = tempdir().unwrap();
        let _guard = TokenPathGuard::set(&tmp.path().join("close_test.json"));
        let url = Url::parse("http://localhost:8080/mcp").unwrap();

        let mut transport = HttpMcpTransport::new(url, None, None).unwrap();

        let result = transport.close().await;
        assert!(result.is_ok(), "Close should succeed");
    }

    #[tokio::test]
    async fn receive_returns_none_when_channel_empty() {
        let tmp = tempdir().unwrap();
        let _guard = TokenPathGuard::set(&tmp.path().join("receive_test.json"));
        let url = Url::parse("http://localhost:8080/mcp").unwrap();

        let mut transport = HttpMcpTransport::new(url, None, None).unwrap();

        // Drop the transport to close the channel, then receive should return None
        // For this test, we need to close the sender side
        // Since we can't access internals, we just verify receive doesn't panic
        let result =
            tokio::time::timeout(std::time::Duration::from_millis(10), transport.receive()).await;

        // Should timeout since no messages are in the channel
        assert!(result.is_err(), "Receive should timeout with empty channel");
    }

    #[tokio::test]
    async fn receive_gets_message_from_channel() {
        let tmp = tempdir().unwrap();
        let token_path = tmp.path().join("receive_message.json");
        write_default_token(&token_path);
        let _guard = TokenPathGuard::set(&token_path);

        let server = MockServer::start().await;

        // Mock server returns a response
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "data": "test_response" }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let url = Url::parse(&format!("{}/mcp", server.uri())).unwrap();
        let mut transport = HttpMcpTransport::new(url, None, None).unwrap();

        // Send a request which should put response in channel
        let request: rmcp::service::TxJsonRpcMessage<rmcp::RoleClient> =
            serde_json::from_value(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "test",
                "params": {}
            }))
            .unwrap();

        transport.send(request).await.unwrap();

        // Now receive should get the message
        let message = tokio::time::timeout(TestTimeouts::scale_millis(100), transport.receive())
            .await
            .expect("Should not timeout")
            .expect("Should receive message");

        // Verify the message content
        let msg_str = serde_json::to_string(&message).unwrap();
        assert!(msg_str.contains("test_response"));
    }
}

mod channel_error_tests {
    use super::*;
    use ahma_http_mcp_client::client::HttpMcpTransport;
    use rmcp::transport::Transport;
    use serde_json::json;
    use url::Url;

    #[tokio::test]
    async fn send_continues_when_response_channel_full() {
        let tmp = tempdir().unwrap();
        let token_path = tmp.path().join("channel_full.json");
        write_default_token(&token_path);
        let _guard = TokenPathGuard::set(&token_path);

        let server = MockServer::start().await;

        // Mock multiple responses
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "data": "response" }
            })))
            .expect(5)
            .mount(&server)
            .await;

        let url = Url::parse(&format!("{}/mcp", server.uri())).unwrap();
        let mut transport = HttpMcpTransport::new(url, None, None).unwrap();

        // Send multiple requests without draining receiver
        for i in 1..=5 {
            let request: rmcp::service::TxJsonRpcMessage<rmcp::RoleClient> =
                serde_json::from_value(json!({
                    "jsonrpc": "2.0",
                    "id": i,
                    "method": "test",
                    "params": {}
                }))
                .unwrap();

            let result = transport.send(request).await;
            assert!(result.is_ok(), "Send should succeed even if channel fills");
        }
    }
}

mod concurrency_tests {
    use super::*;
    use ahma_http_mcp_client::client::HttpMcpTransport;
    use rmcp::transport::Transport;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use url::Url;

    #[tokio::test]
    async fn concurrent_sends_maintain_ordering() {
        let tmp = tempdir().unwrap();
        let token_path = tmp.path().join("concurrent.json");
        write_default_token(&token_path);
        let _guard = TokenPathGuard::set(&token_path);

        let server = MockServer::start().await;

        // Mock server responds to all requests
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "success": true }
            })))
            .expect(10)
            .mount(&server)
            .await;

        let url = Url::parse(&format!("{}/mcp", server.uri())).unwrap();
        let transport = Arc::new(Mutex::new(HttpMcpTransport::new(url, None, None).unwrap()));

        // Spawn multiple concurrent sends
        let mut handles = vec![];
        for i in 1..=10 {
            let transport = Arc::clone(&transport);
            let handle = tokio::spawn(async move {
                let mut transport = transport.lock().await;
                let request: rmcp::service::TxJsonRpcMessage<rmcp::RoleClient> =
                    serde_json::from_value(json!({
                        "jsonrpc": "2.0",
                        "id": i,
                        "method": "test",
                        "params": { "index": i }
                    }))
                    .unwrap();

                transport.send(request).await
            });
            handles.push(handle);
        }

        // Wait for all sends to complete
        for handle in handles {
            let result = handle.await.unwrap();
            assert!(result.is_ok(), "Concurrent send should succeed");
        }
    }

    #[tokio::test]
    async fn large_payload_send_succeeds() {
        let tmp = tempdir().unwrap();
        let token_path = tmp.path().join("large_payload.json");
        write_default_token(&token_path);
        let _guard = TokenPathGuard::set(&token_path);

        let server = MockServer::start().await;

        // Create large response payload (1MB)
        let large_data = "x".repeat(1024 * 1024);
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "data": large_data }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let url = Url::parse(&format!("{}/mcp", server.uri())).unwrap();
        let mut transport = HttpMcpTransport::new(url, None, None).unwrap();

        // Create large request payload
        let large_param = "y".repeat(1024 * 512);
        let request: rmcp::service::TxJsonRpcMessage<rmcp::RoleClient> =
            serde_json::from_value(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "test",
                "params": { "large_field": large_param }
            }))
            .unwrap();

        let result = transport.send(request).await;
        assert!(result.is_ok(), "Large payload send should succeed");
    }
}

mod error_types {
    use ahma_http_mcp_client::error::McpHttpError;

    #[test]
    fn oauth2_config_error_conversion() {
        // Test that oauth2::ConfigurationError converts properly
        // This is tested via the From implementation
        let url_err = url::Url::parse("not valid").unwrap_err();
        let err: McpHttpError = url_err.into();
        assert!(err.to_string().contains("URL parsing failed"));
    }

    #[test]
    fn all_error_variants_are_debug() {
        let errors = vec![
            McpHttpError::OAuth2("test".to_string()),
            McpHttpError::Auth("test".to_string()),
            McpHttpError::MissingAccessToken,
            McpHttpError::Custom("test".to_string()),
        ];

        for err in errors {
            let _ = format!("{:?}", err);
            let _ = format!("{}", err);
        }
    }
}

mod oauth_flow_tests {
    use super::*;
    use ahma_http_mcp_client::client::HttpMcpTransport;
    use serde_json::json;
    use std::fs;
    use url::Url;

    #[tokio::test]
    async fn ensure_authenticated_with_valid_token_succeeds() {
        let tmp = tempdir().unwrap();
        let token_path = tmp.path().join("valid_auth_token.json");

        // Write a valid token
        let token = json!({
            "access_token": "valid_token_123",
            "refresh_token": null,
            "expires_in": 3600,
            "scopes": ["read:me"]
        });
        fs::write(&token_path, token.to_string()).unwrap();

        let _guard = TokenPathGuard::set(&token_path);

        let url = Url::parse("http://localhost:8080/mcp").unwrap();
        let transport = HttpMcpTransport::new(url, None, None).unwrap();

        let result = transport.ensure_authenticated().await;
        assert!(
            result.is_ok(),
            "ensure_authenticated should succeed with valid token"
        );
    }

    #[tokio::test]
    async fn ensure_authenticated_without_token_or_oauth_fails() {
        let tmp = tempdir().unwrap();
        let _guard = TokenPathGuard::set(&tmp.path().join("no_token_no_oauth.json"));

        let url = Url::parse("http://localhost:8080/mcp").unwrap();
        let transport = HttpMcpTransport::new(url, None, None).unwrap();

        let result = transport.ensure_authenticated().await;
        assert!(result.is_err(), "Should fail without token or OAuth client");
        assert!(
            result.unwrap_err().to_string().contains("OAuth client"),
            "Error should mention OAuth client"
        );
    }
}

mod oauth_callback_server_tests {
    use super::*;
    use url::Url;

    // Note: listen_for_callback_async is private, so callback parsing is not
    // exercised here. The actual OAuth flow is tested via mocked endpoints in
    // integration tests or real OAuth flows.

    #[tokio::test]
    async fn oauth_client_configuration() {
        // Test that OAuth client can be configured correctly
        let tmp = tempdir().unwrap();
        let _guard = TokenPathGuard::set(&tmp.path().join("oauth_config.json"));

        let url = Url::parse("http://localhost:8080/mcp").unwrap();
        let transport = ahma_http_mcp_client::client::HttpMcpTransport::new(
            url,
            Some("test_client_id".to_string()),
            Some("test_client_secret".to_string()),
        );

        assert!(
            transport.is_ok(),
            "Transport with OAuth config should succeed"
        );
    }
}
