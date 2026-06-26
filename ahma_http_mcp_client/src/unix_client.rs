//! Unix domain socket MCP transport.
//!
//! This module provides a convenience function to create an MCP client transport
//! that communicates with an ahma HTTP bridge over a Unix domain socket, using
//! the rmcp `StreamableHttpClientTransport` backed by `UnixSocketHttpClient`.
//!
//! This is a Unix-only module (`#[cfg(unix)]`).

#[cfg(unix)]
pub use unix_impl::*;

#[cfg(unix)]
mod unix_impl {
    use anyhow::Result;
    use rmcp::transport::{
        StreamableHttpClientTransport, UnixSocketHttpClient,
        streamable_http_client::StreamableHttpClientTransportConfig,
    };

    /// Create an MCP Streamable HTTP transport that connects via a Unix domain socket.
    ///
    /// The socket path may be a filesystem path (e.g. `/tmp/ahma.sock`) or
    /// a Linux abstract socket with the `@` prefix (e.g. `@ahma`).
    ///
    /// The `uri` is the HTTP URI used inside the socket connection, e.g.
    /// `http://localhost/mcp` (the host portion is used for HTTP `Host` headers
    /// but is not routed over TCP).
    pub fn unix_socket_transport(
        socket_path: &str,
        uri: &str,
    ) -> Result<StreamableHttpClientTransport<UnixSocketHttpClient>> {
        let client = UnixSocketHttpClient::new(socket_path, uri);
        let config = StreamableHttpClientTransportConfig::with_uri(uri);
        Ok(StreamableHttpClientTransport::with_client(client, config))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// A filesystem socket path constructs a transport successfully.
    ///
    /// Note: the function only builds the transport; it performs no I/O and
    /// does not connect, so the socket path need not exist.
    #[tokio::test]
    async fn filesystem_socket_path_constructs_ok() {
        let temp_dir = tempdir().unwrap();
        let socket = temp_dir.path().join("ahma.sock");
        let socket_path = socket.to_string_lossy();

        let result = unix_socket_transport(&socket_path, "http://localhost/mcp");
        assert!(
            result.is_ok(),
            "expected Ok transport for filesystem socket path"
        );
    }

    /// A Linux abstract socket form (`@`-prefixed) also constructs successfully.
    #[tokio::test]
    async fn abstract_socket_path_constructs_ok() {
        let result = unix_socket_transport("@ahma", "http://localhost/mcp");
        assert!(
            result.is_ok(),
            "expected Ok transport for abstract socket name"
        );
    }

    /// Different URI values all construct successfully.
    #[tokio::test]
    async fn varied_uris_construct_ok() {
        let temp_dir = tempdir().unwrap();
        let socket = temp_dir.path().join("ahma.sock");
        let socket_path = socket.to_string_lossy();

        for uri in [
            "http://localhost/mcp",
            "http://127.0.0.1:3000/mcp",
            "http://example.invalid/api/v1/mcp",
        ] {
            let result = unix_socket_transport(&socket_path, uri);
            assert!(result.is_ok(), "expected Ok transport for uri {uri}");
        }
    }
}
