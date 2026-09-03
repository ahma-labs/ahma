//! Bridge between `oauth2`'s HTTP abstraction and the workspace `reqwest` client.
//!
//! `oauth2`'s default features ship their own `reqwest` (a second major, 0.12,
//! with its own hyper-rustls/h2/tower-http duplicates and the `ring` provider).
//! This crate disables those features and instead drives every token request
//! through the one `reqwest::Client` (0.13) it already owns for MCP traffic, so
//! the OAuth flow shares the same TLS/HTTP configuration and no duplicate HTTP
//! stack is linked into any binary.
//!
//! The adapter mirrors `oauth2`'s own (feature-gated) `reqwest_client.rs`:
//! `oauth2::HttpRequest` is `http::Request<Vec<u8>>`, which `reqwest` converts
//! directly; the response's status, version, headers and body bytes are copied
//! back into an `oauth2::HttpResponse` (`http::Response<Vec<u8>>`).

use oauth2::{AsyncHttpClient, HttpClientError, HttpRequest, HttpResponse};
use std::{future::Future, pin::Pin};

/// A [`reqwest::Client`] that `oauth2` token requests can be sent through.
///
/// Cheap to construct from an existing client — `reqwest::Client` is an `Arc`
/// internally, so `OAuthHttpClient::from(client.clone())` shares the same
/// connection pool and TLS/HTTP configuration as the MCP transport itself.
#[derive(Clone, Debug)]
pub struct OAuthHttpClient {
    client: reqwest::Client,
}

impl OAuthHttpClient {
    /// Wraps an existing workspace `reqwest::Client`.
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl From<reqwest::Client> for OAuthHttpClient {
    fn from(client: reqwest::Client) -> Self {
        Self::new(client)
    }
}

impl<'c> AsyncHttpClient<'c> for OAuthHttpClient {
    type Error = HttpClientError<reqwest::Error>;
    type Future =
        Pin<Box<dyn Future<Output = Result<HttpResponse, Self::Error>> + Send + Sync + 'c>>;

    fn call(&'c self, request: HttpRequest) -> Self::Future {
        Box::pin(async move {
            let response = self
                .client
                .execute(request.try_into().map_err(Box::new)?)
                .await
                .map_err(Box::new)?;

            let mut builder = oauth2::http::Response::builder()
                .status(response.status())
                .version(response.version());
            for (name, value) in response.headers() {
                builder = builder.header(name, value);
            }

            builder
                .body(response.bytes().await.map_err(Box::new)?.to_vec())
                .map_err(HttpClientError::Http)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oauth2::AsyncHttpClient;
    use oauth2::http::{Method, Request, StatusCode};
    use wiremock::{Mock, MockServer, Respond, ResponseTemplate, matchers};

    /// Mirrors the request back so the test can prove the adapter forwarded the
    /// method, a header and the body unchanged, and copies the response back
    /// verbatim (status, header, body).
    struct Echo;

    impl Respond for Echo {
        fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
            let probe = request
                .headers
                .get("x-probe")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<missing>")
                .to_string();
            ResponseTemplate::new(202)
                .insert_header("x-echo-method", request.method.as_str())
                .insert_header("x-echo-probe", probe.as_str())
                .set_body_bytes(request.body.clone())
        }
    }

    #[tokio::test]
    async fn adapter_round_trips_method_headers_and_body_through_workspace_reqwest() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/echo"))
            .respond_with(Echo)
            .expect(1)
            .mount(&server)
            .await;

        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("{}/echo", server.uri()))
            .header("x-probe", "forty-two")
            .body(b"grant_type=echo&code=abc".to_vec())
            .unwrap();

        let client = OAuthHttpClient::new(reqwest::Client::new());
        let response = client.call(request).await.expect("adapter call succeeds");

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            response.headers().get("x-echo-method").unwrap(),
            "POST",
            "method must reach the server unchanged"
        );
        assert_eq!(
            response.headers().get("x-echo-probe").unwrap(),
            "forty-two",
            "request headers must reach the server unchanged"
        );
        assert_eq!(
            response.body().as_slice(),
            b"grant_type=echo&code=abc",
            "body must round-trip byte for byte"
        );
    }

    #[tokio::test]
    async fn adapter_surfaces_transport_failure_as_reqwest_error() {
        // Nothing listens here: the port is taken from a listener that is
        // immediately dropped, so the connection is refused rather than hanging.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let request = Request::builder()
            .method(Method::GET)
            .uri(format!("http://127.0.0.1:{port}/nowhere"))
            .body(Vec::new())
            .unwrap();

        let client = OAuthHttpClient::from(reqwest::Client::new());
        let err = client
            .call(request)
            .await
            .expect_err("a refused connection must be an error, not a panic");
        assert!(
            matches!(err, oauth2::HttpClientError::Reqwest(_)),
            "transport failures map to HttpClientError::Reqwest, got {err:?}"
        );
    }
}
