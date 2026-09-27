//! MCP Streamable HTTP over the per-user local socket, on every OS.
//!
//! The per-user daemon serves its MCP endpoint on an `AF_UNIX` socket on every
//! OS (SPEC R-DAEMON.2), and the stdio proxy an editor spawns reaches it through
//! [`LocalSocketHttpClient`](crate::local_socket_client::LocalSocketHttpClient). rmcp ships an equivalent, `UnixSocketHttpClient`,
//! but it is built on `tokio::net::UnixStream` and so does not exist on Windows,
//! where tokio cannot register `AF_UNIX` sockets. This is that client with the
//! one Unix-specific call — the connect — replaced by
//! [`ahma_common::local_socket::LocalStream`], which works on both. The HTTP
//! handling is rmcp's (2.2.0, `transport/common/unix_socket.rs`, MIT), kept
//! close to the original so a future rmcp fix is easy to carry across.
//!
//! One connection per request, no pooling: the endpoint is local, a connect is
//! cheap, and it is what lets a respawned daemon be picked up by the very next
//! request.

use std::{borrow::Cow, collections::HashMap, path::PathBuf, sync::Arc};

use ahma_common::local_socket::LocalStream;
use bytes::Bytes;
use futures::{StreamExt, stream::BoxStream};
use http::{HeaderName, HeaderValue, Method, Request, StatusCode, header::WWW_AUTHENTICATE};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use rmcp::{
    model::{ClientJsonRpcMessage, ServerJsonRpcMessage},
    transport::{
        StreamableHttpClientTransport,
        common::http_header::{
            EVENT_STREAM_MIME_TYPE, HEADER_LAST_EVENT_ID, HEADER_MCP_PROTOCOL_VERSION,
            HEADER_SESSION_ID, JSON_MIME_TYPE,
        },
        streamable_http_client::{
            AuthRequiredError, InsufficientScopeError, StreamableHttpClient,
            StreamableHttpClientTransportConfig, StreamableHttpError, StreamableHttpPostResponse,
        },
    },
};
use sse_stream::{Sse, SseStream};

/// Why a request over the local socket failed before the server could answer.
#[derive(Debug, thiserror::Error)]
pub enum LocalSocketError {
    #[error("hyper error: {0}")]
    Hyper(#[from] hyper::Error),
    /// The connect or the write failed. Its kind is what tells a proxy the
    /// endpoint is gone (`NotFound`, `ConnectionRefused`) rather than busy.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP error: {0}")]
    Http(#[from] http::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<LocalSocketError> for StreamableHttpError<LocalSocketError> {
    fn from(e: LocalSocketError) -> Self {
        StreamableHttpError::Client(e)
    }
}

/// A [`StreamableHttpClient`] that sends each request over a fresh connection
/// to a local `AF_UNIX` socket.
#[derive(Clone, Debug)]
pub struct LocalSocketHttpClient {
    socket_path: Arc<PathBuf>,
    host_header: HeaderValue,
}

impl LocalSocketHttpClient {
    /// A client for the socket at `socket_path`.
    ///
    /// `uri` is the MCP URI the requests address; only its authority is used,
    /// for the `Host` header hyper does not set on a socket connection. On
    /// Linux an `@name` path is the abstract socket `name`.
    ///
    /// # Panics
    ///
    /// Panics if `socket_path` is empty or a bare `@`.
    pub fn new(socket_path: &str, uri: &str) -> Self {
        assert!(
            !socket_path.is_empty() && socket_path != "@",
            "socket_path must not be empty or a bare '@' (empty abstract socket name)"
        );
        let host_header = uri
            .parse::<http::Uri>()
            .ok()
            .and_then(|u| u.authority().cloned())
            .and_then(|a| HeaderValue::from_str(a.as_str()).ok())
            .unwrap_or_else(|| HeaderValue::from_static("localhost"));
        let socket_path = match socket_path.strip_prefix('@') {
            // tokio treats a leading NUL as the abstract namespace on Linux.
            Some(name) => PathBuf::from(format!("\0{name}")),
            None => PathBuf::from(socket_path),
        };
        Self {
            socket_path: Arc::new(socket_path),
            host_header,
        }
    }
}

/// An MCP client transport that reaches the endpoint at `socket_path`.
///
/// `uri` is the HTTP URI used inside the connection, e.g.
/// `http://localhost/mcp`; its query carries per-session options.
pub fn local_socket_transport(
    socket_path: &str,
    uri: &str,
) -> StreamableHttpClientTransport<LocalSocketHttpClient> {
    let client = LocalSocketHttpClient::new(socket_path, uri);
    let config = StreamableHttpClientTransportConfig::with_uri(uri);
    StreamableHttpClientTransport::with_client(client, config)
}

/// Open a connection and send one request on it.
async fn send_http_request(
    socket_path: &std::path::Path,
    request: Request<Full<Bytes>>,
) -> Result<http::Response<Incoming>, LocalSocketError> {
    let stream = LocalStream::connect(socket_path).await?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::debug!("local socket HTTP/1.1 connection error: {e}");
        }
    });
    Ok(sender.send_request(request).await?)
}

/// Headers a caller may not override, because the transport owns them.
/// `MCP-Protocol-Version` is on rmcp's list but let through: its worker
/// injects it after `initialize`.
const RESERVED_HEADERS: &[&str] = &["accept", HEADER_SESSION_ID, HEADER_LAST_EVENT_ID];

/// Add `custom_headers` to `builder`, refusing any the transport owns.
fn apply_custom_headers(
    mut builder: http::request::Builder,
    custom_headers: HashMap<HeaderName, HeaderValue>,
) -> Result<http::request::Builder, StreamableHttpError<LocalSocketError>> {
    for (name, value) in custom_headers {
        let reserved = RESERVED_HEADERS
            .iter()
            .any(|r| name.as_str().eq_ignore_ascii_case(r))
            && !name
                .as_str()
                .eq_ignore_ascii_case(HEADER_MCP_PROTOCOL_VERSION);
        if reserved {
            return Err(StreamableHttpError::ReservedHeaderConflict(
                name.to_string(),
            ));
        }
        builder = builder.header(name, value);
    }
    Ok(builder)
}

/// The `scope=` parameter of a `WWW-Authenticate` value, quoted or not.
fn extract_scope_from_header(header: &str) -> Option<String> {
    let pos = header.to_ascii_lowercase().find("scope=")?;
    let value = &header[pos + "scope=".len()..];
    if let Some(quoted) = value.strip_prefix('"') {
        return quoted.find('"').map(|end| quoted[..end].to_string());
    }
    let end = value
        .find(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .unwrap_or(value.len());
    (end > 0).then(|| value[..end].to_string())
}

/// The `WWW-Authenticate` value of a 401 or 403, as the error rmcp expects.
fn auth_error(
    response: &http::Response<Incoming>,
) -> Result<Option<StreamableHttpError<LocalSocketError>>, StreamableHttpError<LocalSocketError>> {
    let status = response.status();
    if status != StatusCode::UNAUTHORIZED && status != StatusCode::FORBIDDEN {
        return Ok(None);
    }
    let Some(header) = response.headers().get(WWW_AUTHENTICATE) else {
        return Ok(None);
    };
    let header = header.to_str().map_err(|_| {
        StreamableHttpError::UnexpectedServerResponse(Cow::from(
            "invalid www-authenticate header value",
        ))
    })?;
    Ok(Some(if status == StatusCode::UNAUTHORIZED {
        StreamableHttpError::AuthRequired(AuthRequiredError::new(header.to_string()))
    } else {
        StreamableHttpError::InsufficientScope(InsufficientScopeError::new(
            header.to_string(),
            extract_scope_from_header(header),
        ))
    }))
}

impl StreamableHttpClient for LocalSocketHttpClient {
    type Error = LocalSocketError;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let json_body = serde_json::to_string(&message).map_err(LocalSocketError::Json)?;

        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(uri.as_ref())
            .header(http::header::HOST, self.host_header.clone())
            .header(http::header::CONTENT_TYPE, JSON_MIME_TYPE)
            .header(
                http::header::ACCEPT,
                format!("{EVENT_STREAM_MIME_TYPE}, {JSON_MIME_TYPE}"),
            );
        if let Some(auth) = auth_token {
            builder = builder.header(http::header::AUTHORIZATION, format!("Bearer {auth}"));
        }
        builder = apply_custom_headers(builder, custom_headers)?;
        let session_was_attached = session_id.is_some();
        if let Some(sid) = session_id {
            builder = builder.header(HEADER_SESSION_ID, sid.as_ref());
        }
        let request = builder
            .body(Full::new(Bytes::from(json_body)))
            .map_err(LocalSocketError::Http)?;

        let response = send_http_request(&self.socket_path, request).await?;
        if let Some(err) = auth_error(&response)? {
            return Err(err);
        }

        let status = response.status();
        if matches!(status, StatusCode::ACCEPTED | StatusCode::NO_CONTENT) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status == StatusCode::NOT_FOUND && session_was_attached {
            return Err(StreamableHttpError::SessionExpired);
        }
        if !status.is_success() {
            // The whole body, verbatim: the proxy recovers the bridge's
            // JSON-RPC error from it.
            let body = response
                .into_body()
                .collect()
                .await
                .map(|c| String::from_utf8_lossy(&c.to_bytes()).into_owned())
                .unwrap_or_else(|_| "<failed to read response body>".to_owned());
            return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                format!("HTTP {status}: {body}"),
            )));
        }

        let content_type = response.headers().get(http::header::CONTENT_TYPE).cloned();
        let content_length = response
            .headers()
            .get(http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let session_id = response
            .headers()
            .get(HEADER_SESSION_ID)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        if content_length == Some(0)
            && matches!(
                message,
                ClientJsonRpcMessage::Notification(_)
                    | ClientJsonRpcMessage::Response(_)
                    | ClientJsonRpcMessage::Error(_)
            )
        {
            return Ok(StreamableHttpPostResponse::Accepted);
        }

        match content_type {
            Some(ref ct) if ct.as_bytes().starts_with(EVENT_STREAM_MIME_TYPE.as_bytes()) => {
                let sse_stream = SseStream::new(response.into_body()).boxed();
                Ok(StreamableHttpPostResponse::Sse(sse_stream, session_id))
            }
            Some(ref ct) if ct.as_bytes().starts_with(JSON_MIME_TYPE.as_bytes()) => {
                let body = response
                    .into_body()
                    .collect()
                    .await
                    .map_err(LocalSocketError::Hyper)?
                    .to_bytes();
                match serde_json::from_slice::<ServerJsonRpcMessage>(&body) {
                    Ok(message) => Ok(StreamableHttpPostResponse::Json(message, session_id)),
                    Err(e) => {
                        tracing::warn!(
                            "could not parse JSON response as ServerJsonRpcMessage, treating as accepted: {e}"
                        );
                        Ok(StreamableHttpPostResponse::Accepted)
                    }
                }
            }
            _ => Err(StreamableHttpError::UnexpectedContentType(
                content_type.map(|ct| String::from_utf8_lossy(ct.as_bytes()).into_owned()),
            )),
        }
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        let mut builder = Request::builder()
            .method(Method::DELETE)
            .uri(uri.as_ref())
            .header(http::header::HOST, self.host_header.clone())
            .header(HEADER_SESSION_ID, session_id.as_ref());
        if let Some(auth) = auth_token {
            builder = builder.header(http::header::AUTHORIZATION, format!("Bearer {auth}"));
        }
        builder = apply_custom_headers(builder, custom_headers)?;
        let request = builder
            .body(Full::new(Bytes::new()))
            .map_err(LocalSocketError::Http)?;

        let response = send_http_request(&self.socket_path, request).await?;
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            tracing::debug!("this server doesn't support deleting session");
            return Ok(());
        }
        if !response.status().is_success() {
            return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                format!("delete_session returned {}", response.status()),
            )));
        }
        Ok(())
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, sse_stream::Error>>, StreamableHttpError<Self::Error>>
    {
        let mut builder = Request::builder()
            .method(Method::GET)
            .uri(uri.as_ref())
            .header(http::header::HOST, self.host_header.clone())
            .header(
                http::header::ACCEPT,
                format!("{EVENT_STREAM_MIME_TYPE}, {JSON_MIME_TYPE}"),
            )
            .header(HEADER_SESSION_ID, session_id.as_ref());
        if let Some(last_id) = last_event_id {
            builder = builder.header(HEADER_LAST_EVENT_ID, last_id);
        }
        if let Some(auth) = auth_token {
            builder = builder.header(http::header::AUTHORIZATION, format!("Bearer {auth}"));
        }
        builder = apply_custom_headers(builder, custom_headers)?;
        let request = builder
            .body(Full::new(Bytes::new()))
            .map_err(LocalSocketError::Http)?;

        let response = send_http_request(&self.socket_path, request).await?;
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        if let Some(err) = auth_error(&response)? {
            return Err(err);
        }
        if !response.status().is_success() {
            return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                format!("get_stream returned {}", response.status()),
            )));
        }
        match response.headers().get(http::header::CONTENT_TYPE) {
            Some(ct)
                if ct.as_bytes().starts_with(EVENT_STREAM_MIME_TYPE.as_bytes())
                    || ct.as_bytes().starts_with(JSON_MIME_TYPE.as_bytes()) => {}
            other => {
                return Err(StreamableHttpError::UnexpectedContentType(
                    other.map(|ct| String::from_utf8_lossy(ct.as_bytes()).into_owned()),
                ));
            }
        }
        Ok(SseStream::new(response.into_body()).boxed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::serve_on_local_socket as serve;
    use ahma_common::local_socket::LocalListener;
    use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};
    use rmcp::transport::streamable_http_client::{
        StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
    };
    use std::collections::HashMap;

    fn ping() -> ClientJsonRpcMessage {
        serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0", "id": 7, "method": "ping"
        }))
        .expect("a ping request")
    }

    /// The proxy reaches the daemon's MCP endpoint through this client, so it
    /// must work where the daemon now listens: an `AF_UNIX` socket on every OS,
    /// Windows included (SPEC R-DAEMON.2). rmcp's own client is Unix-only.
    #[tokio::test]
    async fn a_request_round_trips_over_the_local_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mcp.sock");
        let app = axum::Router::new().route(
            "/mcp",
            axum::routing::post(|headers: axum::http::HeaderMap, body: String| async move {
                let request: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(
                    headers.get("mcp-session-id").and_then(|v| v.to_str().ok()),
                    Some("s-1"),
                    "the session header travels"
                );
                (
                    [
                        ("content-type", "application/json"),
                        ("mcp-session-id", "s-1"),
                    ],
                    serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "result": {}})
                        .to_string(),
                )
            }),
        );
        serve(LocalListener::bind(&sock).unwrap(), app);

        let client = LocalSocketHttpClient::new(&sock.to_string_lossy(), "http://localhost/mcp");
        let response = client
            .post_message(
                "http://localhost/mcp".into(),
                ping(),
                Some("s-1".into()),
                None,
                HashMap::new(),
            )
            .await
            .expect("the endpoint answers");
        match response {
            StreamableHttpPostResponse::Json(ServerJsonRpcMessage::Response(r), session) => {
                assert_eq!(r.id, rmcp::model::RequestId::Number(7));
                assert_eq!(session.as_deref(), Some("s-1"));
            }
            other => panic!("expected a JSON response, got {other:?}"),
        }
    }

    /// The proxy recovers a gone endpoint by respawning the daemon, and it
    /// recognises "gone" by the io error kind. That has to survive the trip
    /// through this client, whatever the OS calls the error.
    #[tokio::test]
    async fn a_missing_endpoint_is_reported_as_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("nobody.sock");
        let client = LocalSocketHttpClient::new(&sock.to_string_lossy(), "http://localhost/mcp");
        let err = client
            .post_message(
                "http://localhost/mcp".into(),
                ping(),
                None,
                None,
                HashMap::new(),
            )
            .await
            .expect_err("nothing is listening");
        match err {
            StreamableHttpError::Client(LocalSocketError::Io(io)) => assert!(
                matches!(
                    io.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ),
                "{io:?}"
            ),
            other => panic!("expected an io error, got {other:?}"),
        }
    }

    /// A bridge's JSON-RPC error body must reach the proxy intact: it is how a
    /// 409 "sandbox initializing" answer becomes an instruction the model can
    /// act on rather than a generic failure.
    #[tokio::test]
    async fn a_non_success_status_carries_its_body() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mcp.sock");
        let app = axum::Router::new().route(
            "/mcp",
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::CONFLICT,
                    r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32001,"message":"wait"}}"#,
                )
            }),
        );
        serve(LocalListener::bind(&sock).unwrap(), app);

        let client = LocalSocketHttpClient::new(&sock.to_string_lossy(), "http://localhost/mcp");
        let err = client
            .post_message(
                "http://localhost/mcp".into(),
                ping(),
                None,
                None,
                HashMap::new(),
            )
            .await
            .expect_err("a 409 is an error");
        let rendered = err.to_string();
        assert!(
            rendered.contains("HTTP 409") && rendered.contains("-32001"),
            "{rendered}"
        );
    }

    #[test]
    fn the_host_header_comes_from_the_uri() {
        let client = LocalSocketHttpClient::new("/run/x.sock", "http://mcp.internal:8080/mcp");
        assert_eq!(client.host_header, "mcp.internal:8080");
        let client = LocalSocketHttpClient::new("/run/x.sock", "/mcp");
        assert_eq!(client.host_header, "localhost");
    }

    #[test]
    fn a_reserved_header_is_refused() {
        let mut headers = HashMap::new();
        headers.insert(
            http::HeaderName::from_static("accept"),
            http::HeaderValue::from_static("text/plain"),
        );
        assert!(matches!(
            apply_custom_headers(http::Request::builder(), headers),
            Err(StreamableHttpError::ReservedHeaderConflict(_))
        ));

        let mut headers = HashMap::new();
        headers.insert(
            http::HeaderName::from_static("mcp-protocol-version"),
            http::HeaderValue::from_static("2025-11-25"),
        );
        assert!(
            apply_custom_headers(http::Request::builder(), headers).is_ok(),
            "the worker injects the protocol version after initialize"
        );
    }

    #[test]
    fn the_scope_is_read_from_www_authenticate() {
        assert_eq!(
            extract_scope_from_header(r#"Bearer error="insufficient_scope", scope="a b""#),
            Some("a b".to_string())
        );
        assert_eq!(
            extract_scope_from_header("Bearer scope=read:data, x=y"),
            Some("read:data".to_string())
        );
        assert_eq!(extract_scope_from_header("Bearer realm=x"), None);
    }
}
