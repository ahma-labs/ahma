//! Policy-enforced HTTP egress client.
//!
//! [`EgressClient`] wraps a `reqwest::Client` together with an [`EgressPolicy`],
//! so every outbound HTTP request is automatically checked against the vault's
//! allowlist **before** the network call is made.
//!
//! This prevents code from bypassing the egress policy by creating a raw
//! `reqwest::Client::new()` directly.
//!
//! # Example
//!
//! ```rust,no_run
//! use ahma_vault::{EgressClient, EgressPolicy};
//! use serde_json::Value;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let policy = EgressPolicy::from_vault_path(std::path::Path::new("/my/vault"));
//! let client = EgressClient::new(policy);
//!
//! // Checked against the policy before making the request.
//! let response: Value = client.get_json("http://localhost:11434/v1/models").await?;
//! # Ok(())
//! # }
//! ```

use anyhow::{Context, Result};
use reqwest::Client;
use serde::de::DeserializeOwned;
use tracing::debug;

use crate::egress::EgressPolicy;

/// Error returned when egress is blocked by policy.
#[derive(Debug, thiserror::Error)]
pub enum EgressError {
    /// The target host is not on the vault's allowlist.
    #[error("Egress denied: host '{0}' is not on the vault allowlist")]
    Denied(String),
    /// An HTTP or serialisation error from reqwest.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
}

/// Policy-enforced HTTP client.
///
/// All requests go through [`EgressPolicy::allows`] before hitting the network.
/// Use this type anywhere you would otherwise use `reqwest::Client` to ensure
/// vault egress rules are respected.
#[derive(Debug, Clone)]
pub struct EgressClient {
    http: Client,
    policy: EgressPolicy,
}

impl EgressClient {
    /// Create a new `EgressClient` with the given policy and a default
    /// `reqwest::Client`.
    pub fn new(policy: EgressPolicy) -> Self {
        Self {
            http: Client::new(),
            policy,
        }
    }

    /// Create a new `EgressClient` with the given policy and a custom
    /// `reqwest::Client` (useful for setting timeouts, TLS settings, etc.).
    pub fn with_client(policy: EgressPolicy, client: Client) -> Self {
        Self {
            http: client,
            policy,
        }
    }

    /// Return a reference to the underlying egress policy.
    pub fn policy(&self) -> &EgressPolicy {
        &self.policy
    }

    /// Check `url`'s host against the egress policy.
    ///
    /// Returns `Ok(())` if allowed, or `Err(EgressError::Denied)` if not.
    fn check_host(&self, url: &str) -> Result<(), EgressError> {
        let host = extract_host(url);
        if self.policy.allows(&host) {
            debug!("Egress allowed: {host} (matched policy)");
            Ok(())
        } else {
            Err(EgressError::Denied(host))
        }
    }

    /// Perform a checked `GET` request and return the raw response bytes.
    pub async fn get(&self, url: &str) -> Result<reqwest::Response, EgressError> {
        self.check_host(url)?;
        Ok(self.http.get(url).send().await?)
    }

    /// Perform a checked `GET` request and deserialise the JSON response.
    pub async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        self.check_host(url)
            .with_context(|| format!("Egress policy denied GET {url}"))?;
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET {url} failed"))?;
        let value = resp
            .json()
            .await
            .with_context(|| format!("Failed to parse JSON from {url}"))?;
        Ok(value)
    }

    /// Perform a checked `POST` request with a JSON body and return the raw response.
    pub async fn post_json(
        &self,
        url: &str,
        body: &impl serde::Serialize,
    ) -> Result<reqwest::Response, EgressError> {
        self.check_host(url)?;
        Ok(self.http.post(url).json(body).send().await?)
    }
}

/// Extract the hostname from a URL string.
///
/// Parses the authority component (`host[:port]`) and strips any port.
/// Handles IPv6 bracket notation (`[::1]` and `[::1]:8080`).
/// Falls back to the whole URL if parsing fails (policy will reject it).
fn extract_host(url: &str) -> String {
    // Fast path for common `http://host/path` and `https://host/path` forms.
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);

    // The authority ends at the first `/`, `?`, or `#`.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);

    // IPv6 literal: authority is `[addr]` or `[addr]:port`.
    if let Some(bracket_end) = authority.strip_prefix('[') {
        // Find the closing `]` and return the address without brackets.
        return bracket_end
            .split_once(']')
            .map(|(addr, _)| addr.to_string())
            .unwrap_or_else(|| authority.to_string());
    }

    // IPv4 / hostname: strip optional `:port` suffix.
    if let Some(host) = authority.rsplit_once(':').map(|(h, _)| h) {
        host.to_string()
    } else {
        authority.to_string()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_host ──────────────────────────────────────────────────────────

    #[test]
    fn extract_host_http() {
        assert_eq!(
            extract_host("http://localhost:11434/v1/models"),
            "localhost"
        );
    }

    #[test]
    fn extract_host_https_with_path() {
        assert_eq!(
            extract_host("https://api.openai.com/v1/chat/completions"),
            "api.openai.com"
        );
    }

    #[test]
    fn extract_host_no_port() {
        assert_eq!(
            extract_host("http://gpu-node.internal/"),
            "gpu-node.internal"
        );
    }

    #[test]
    fn extract_host_ip_with_port() {
        assert_eq!(extract_host("http://10.0.0.1:4000/tasks"), "10.0.0.1");
    }

    #[test]
    fn extract_host_ipv6_with_port() {
        assert_eq!(extract_host("http://[::1]:8080/api"), "::1");
    }

    #[test]
    fn extract_host_ipv6_no_port() {
        assert_eq!(extract_host("http://[::1]/api"), "::1");
    }

    // ── policy enforcement ────────────────────────────────────────────────────

    #[test]
    fn loopback_always_allowed() {
        let client = EgressClient::new(EgressPolicy::loopback_only());
        assert!(client.check_host("http://localhost:11434/v1").is_ok());
        assert!(client.check_host("http://127.0.0.1:8080/api").is_ok());
    }

    #[test]
    fn external_host_blocked_by_default() {
        let client = EgressClient::new(EgressPolicy::loopback_only());
        let err = client
            .check_host("https://api.openai.com/v1")
            .expect_err("external host should be blocked");
        assert!(
            matches!(err, EgressError::Denied(ref h) if h == "api.openai.com"),
            "error must be Denied(api.openai.com), got: {err:?}"
        );
    }

    #[test]
    fn allow_all_permits_external() {
        let client = EgressClient::new(EgressPolicy::allow_all());
        assert!(client.check_host("https://api.openai.com/v1").is_ok());
        assert!(client.check_host("http://evil.example.com/exfil").is_ok());
    }

    #[test]
    fn denied_error_contains_host_in_message() {
        let client = EgressClient::new(EgressPolicy::loopback_only());
        let err = client.check_host("https://api.openai.com/v1").unwrap_err();
        assert!(
            err.to_string().contains("api.openai.com"),
            "error message must include the denied host: {err}"
        );
    }

    // ── coverage batch: with_client / policy / get / get_json / post_json ──────
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::mpsc::Receiver;
    use std::time::Duration;

    fn http_200(content_type: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
            ct = content_type,
            len = body.len(),
            body = body,
        )
    }

    fn spawn_oneshot_http_server(response: String) -> (u16, Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                let mut request: Vec<u8> = Vec::new();
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            request.extend_from_slice(&buf[..n]);
                            let s = String::from_utf8_lossy(&request);
                            if let Some(hdr_end) = s.find("\r\n\r\n") {
                                let headers = &s[..hdr_end];
                                let content_len = headers
                                    .lines()
                                    .find_map(|l| {
                                        let l = l.to_ascii_lowercase();
                                        l.strip_prefix("content-length:")
                                            .map(|v| v.trim().parse::<usize>().ok())
                                    })
                                    .flatten()
                                    .unwrap_or(0);
                                let body_start = hdr_end + 4;
                                if request.len() >= body_start + content_len {
                                    break;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&request).to_string());
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Write);
            }
        });
        (port, rx)
    }

    fn timeout_client() -> Client {
        Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("build reqwest client")
    }

    #[test]
    fn with_client_preserves_policy_and_custom_client() {
        let client = EgressClient::with_client(EgressPolicy::allow_all(), timeout_client());
        assert!(client.policy().allows("api.openai.com"));
        assert!(client.check_host("https://api.openai.com/v1").is_ok());
    }

    #[test]
    fn policy_accessor_returns_configured_policy() {
        let client = EgressClient::new(EgressPolicy::loopback_only());
        let policy = client.policy();
        assert!(policy.allows("127.0.0.1"));
        assert!(!policy.allows("api.openai.com"));
    }

    #[tokio::test]
    async fn get_denied_returns_denied_error_without_network() {
        let client = EgressClient::new(EgressPolicy::loopback_only());
        let err = client
            .get("https://api.openai.com/v1/models")
            .await
            .expect_err("external host must be denied");
        assert!(
            matches!(err, EgressError::Denied(ref h) if h == "api.openai.com"),
            "expected Denied(api.openai.com), got {err:?}"
        );
    }

    #[tokio::test]
    async fn get_success_returns_200_from_local_server() {
        let (port, _rx) = spawn_oneshot_http_server(http_200("text/plain", "hello"));
        let client = EgressClient::with_client(EgressPolicy::loopback_only(), timeout_client());
        let url = format!("http://127.0.0.1:{port}/ping");
        let resp = client.get(&url).await.expect("loopback GET should succeed");
        assert_eq!(resp.status().as_u16(), 200);
        let body = resp.text().await.expect("read body");
        assert_eq!(body, "hello");
    }

    #[tokio::test]
    async fn get_json_denied_message_mentions_egress_policy() {
        let client = EgressClient::new(EgressPolicy::loopback_only());
        let err = client
            .get_json::<serde_json::Value>("https://api.openai.com/v1/models")
            .await
            .expect_err("external host must be denied");
        assert!(
            err.to_string().contains("Egress policy denied"),
            "error should mention denial context, got: {err}"
        );
    }

    #[tokio::test]
    async fn get_json_success_deserializes_struct() {
        #[derive(serde::Deserialize, Debug)]
        struct Models {
            object: String,
            count: u32,
        }

        let body = r#"{"object":"list","count":3}"#;
        let (port, _rx) = spawn_oneshot_http_server(http_200("application/json", body));
        let client = EgressClient::with_client(EgressPolicy::loopback_only(), timeout_client());
        let url = format!("http://127.0.0.1:{port}/v1/models");
        let models: Models = client.get_json(&url).await.expect("JSON should parse");
        assert_eq!(models.object, "list");
        assert_eq!(models.count, 3);
    }

    #[tokio::test]
    async fn get_json_parse_failure_returns_context_error() {
        let (port, _rx) =
            spawn_oneshot_http_server(http_200("text/plain", "this is definitely not json"));
        let client = EgressClient::with_client(EgressPolicy::loopback_only(), timeout_client());
        let url = format!("http://127.0.0.1:{port}/bad");
        let err = client
            .get_json::<serde_json::Value>(&url)
            .await
            .expect_err("non-JSON body must fail to parse");
        assert!(
            err.to_string().contains("Failed to parse JSON"),
            "error should carry parse context, got: {err}"
        );
    }

    #[tokio::test]
    async fn post_json_denied_returns_denied_error() {
        let client = EgressClient::new(EgressPolicy::loopback_only());
        let payload = serde_json::json!({ "model": "gpt" });
        let err = client
            .post_json("https://api.openai.com/v1/chat", &payload)
            .await
            .expect_err("external host must be denied");
        assert!(
            matches!(err, EgressError::Denied(ref h) if h == "api.openai.com"),
            "expected Denied(api.openai.com), got {err:?}"
        );
    }

    #[tokio::test]
    async fn post_json_success_sends_body_and_returns_200() {
        let (port, rx) = spawn_oneshot_http_server(http_200("application/json", r#"{"ok":true}"#));
        let client = EgressClient::with_client(EgressPolicy::loopback_only(), timeout_client());
        let url = format!("http://127.0.0.1:{port}/v1/chat");
        let payload = serde_json::json!({ "greeting": "hello" });
        let resp = client
            .post_json(&url, &payload)
            .await
            .expect("loopback POST should succeed");
        assert_eq!(resp.status().as_u16(), 200);

        let raw_request = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("server should have captured the request");
        assert!(
            raw_request.starts_with("POST "),
            "expected a POST request line, got: {raw_request}"
        );
        assert!(
            raw_request.contains("hello"),
            "server should have received the posted JSON body, got: {raw_request}"
        );
    }
}
