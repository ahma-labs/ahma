//! One HTTP client for ahma's MCP endpoints, over TCP or the local socket.
//!
//! The per-user hub serves MCP on an `AF_UNIX` socket on every OS (SPEC
//! R-HUB.2), but `reqwest` reaches such a socket only on Unix: on Windows it
//! offers named pipes instead, and its connector cannot be replaced. So
//! [`HttpClient`](crate::http_client::HttpClient) builds every request with
//! `reqwest` as before and chooses how to *send* it: through `reqwest` over
//! TCP, or through hyper over a
//! [`LocalStream`](ahma_common::local_socket::LocalStream). Either way the
//! caller gets back an ordinary [`reqwest::Response`], streaming body
//! included, so the code above it — the handshake, the SSE listener, the retry
//! rules — is the same for both.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use ahma_common::http_retry::{Failure, TransportError, classify_transport};
use ahma_common::local_socket::LocalStream;
use http::HeaderValue;
use http::header::HOST;
use hyper_util::rt::TokioIo;

/// The `unix://` URL scheme ahma uses for a local socket, on every OS.
pub const LOCAL_SOCKET_SCHEME: &str = "unix://";

/// The base URL requests over a local socket address. Only its path reaches
/// the server; the host is what the `Host` header says.
pub const LOCAL_SOCKET_REQUEST_BASE: &str = "http://localhost";

/// How to reach an HTTP endpoint: over the network, or over a local socket.
#[derive(Clone, Debug)]
pub enum HttpClient {
    /// HTTP over TCP (or TLS, or QUIC) through `reqwest`.
    Tcp(reqwest::Client),
    /// HTTP/1.1 over an `AF_UNIX` socket, on every OS.
    LocalSocket(LocalSocketSender),
}

impl From<reqwest::Client> for HttpClient {
    fn from(client: reqwest::Client) -> Self {
        Self::Tcp(client)
    }
}

/// Sends requests over a local socket, one connection per request.
///
/// No pooling: the endpoint is local, a connect is cheap, and a fresh connect
/// is what lets a respawned hub be picked up by the very next request.
#[derive(Clone, Debug)]
pub struct LocalSocketSender {
    path: Arc<PathBuf>,
    timeout: Option<Duration>,
}

/// Why a request could not be sent, or got no answer.
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    /// `reqwest` failed: over TCP, or while building the request.
    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
    /// Nothing accepted the connection, so the request never arrived.
    #[error("couldn't connect to the local socket {path}: {source}")]
    Connect {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// The connection failed after the request may have been sent.
    #[error("the connection to the local socket {path} failed: {source}")]
    Exchange {
        path: String,
        #[source]
        source: hyper::Error,
    },
    /// No response arrived in time.
    #[error("no response from the local socket {path} within {timeout:?}")]
    Timeout { path: String, timeout: Duration },
}

impl SendError {
    /// Whether the endpoint is gone, rather than slow or failing: nothing was
    /// listening at the socket.
    pub fn is_connect(&self) -> bool {
        match self {
            Self::Reqwest(e) => e.is_connect(),
            Self::Connect { .. } => true,
            Self::Exchange { .. } | Self::Timeout { .. } => false,
        }
    }
}

impl TransportError for SendError {
    fn is_timeout(&self) -> bool {
        match self {
            Self::Reqwest(e) => e.is_timeout(),
            Self::Timeout { .. } => true,
            Self::Connect { .. } | Self::Exchange { .. } => false,
        }
    }
}

/// What a [`SendError`] means for retrying (SPEC R-HTTP.2): a failed connect
/// never delivered the request; anything later may have.
pub fn classify_send_error(error: &SendError) -> Failure {
    match error {
        SendError::Reqwest(e) => classify_transport(e),
        SendError::Connect { .. } => Failure::NotDelivered,
        SendError::Exchange { .. } | SendError::Timeout { .. } => Failure::Interrupted,
    }
}

/// Builds requests for the local-socket path. It never sends one, so its
/// configuration does not matter; it exists because a `reqwest` request is
/// only built through a client.
static REQUEST_BUILDER: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

impl HttpClient {
    /// A client for the local socket at `path`. On Linux an `@name` path is
    /// the abstract socket `name`.
    ///
    /// `timeout` bounds each request from connect to response headers; a
    /// request's own [`reqwest::RequestBuilder::timeout`] overrides it. `None`
    /// waits as long as it takes, which a long-lived SSE stream needs.
    pub fn local_socket(path: impl AsRef<Path>, timeout: Option<Duration>) -> Self {
        let path = path.as_ref();
        let path = match path.to_str().and_then(|p| p.strip_prefix('@')) {
            // tokio treats a leading NUL as the abstract namespace on Linux.
            Some(name) => PathBuf::from(format!("\0{name}")),
            None => path.to_path_buf(),
        };
        Self::LocalSocket(LocalSocketSender {
            path: Arc::new(path),
            timeout,
        })
    }

    /// The client for `base_url`, and the base URL to address requests to.
    ///
    /// `unix://<path>` is the local socket at `<path>`, addressed as
    /// [`LOCAL_SOCKET_REQUEST_BASE`]; anything else is used as it is, over
    /// `reqwest` with `timeout`.
    pub fn for_base_url(
        base_url: &str,
        timeout: Option<Duration>,
    ) -> Result<(String, Self), reqwest::Error> {
        if let Some(path) = base_url.strip_prefix(LOCAL_SOCKET_SCHEME) {
            let path = path.split_once('#').map_or(path, |(path, _)| path);
            return Ok((
                LOCAL_SOCKET_REQUEST_BASE.to_string(),
                Self::local_socket(path, timeout),
            ));
        }
        let mut builder = reqwest::Client::builder();
        if let Some(timeout) = timeout {
            builder = builder.timeout(timeout);
        }
        Ok((base_url.to_string(), Self::Tcp(builder.build()?)))
    }

    fn builder_client(&self) -> &reqwest::Client {
        match self {
            Self::Tcp(client) => client,
            Self::LocalSocket(_) => &REQUEST_BUILDER,
        }
    }

    /// Start a `GET` of `url`, to send with [`Self::send`].
    pub fn get(&self, url: &str) -> reqwest::RequestBuilder {
        self.builder_client().get(url)
    }

    /// Start a `POST` to `url`, to send with [`Self::send`].
    pub fn post(&self, url: &str) -> reqwest::RequestBuilder {
        self.builder_client().post(url)
    }

    /// Start a `DELETE` of `url`, to send with [`Self::send`].
    pub fn delete(&self, url: &str) -> reqwest::RequestBuilder {
        self.builder_client().delete(url)
    }

    /// Send a request made by [`Self::get`], [`Self::post`] or
    /// [`Self::delete`].
    pub async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, SendError> {
        match self {
            Self::Tcp(_) => Ok(request.send().await?),
            Self::LocalSocket(sender) => sender.send(request.build()?).await,
        }
    }
}

impl LocalSocketSender {
    fn display_path(&self) -> String {
        self.path.to_string_lossy().replace('\0', "@")
    }

    async fn send(&self, request: reqwest::Request) -> Result<reqwest::Response, SendError> {
        let Some(timeout) = request.timeout().copied().or(self.timeout) else {
            return self.exchange(request).await;
        };
        tokio::time::timeout(timeout, self.exchange(request))
            .await
            .map_err(|_| SendError::Timeout {
                path: self.display_path(),
                timeout,
            })?
    }

    /// Connect, send `request` and return the response once its headers are
    /// in. The body streams from the connection as the caller reads it.
    async fn exchange(&self, request: reqwest::Request) -> Result<reqwest::Response, SendError> {
        let mut request: http::Request<reqwest::Body> = request.try_into()?;
        // Over TCP reqwest writes the Host header and an origin-form target
        // itself; hyper's bare connection does neither.
        if !request.headers().contains_key(HOST)
            && let Some(authority) = request.uri().authority()
            && let Ok(host) = HeaderValue::from_str(authority.as_str())
        {
            request.headers_mut().insert(HOST, host);
        }
        if let Some(target) = request.uri().path_and_query().cloned() {
            *request.uri_mut() = target.into();
        }

        let stream =
            LocalStream::connect(&self.path)
                .await
                .map_err(|source| SendError::Connect {
                    path: self.display_path(),
                    source,
                })?;
        let exchange_error = |source| SendError::Exchange {
            path: self.display_path(),
            source,
        };
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(exchange_error)?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::debug!("local socket HTTP/1.1 connection ended: {e}");
            }
        });
        let response = sender.send_request(request).await.map_err(exchange_error)?;
        Ok(response.map(reqwest::Body::wrap).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::serve_on_local_socket;
    use ahma_common::local_socket::LocalListener;

    fn echo_app() -> axum::Router {
        axum::Router::new().route(
            "/mcp",
            axum::routing::post(|headers: axum::http::HeaderMap, body: String| async move {
                let host = headers
                    .get("host")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                ([("x-host", host)], body)
            }),
        )
    }

    /// The TUI and the agent reach the hub through this client, so it must
    /// work where the hub listens: an `AF_UNIX` socket on every OS, Windows
    /// included, where `reqwest` cannot connect to one (SPEC R-HUB.2).
    #[tokio::test]
    async fn a_request_is_sent_over_the_local_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mcp.sock");
        serve_on_local_socket(LocalListener::bind(&sock).unwrap(), echo_app());

        let client = HttpClient::local_socket(&sock, None);
        let response = client
            .send(
                client
                    .post("http://localhost/mcp")
                    .json(&serde_json::json!({"hello": "socket"})),
            )
            .await
            .expect("the request is answered");
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers().get("x-host").unwrap(),
            "localhost",
            "the Host header comes from the URL, as over TCP"
        );
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body, serde_json::json!({"hello": "socket"}));
    }

    /// A socket nobody listens on never received the request, so it is safe
    /// to retry, and it is what tells a caller the endpoint is gone.
    #[tokio::test]
    async fn a_missing_socket_is_a_connect_failure() {
        let dir = tempfile::tempdir().unwrap();
        let client = HttpClient::local_socket(dir.path().join("absent.sock"), None);
        let err = client
            .send(client.post("http://localhost/mcp"))
            .await
            .expect_err("nothing listens there");
        assert!(err.is_connect(), "{err}");
        assert_eq!(classify_send_error(&err), Failure::NotDelivered);
        assert!(err.to_string().contains("absent.sock"), "{err}");
    }

    /// A reply that does not come in time is a timeout, which the retry
    /// policy may make final, not a hang.
    #[tokio::test]
    async fn a_silent_endpoint_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("slow.sock");
        let app = axum::Router::new().route(
            "/mcp",
            axum::routing::post(std::future::pending::<&'static str>),
        );
        serve_on_local_socket(LocalListener::bind(&sock).unwrap(), app);

        let client = HttpClient::local_socket(&sock, Some(Duration::from_millis(200)));
        let err = client
            .send(client.post("http://localhost/mcp"))
            .await
            .expect_err("the endpoint never answers");
        assert!(err.is_timeout(), "{err}");
        assert_eq!(classify_send_error(&err), Failure::Interrupted);
    }

    /// The response body streams: the first SSE event arrives while the
    /// server is still holding the stream open. The MCP handshake depends on
    /// it — `roots/list` comes down a stream that never ends.
    #[tokio::test]
    async fn a_response_body_streams_before_it_ends() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("sse.sock");
        let app = axum::Router::new().route(
            "/mcp",
            axum::routing::get(|| async {
                use futures::StreamExt;
                let first = futures::stream::iter([Ok::<_, std::convert::Infallible>(
                    bytes::Bytes::from_static(b"data: 1\n\n"),
                )]);
                let body = axum::body::Body::from_stream(first.chain(futures::stream::pending()));
                ([("content-type", "text/event-stream")], body)
            }),
        );
        serve_on_local_socket(LocalListener::bind(&sock).unwrap(), app);

        let client = HttpClient::local_socket(&sock, None);
        let mut response = client
            .send(client.get("http://localhost/mcp"))
            .await
            .expect("the stream opens");
        let chunk = tokio::time::timeout(
            ahma_common::timeouts::TestTimeouts::get(ahma_common::timeouts::TimeoutCategory::Quick),
            response.chunk(),
        )
        .await
        .expect("the first event arrives while the stream is still open")
        .unwrap()
        .unwrap();
        assert_eq!(&chunk[..], b"data: 1\n\n");
    }

    #[test]
    fn a_unix_url_means_the_local_socket() {
        let (base, client) =
            HttpClient::for_base_url("unix:///run/ahma/mcp.sock#/mcp", None).unwrap();
        assert_eq!(base, LOCAL_SOCKET_REQUEST_BASE);
        match client {
            HttpClient::LocalSocket(sender) => {
                assert_eq!(*sender.path, PathBuf::from("/run/ahma/mcp.sock"));
            }
            HttpClient::Tcp(_) => panic!("unix:// must not go over TCP"),
        }

        let (base, client) = HttpClient::for_base_url("http://127.0.0.1:3000", None).unwrap();
        assert_eq!(base, "http://127.0.0.1:3000");
        assert!(matches!(client, HttpClient::Tcp(_)));
    }

    #[test]
    fn an_at_path_is_the_abstract_socket() {
        let HttpClient::LocalSocket(sender) = HttpClient::local_socket("@ahma-test", None) else {
            panic!("a local socket");
        };
        assert_eq!(*sender.path, PathBuf::from("\0ahma-test"));
        assert_eq!(sender.display_path(), "@ahma-test");
    }
}
