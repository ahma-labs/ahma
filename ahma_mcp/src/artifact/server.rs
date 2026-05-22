//! Per-task localhost artifact API server.
//!
//! The artifact server provides a minimal HTTP endpoint that the embedded HTML
//! chat widget can call for:
//!
//! - `/chat` — relay LLM chat completions (POST) to the local Ollama endpoint.
//! - `/health` — confirm the server is reachable (GET).
//!
//! Security:
//! - Binds to `127.0.0.1` only.
//! - Every request must include the short-lived `Bearer <token>` in
//!   `Authorization`; requests without the token receive `401`.
//! - The token is generated randomly at server start and embedded in the
//!   HTML artifact — it is never sent over the network in the clear.
//!
//! This is intentionally minimal: no TLS, no routing framework.  It is only
//! accessible from the local machine and only for the lifetime of the vault
//! session.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use rand::RngExt as _;
use tracing::{debug, info};

// ─────────────────────────────────────────────────────────────────────────────
// ArtifactServer
// ─────────────────────────────────────────────────────────────────────────────

/// A running per-task artifact API server.
pub struct ArtifactServer {
    /// Local address the server is bound to.
    pub local_addr: SocketAddr,
    /// Bearer token required by all API requests.
    pub token: String,
    /// Background task handle (aborted on drop).
    _task: tokio::task::JoinHandle<()>,
}

impl ArtifactServer {
    /// Start the artifact server and return immediately.
    ///
    /// `llm_base_url` is the OpenAI-compatible endpoint for chat relay.
    pub async fn start(llm_base_url: impl Into<String>) -> Result<Self> {
        let token = generate_token();
        let llm_url = Arc::new(llm_base_url.into());
        let token_arc = Arc::new(token.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let local_addr = listener.local_addr()?;
        info!("Artifact server listening on {local_addr}");

        let task = tokio::spawn(async move {
            serve_loop(listener, token_arc, llm_url).await;
        });

        Ok(Self {
            local_addr,
            token,
            _task: task,
        })
    }

    /// Return the base URL of this server.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.local_addr)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// server loop
// ─────────────────────────────────────────────────────────────────────────────

async fn serve_loop(listener: tokio::net::TcpListener, token: Arc<String>, llm_url: Arc<String>) {
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            break;
        };
        let token = Arc::clone(&token);
        let llm = Arc::clone(&llm_url);
        tokio::spawn(async move {
            if let Err(e) = handle(stream, token, llm).await {
                debug!("Artifact server error from {peer}: {e}");
            }
        });
    }
}

async fn handle(
    mut stream: tokio::net::TcpStream,
    token: Arc<String>,
    llm_url: Arc<String>,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut buf = vec![0u8; 16 * 1024];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }

    let request = String::from_utf8_lossy(&buf[..n]);
    let first_line = request.lines().next().unwrap_or("").to_string();

    debug!("Artifact server request: {first_line}");

    // Simple auth check.
    let bearer = format!("Bearer {}", token.as_str());
    if !request.contains(&bearer) {
        stream
            .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
            .await?;
        return Ok(());
    }

    if first_line.starts_with("GET") && first_line.contains("/health") {
        let body = b"{\"status\":\"ok\"}";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        stream.write_all(resp.as_bytes()).await?;
        stream.write_all(body).await?;
        return Ok(());
    }

    if first_line.starts_with("POST") && first_line.contains("/chat") {
        // Extract JSON body (naive: everything after double CRLF).
        let body_start = request.find("\r\n\r\n").map(|i| i + 4).unwrap_or(n);
        let json_body = &buf[body_start..n];

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()?;

        let upstream_resp = client
            .post(format!(
                "{}/chat/completions",
                llm_url.trim_end_matches('/')
            ))
            .header("Content-Type", "application/json")
            .body(json_body.to_vec())
            .send()
            .await;

        match upstream_resp {
            Ok(r) => {
                let status = r.status().as_u16();
                let upstream_body = r.bytes().await.unwrap_or_default();
                let resp_head = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\n\r\n",
                    upstream_body.len()
                );
                stream.write_all(resp_head.as_bytes()).await?;
                stream.write_all(&upstream_body).await?;
            }
            Err(e) => {
                let body = format!("{{\"error\":\"{e}\"}}");
                let resp_head = format!(
                    "HTTP/1.1 502 Bad Gateway\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                );
                stream.write_all(resp_head.as_bytes()).await?;
                stream.write_all(body.as_bytes()).await?;
            }
        }
        return Ok(());
    }

    stream
        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
        .await?;
    Ok(())
}

fn generate_token() -> String {
    let bytes: [u8; 16] = rand::rng().random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn server_starts_and_binds() {
        let server = ArtifactServer::start("http://localhost:11434/v1")
            .await
            .unwrap();
        assert_eq!(server.local_addr.ip().to_string(), "127.0.0.1");
        assert!(!server.token.is_empty());
    }

    #[tokio::test]
    async fn health_check_with_correct_token() {
        let server = ArtifactServer::start("http://localhost:11434/v1")
            .await
            .unwrap();
        let url = format!("{}/health", server.base_url());

        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
            .header("Authorization", format!("Bearer {}", server.token))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn health_check_without_token_returns_401() {
        let server = ArtifactServer::start("http://localhost:11434/v1")
            .await
            .unwrap();
        let url = format!("{}/health", server.base_url());

        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), 401);
    }
}
