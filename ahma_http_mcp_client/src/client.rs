//! HTTP MCP transport implementation.
//!
//! This module provides the `HttpMcpTransport`, which implements the MCP transport
//! over HTTP and manages optional OAuth2 authentication for Atlassian-compatible
//! providers.

use crate::error::{McpHttpError, Result};
use crate::oauth_http::OAuthHttpClient;
use ahma_common::state_machine::StateMachine;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse, TokenUrl, basic::BasicClient,
};
use rmcp::{
    RoleClient,
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::Transport,
};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::{BufReader, BufWriter, Write},
    path::PathBuf,
    sync::Arc,
};
use tokio::sync::{Mutex, Notify, mpsc};
use tracing::{error, info};
use url::Url;

type ConfiguredOAuthClient = oauth2::Client<
    oauth2::StandardErrorResponse<oauth2::basic::BasicErrorResponseType>,
    oauth2::StandardTokenResponse<oauth2::EmptyExtraTokenFields, oauth2::basic::BasicTokenType>,
    oauth2::StandardTokenIntrospectionResponse<
        oauth2::EmptyExtraTokenFields,
        oauth2::basic::BasicTokenType,
    >,
    oauth2::StandardRevocableToken,
    oauth2::StandardErrorResponse<oauth2::RevocationErrorResponseType>,
    oauth2::EndpointSet,
    oauth2::EndpointNotSet,
    oauth2::EndpointNotSet,
    oauth2::EndpointNotSet,
    oauth2::EndpointSet,
>;

/// The loopback port the OAuth provider redirects back to.
///
/// Fixed rather than ephemeral because it is baked into the redirect URI
/// registered with the provider: an ephemeral port would not match, and the
/// provider would refuse the redirect. One constant so the URI advertised in
/// `set_redirect_uri` and the address actually bound cannot drift apart.
const OAUTH_CALLBACK_PORT: u16 = 8080;

/// The address [`OAUTH_CALLBACK_PORT`] is bound on. Loopback only: the callback
/// carries an authorization code, and there is no reason for any other host to
/// be able to reach it.
const OAUTH_CALLBACK_ADDR: &str = "127.0.0.1:8080";

const TOKEN_FILE_NAME: &str = "mcp_http_token.json";
/// Environment variable to override the token storage path.
const TOKEN_PATH_ENV: &str = "AHMA_HTTP_CLIENT_TOKEN_PATH";

/// HTTP Transport implementation for the Model Context Protocol (MCP).
///
/// This transport sends JSON-RPC messages over HTTP POST requests and supports
/// receiving responses. It handles OAuth2 authentication with Atlassian-compatible
/// providers, managing the token lifecycle (storage, loading).
///
/// # Token Storage
///
/// Tokens are stored in a JSON file. By default, this is `mcp_http_token.json` in the system's
/// temporary directory. You can override the full path to this file by setting the
/// `AHMA_HTTP_CLIENT_TOKEN_PATH` environment variable.
pub struct HttpMcpTransport {
    client: reqwest::Client,
    mcp_url: Url,

    auth_state: Arc<StateMachine<AuthState>>,
    oauth_client: Option<ConfiguredOAuthClient>,
    receiver: Arc<Mutex<mpsc::Receiver<RxJsonRpcMessage<RoleClient>>>>,
    sender: mpsc::Sender<RxJsonRpcMessage<RoleClient>>,
}

#[derive(Debug)]
enum AuthState {
    Unauthenticated,
    Authenticating(Arc<Notify>),
    Authenticated(StoredToken),
}

impl HttpMcpTransport {
    /// Creates a new `HttpMcpTransport`.
    ///
    /// The `url` should point to the MCP server's endpoint.
    /// If OAuth2 authentication is required (e.g. for Atlassian), provide `atlassian_client_id`
    /// and `atlassian_client_secret`.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL of the MCP server.
    /// * `atlassian_client_id` - Optional OAuth2 Client ID.
    /// * `atlassian_client_secret` - Optional OAuth2 Client Secret.
    ///
    /// # Example
    ///
    /// ```
    /// # use ahma_http_mcp_client::client::HttpMcpTransport;
    /// # use url::Url;
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let url = Url::parse("http://localhost:8000/mcp")?;
    /// let transport = HttpMcpTransport::new(url, None, None)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn new(
        url: Url,
        atlassian_client_id: Option<String>,
        atlassian_client_secret: Option<String>,
    ) -> Result<Self> {
        let oauth_client = if let (Some(atlassian_client_id), Some(atlassian_client_secret)) =
            (atlassian_client_id, atlassian_client_secret)
        {
            let mut client = BasicClient::new(ClientId::new(atlassian_client_id))
                .set_client_secret(ClientSecret::new(atlassian_client_secret))
                .set_auth_uri(AuthUrl::new(
                    "https://auth.atlassian.com/authorize".to_string(),
                )?)
                .set_token_uri(TokenUrl::new(
                    "https://auth.atlassian.com/oauth/token".to_string(),
                )?);
            client = client.set_redirect_uri(RedirectUrl::new(format!(
                "http://localhost:{OAUTH_CALLBACK_PORT}"
            ))?);
            Some(client)
        } else {
            None
        };

        let (sender, receiver) = mpsc::channel(100);

        let initial_token = load_token()?;
        let initial_state = match initial_token {
            Some(token) => AuthState::Authenticated(token),
            None => AuthState::Unauthenticated,
        };

        // Build HTTP client preferring HTTP/3 (QUIC) when the server supports it.
        // reqwest with the `http3` feature automatically upgrades via Alt-Svc headers.
        let http_client = reqwest::Client::builder()
            .build()
            .map_err(|e| McpHttpError::Custom(format!("Failed to build HTTP client: {e}")))?;

        let transport = Self {
            client: http_client,
            mcp_url: url,
            auth_state: Arc::new(StateMachine::new(initial_state)),
            oauth_client,
            receiver: Arc::new(Mutex::new(receiver)),
            sender,
        };

        Ok(transport)
    }

    /// Checks if a valid token exists, and if not, initiates the OAuth2 flow.
    ///
    /// If an OAuth2 client was configured during creation (via `atlassian_client_id` and secret),
    /// this method will:
    /// 1. Check if a token is already loaded in memory.
    /// 2. (Future) Check for token expiration and refresh if needed.
    /// 3. If no token is present, start the interactive OAuth2 flow, prompting the user
    ///    to open a URL in their browser.
    /// 4. Wait for the callback on `localhost:8080`, exchange the code for a token,
    ///    and save it locally.
    ///
    /// If no OAuth2 client is configured, this returns an error if no token is present.
    pub async fn ensure_authenticated(&self) -> Result<()> {
        loop {
            enum Action {
                PerformAuth(Arc<Notify>),
                Wait(Arc<Notify>),
                Ok,
            }

            let action = self.auth_state.transition(|state| match state {
                AuthState::Authenticated(_) => Action::Ok,
                AuthState::Authenticating(notify) => Action::Wait(notify.clone()),
                AuthState::Unauthenticated => {
                    let notify = Arc::new(Notify::new());
                    *state = AuthState::Authenticating(notify.clone());
                    Action::PerformAuth(notify)
                }
            });

            match action {
                Action::Ok => return Ok(()),
                Action::Wait(notify) => {
                    notify.notified().await;
                    continue;
                }
                Action::PerformAuth(notify) => {
                    if let Some(oauth_client) = &self.oauth_client {
                        info!("No token found, starting authentication flow.");
                        match self.perform_oauth_flow(oauth_client).await {
                            Ok(new_token) => {
                                self.auth_state.transition(|state| {
                                    *state = AuthState::Authenticated(new_token);
                                });
                                info!("Authentication successful.");
                                notify.notify_waiters();
                                return Ok(());
                            }
                            Err(e) => {
                                self.auth_state.transition(|state| {
                                    *state = AuthState::Unauthenticated;
                                });
                                notify.notify_waiters();
                                return Err(e);
                            }
                        }
                    } else {
                        self.auth_state.transition(|state| {
                            *state = AuthState::Unauthenticated;
                        });
                        notify.notify_waiters();
                        return Err(McpHttpError::Auth(
                            "OAuth client not configured, but authentication is required."
                                .to_string(),
                        ));
                    }
                }
            }
        }
    }

    async fn perform_oauth_flow(
        &self,
        oauth_client: &ConfiguredOAuthClient,
    ) -> Result<StoredToken> {
        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

        let scopes = vec![
            "read:me",
            "read:confluence-content.summary",
            "read:confluence-space.summary",
            "read:confluence-props",
            "read:confluence-content.all",
            "read:confluence-user",
            "read:jira-user",
            "read:jira-work",
            "write:jira-work",
            "read:confluence-content.permission",
            "offline_access",
        ]
        .into_iter()
        .map(|s| Scope::new(s.to_string()));

        let (auth_url, csrf_token) = oauth_client
            .authorize_url(CsrfToken::new_random)
            .set_pkce_challenge(pkce_challenge)
            .add_scopes(scopes)
            .url();

        info!("Please open this URL in your browser to authenticate:");
        info!("{}", auth_url);

        if webbrowser::open(auth_url.as_str()).is_err() {
            error!(
                "Failed to open web browser automatically. Please copy the URL and open it manually."
            );
        }

        let (code, state) = self.listen_for_callback_async().await?;

        if state.secret() != csrf_token.secret() {
            return Err(McpHttpError::Auth("CSRF token mismatch".to_string()));
        }

        let stored_token = self
            .exchange_authorization_code(oauth_client, code, pkce_verifier)
            .await?;

        save_token(&stored_token)?;

        Ok(stored_token)
    }

    /// Redeem an authorization code at the provider's token endpoint.
    ///
    /// The request goes out through this transport's own `reqwest::Client`
    /// (via [`OAuthHttpClient`]) rather than a client bundled with `oauth2`, so
    /// the token exchange shares the MCP traffic's TLS/HTTP configuration and
    /// no second HTTP stack is linked in. Split from the interactive flow so
    /// the one network hop that needs no browser can be tested against a mock
    /// token endpoint.
    async fn exchange_authorization_code(
        &self,
        oauth_client: &ConfiguredOAuthClient,
        code: String,
        pkce_verifier: PkceCodeVerifier,
    ) -> Result<StoredToken> {
        let http_client = OAuthHttpClient::from(self.client.clone());
        let token_result = oauth_client
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(pkce_verifier)
            .request_async(&http_client)
            .await
            .map_err(|e| McpHttpError::OAuth2(format!("{:?}", e)))?;

        Ok(StoredToken {
            access_token: token_result.access_token().secret().to_string(),
            refresh_token: token_result
                .refresh_token()
                .map(|rt| rt.secret().to_string()),
            expires_in: token_result.expires_in().map(|d| d.as_secs()),
            scopes: token_result
                .scopes()
                .map(|s| s.iter().map(|sc| sc.to_string()).collect()),
        })
    }

    /// Bind the registered callback address and wait for the provider's redirect.
    ///
    /// The port is fixed because it is part of the redirect URI registered with
    /// the OAuth provider (see [`OAUTH_CALLBACK_ADDR`]) — it cannot be ephemeral
    /// in production. The bind is therefore the one step that can fail for a
    /// reason entirely outside ahma, so it says so: `Address already in use` on
    /// its own gives a user no way to connect the failure to whatever else is
    /// holding 8080.
    async fn listen_for_callback_async(&self) -> Result<(String, CsrfToken)> {
        let listener = tokio::net::TcpListener::bind(OAUTH_CALLBACK_ADDR)
            .await
            .map_err(|e| {
                McpHttpError::Auth(format!(
                    "cannot listen on http://{OAUTH_CALLBACK_ADDR} for the OAuth callback: {e}. \
                     This address is fixed because it is the redirect URI registered with the \
                     provider. Stop whatever is using the port and retry."
                ))
            })?;
        info!("Listening on http://{OAUTH_CALLBACK_ADDR} for OAuth callback.");
        Self::accept_callback(listener).await
    }

    /// The callback protocol itself, on an already-bound listener.
    ///
    /// Split from [`Self::listen_for_callback_async`] so it can be exercised on
    /// an ephemeral port. Its test used to drive the real
    /// `listen_for_callback_async`, and therefore the real 8080: on any machine
    /// where something else held that port the test did not fail, it *hung* —
    /// the client half blocked in `read_to_end` against a stranger's socket
    /// until nextest killed the process at 120s. That is a test which reports
    /// "timed out" for a condition that has nothing to do with the code under
    /// test, and it would have made the new `--run-ignored all` workflow flaky
    /// on any runner with a busy 8080.
    async fn accept_callback(listener: tokio::net::TcpListener) -> Result<(String, CsrfToken)> {
        let (mut stream, _) = listener.accept().await?;

        let (reader, mut writer) = tokio::io::split(&mut stream);
        let mut reader = tokio::io::BufReader::new(reader);

        let mut request_line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut request_line).await?;

        let redirect_url = request_line.split_whitespace().nth(1).unwrap_or("/");
        let url = Url::parse(&("http://localhost".to_string() + redirect_url))?;

        let code = url
            .query_pairs()
            .find(|(key, _)| key == "code")
            .map(|(_, value)| value.into_owned())
            .ok_or_else(|| McpHttpError::Auth("Missing auth code in callback".to_string()))?;

        let state = url
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| CsrfToken::new(value.into_owned()))
            .ok_or_else(|| McpHttpError::Auth("Missing state in callback".to_string()))?;

        let message = "Authentication successful! You can close this tab.";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            message.len(),
            message
        );
        use tokio::io::AsyncWriteExt;
        writer.write_all(response.as_bytes()).await?;
        writer.flush().await?;

        Ok((code, state))
    }
}

impl Transport<RoleClient> for HttpMcpTransport {
    type Error = McpHttpError;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl std::future::Future<Output = std::result::Result<(), Self::Error>> + Send + 'static
    {
        let client = self.client.clone();
        let auth_state = self.auth_state.clone();
        let mcp_url = self.mcp_url.clone();
        let sender = self.sender.clone();

        async move {
            // Ensure authenticated
            let access_token = auth_state.transition(|state| match state {
                AuthState::Authenticated(token) => Ok(token.access_token.clone()),
                _ => Err(McpHttpError::MissingAccessToken),
            })?;

            let res = client
                .post(mcp_url)
                .bearer_auth(access_token)
                .json(&item)
                .send()
                .await?;

            if !res.status().is_success() {
                let status = res.status();
                let text = res.text().await.unwrap_or_default();
                let err_msg = format!("HTTP Error: {} - {}", status, text);
                error!("{}", err_msg);
                return Err(McpHttpError::Custom(err_msg));
            }

            // Try to parse response as JSON-RPC message and send to channel
            // Note: Notifications might not return a body, or return empty body
            // We read the full body bytes to determine if it's empty, rather than relying on
            // Content-Length header which might be missing or unreliable in some environments (e.g. CI).
            let body_bytes = res.bytes().await?;
            if !body_bytes.is_empty() {
                // Try to parse non-empty body
                match serde_json::from_slice::<RxJsonRpcMessage<RoleClient>>(&body_bytes) {
                    Ok(msg) => {
                        if let Err(e) = sender.send(msg).await {
                            error!("Failed to send response to channel: {}", e);
                            return Err(McpHttpError::Custom(format!("Channel error: {}", e)));
                        }
                    }
                    Err(e) => {
                        error!("Failed to parse response body: {}", e);
                        // Start of body for debugging context (first 100 chars)
                        let preview = String::from_utf8_lossy(&body_bytes)
                            .chars()
                            .take(100)
                            .collect::<String>();
                        error!("Response body start: {:?}", preview);

                        return Err(McpHttpError::Json(e));
                    }
                }
            }

            Ok(())
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        let mut receiver = self.receiver.lock().await;
        receiver.recv().await
    }

    async fn close(&mut self) -> std::result::Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredToken {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    scopes: Option<Vec<String>>,
}

fn load_token() -> Result<Option<StoredToken>> {
    let path = token_file_path()?;
    if !path.exists() {
        return Ok(None);
    }

    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let token = serde_json::from_reader(reader)?;
    Ok(Some(token))
}

fn save_token(token: &StoredToken) -> Result<()> {
    let path = token_file_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }

    let mut options = fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, token)?;
    writer.flush()?;
    Ok(())
}

fn token_file_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os(TOKEN_PATH_ENV) {
        return Ok(PathBuf::from(path));
    }
    let home = ahma_common::config::ahma_home_dir()
        .ok_or_else(|| McpHttpError::Custom("Could not determine home directory".to_string()))?;
    Ok(home.join(".ahma").join(TOKEN_FILE_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ahma_common::timeouts::TestTimeouts;
    use std::sync::{Mutex as StdMutex, OnceLock};
    use tempfile::tempdir;

    fn token_env_guard() -> &'static StdMutex<()> {
        static GUARD: OnceLock<StdMutex<()>> = OnceLock::new();
        GUARD.get_or_init(|| StdMutex::new(()))
    }

    #[test]
    fn load_token_returns_none_when_override_missing() {
        let _guard = token_env_guard().lock();
        let tmp = tempdir().unwrap();
        let token_path = tmp.path().join("custom_token.json");
        unsafe {
            env::set_var(TOKEN_PATH_ENV, token_path.to_str().unwrap());
        }

        let loaded = load_token().unwrap();
        assert!(loaded.is_none());

        unsafe {
            env::remove_var(TOKEN_PATH_ENV);
        }
    }

    #[test]
    fn save_token_round_trips_via_override_path() {
        let _guard = token_env_guard().lock();
        let tmp = tempdir().unwrap();
        let token_path = tmp.path().join("custom_token.json");
        unsafe {
            env::set_var(TOKEN_PATH_ENV, token_path.to_str().unwrap());
        }

        let token = StoredToken {
            access_token: "abc123".to_string(),
            refresh_token: Some("ref-456".to_string()),
            expires_in: Some(3600),
            scopes: Some(vec!["scope1".to_string(), "scope2".to_string()]),
        };

        save_token(&token).unwrap();
        let loaded = load_token().unwrap().expect("token to exist");
        assert_eq!(loaded.access_token, token.access_token);
        assert_eq!(loaded.refresh_token, token.refresh_token);
        assert_eq!(loaded.expires_in, token.expires_in);
        assert_eq!(loaded.scopes, token.scopes);

        unsafe {
            env::remove_var(TOKEN_PATH_ENV);
        }
    }

    #[test]
    fn token_file_path_uses_env_override() {
        let _guard = token_env_guard().lock();
        let custom_path = "/custom/path/token.json";
        unsafe {
            env::set_var(TOKEN_PATH_ENV, custom_path);
        }

        let path = token_file_path().unwrap();
        assert_eq!(path.to_str().unwrap(), custom_path);

        unsafe {
            env::remove_var(TOKEN_PATH_ENV);
        }
    }

    #[test]
    fn token_file_path_uses_home_dir_default() {
        let _guard = token_env_guard().lock();
        unsafe {
            env::remove_var(TOKEN_PATH_ENV);
        }

        let path = token_file_path().unwrap();
        assert!(path.ends_with(TOKEN_FILE_NAME));
        assert!(path.starts_with(ahma_common::config::ahma_home_dir().unwrap().join(".ahma")));
    }

    #[test]
    fn save_token_creates_parent_directories() {
        let _guard = token_env_guard().lock();
        let tmp = tempdir().unwrap();
        let token_path = tmp.path().join("nested/deep/token.json");
        unsafe {
            env::set_var(TOKEN_PATH_ENV, token_path.to_str().unwrap());
        }

        let token = StoredToken {
            access_token: "test".to_string(),
            refresh_token: None,
            expires_in: None,
            scopes: None,
        };

        save_token(&token).unwrap();
        assert!(token_path.exists());

        unsafe {
            env::remove_var(TOKEN_PATH_ENV);
        }
    }

    #[test]
    fn stored_token_minimal_fields() {
        let token = StoredToken {
            access_token: "test_token".to_string(),
            refresh_token: None,
            expires_in: None,
            scopes: None,
        };

        let json = serde_json::to_string(&token).unwrap();
        assert!(json.contains("test_token"));

        let parsed: StoredToken = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.access_token, "test_token");
        assert!(parsed.refresh_token.is_none());
    }

    #[test]
    fn stored_token_debug_display() {
        let token = StoredToken {
            access_token: "secret".to_string(),
            refresh_token: Some("refresh".to_string()),
            expires_in: Some(3600),
            scopes: Some(vec!["scope1".to_string()]),
        };

        let debug = format!("{:?}", token);
        assert!(debug.contains("StoredToken"));
        assert!(debug.contains("secret")); // Note: in real impl might want to redact
    }

    #[test]
    fn stored_token_clone() {
        let token = StoredToken {
            access_token: "abc".to_string(),
            refresh_token: Some("ref".to_string()),
            expires_in: Some(1800),
            scopes: Some(vec!["s1".to_string(), "s2".to_string()]),
        };

        let cloned = token.clone();
        assert_eq!(cloned.access_token, token.access_token);
        assert_eq!(cloned.refresh_token, token.refresh_token);
        assert_eq!(cloned.expires_in, token.expires_in);
        assert_eq!(cloned.scopes, token.scopes);
    }

    /// The token endpoint is the only network hop in the OAuth flow that does
    /// not involve a browser, so it is the one that can be driven end to end
    /// against wiremock. It proves the exchange goes out through the
    /// transport's own reqwest client (no `oauth2::reqwest`), carries the
    /// authorization-code grant and PKCE verifier, and lands in a `StoredToken`.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn exchange_authorization_code_posts_grant_through_transport_client() {
        use wiremock::matchers::{body_string_contains, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _guard = token_env_guard().lock();
        let tmp = tempdir().unwrap();
        let transport = isolated_transport(&tmp.path().join("exchange.json"));

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(header("content-type", "application/x-www-form-urlencoded"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("code=the_code"))
            .and(body_string_contains("code_verifier="))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "issued_access",
                "token_type": "bearer",
                "expires_in": 1234,
                "refresh_token": "issued_refresh",
                "scope": "read:me offline_access"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let oauth_client = BasicClient::new(ClientId::new("cid".into()))
            .set_client_secret(ClientSecret::new("secret".into()))
            .set_auth_uri(AuthUrl::new(format!("{}/authorize", server.uri())).unwrap())
            .set_token_uri(TokenUrl::new(format!("{}/oauth/token", server.uri())).unwrap())
            .set_redirect_uri(RedirectUrl::new("http://localhost:8080".into()).unwrap());

        let (_challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let token = transport
            .exchange_authorization_code(&oauth_client, "the_code".to_string(), verifier)
            .await
            .expect("token exchange succeeds against the mock endpoint");

        assert_eq!(token.access_token, "issued_access");
        assert_eq!(token.refresh_token.as_deref(), Some("issued_refresh"));
        assert_eq!(token.expires_in, Some(1234));
        assert_eq!(
            token.scopes,
            Some(vec!["read:me".to_string(), "offline_access".to_string()])
        );

        unsafe {
            env::remove_var(TOKEN_PATH_ENV);
        }
    }

    #[test]
    fn test_reqwest_http3_feature_is_available() {
        // Assert that reqwest has the http3 feature enabled.
        // If it is disabled, this will fail to compile.
        let _ = reqwest::Client::builder().http3_prior_knowledge();
    }

    // ── coverage batch: listen_for_callback_async + transport round-trips ──────
    fn isolated_transport(token_path: &std::path::Path) -> HttpMcpTransport {
        unsafe {
            env::set_var(TOKEN_PATH_ENV, token_path.to_str().unwrap());
        }
        let url = Url::parse("http://localhost:8080/mcp").unwrap();
        HttpMcpTransport::new(url, None, None).unwrap()
    }

    /// Send one HTTP request line to an already-bound loopback listener.
    ///
    /// Takes the port rather than assuming 8080: the listener under test is now
    /// bound on an ephemeral port, so there is a specific socket to talk to and
    /// no connect-retry loop is needed. The old version dialled the fixed 8080
    /// and retried for four seconds, which meant that on a machine where some
    /// other process held that port it connected to *that* and then blocked
    /// forever in `read_to_end`.
    async fn drive_callback_client(port: u16, request_line: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("the listener is already bound before this is called");
        stream.write_all(request_line.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        let mut buf = Vec::new();
        let _ = stream.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// Bind an ephemeral loopback listener and report the port it actually got.
    async fn ephemeral_listener() -> (tokio::net::TcpListener, u16) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binding an ephemeral loopback port cannot fail");
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    /// Drive [`HttpMcpTransport::accept_callback`] against one ephemeral
    /// listener and return its verdict alongside the HTTP response the browser
    /// would have seen.
    async fn run_callback(request_line: &str) -> (Result<(String, CsrfToken)>, String) {
        let (listener, port) = ephemeral_listener().await;
        let server_fut = tokio::time::timeout(
            TestTimeouts::scale_secs(10),
            HttpMcpTransport::accept_callback(listener),
        );
        let (server_res, response) =
            tokio::join!(server_fut, drive_callback_client(port, request_line));
        (
            server_res.expect("the client half connects immediately; this cannot time out"),
            response,
        )
    }

    // These three scenarios each get their own ephemeral listener, so they no
    // longer contend for anything and no longer need to be one sequential test.
    // They are kept together only because they are three cases of one behaviour.
    //
    // Previously they drove the real `listen_for_callback_async`, which binds the
    // production port 127.0.0.1:8080. That made the test hostage to whatever else
    // was on the machine: with 8080 taken, the client half connected to the
    // stranger's socket and blocked in `read_to_end` until nextest killed the
    // process at 120 s. Not a failure — a hang, reported as a timeout, on a
    // condition unrelated to the code under test. Found by running the
    // `--run-ignored all` half of the Definition of Done, which nothing had.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn listen_for_callback_async_scenarios() {
        let _guard = token_env_guard().lock();

        // 1. Happy path: both code and state present.
        {
            let (result, response) = run_callback(
                "GET /?code=the_code&state=the_state HTTP/1.1\r\nHost: localhost\r\n\r\n",
            )
            .await;
            let (code, state) = result.expect("listener should return Ok with code/state");
            assert_eq!(code, "the_code");
            assert_eq!(state.secret(), "the_state");
            assert!(response.contains("200 OK"), "response was: {response:?}");
            assert!(
                response.contains("Authentication successful"),
                "response was: {response:?}"
            );
        }

        // 2. Missing code → error.
        {
            let (result, _response) =
                run_callback("GET /?state=only_state HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
            let err = result.expect_err("listener should error when code is missing");
            assert!(
                err.to_string().contains("Missing auth code"),
                "unexpected error: {err}"
            );
        }

        // 3. Missing state → error.
        {
            let (result, _response) =
                run_callback("GET /?code=only_code HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
            let err = result.expect_err("listener should error when state is missing");
            assert!(
                err.to_string().contains("Missing state"),
                "unexpected error: {err}"
            );
        }

        unsafe {
            env::remove_var(TOKEN_PATH_ENV);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn ensure_authenticated_short_circuits_with_stored_token() {
        let _guard = token_env_guard().lock();
        let tmp = tempdir().unwrap();
        let token_path = tmp.path().join("stored.json");
        unsafe {
            env::set_var(TOKEN_PATH_ENV, token_path.to_str().unwrap());
        }

        let token = StoredToken {
            access_token: "stored_access".to_string(),
            refresh_token: Some("r".to_string()),
            expires_in: Some(3600),
            scopes: Some(vec!["read:me".to_string()]),
        };
        save_token(&token).unwrap();

        let url = Url::parse("http://localhost:8080/mcp").unwrap();
        let transport = HttpMcpTransport::new(url, None, None).unwrap();

        let result = transport.ensure_authenticated().await;
        assert!(
            result.is_ok(),
            "ensure_authenticated should short-circuit with a stored token: {result:?}"
        );

        assert!(transport.ensure_authenticated().await.is_ok());

        unsafe {
            env::remove_var(TOKEN_PATH_ENV);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn send_receive_round_trip_with_stored_token() {
        use rmcp::service::TxJsonRpcMessage;
        use rmcp::transport::Transport;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _guard = token_env_guard().lock();
        let tmp = tempdir().unwrap();
        let token_path = tmp.path().join("send_token.json");
        unsafe {
            env::set_var(TOKEN_PATH_ENV, token_path.to_str().unwrap());
        }
        let token = StoredToken {
            access_token: "round_trip_token".to_string(),
            refresh_token: None,
            expires_in: Some(3600),
            scopes: None,
        };
        save_token(&token).unwrap();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "marker": "pong_42" }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let url = Url::parse(&format!("{}/mcp", server.uri())).unwrap();
        let mut transport = HttpMcpTransport::new(url, None, None).unwrap();

        let request: TxJsonRpcMessage<RoleClient> = serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "ping",
            "params": {}
        }))
        .unwrap();

        transport.send(request).await.expect("send should succeed");

        let received = tokio::time::timeout(TestTimeouts::scale_secs(5), transport.receive())
            .await
            .expect("receive should not time out")
            .expect("a response should be queued");
        let received_str = serde_json::to_string(&received).unwrap();
        assert!(
            received_str.contains("pong_42"),
            "received message did not match: {received_str}"
        );

        unsafe {
            env::remove_var(TOKEN_PATH_ENV);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn send_without_token_yields_missing_access_token() {
        use rmcp::service::TxJsonRpcMessage;
        use rmcp::transport::Transport;

        let _guard = token_env_guard().lock();
        let tmp = tempdir().unwrap();
        let mut transport = isolated_transport(&tmp.path().join("absent.json"));

        let request: TxJsonRpcMessage<RoleClient> = serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/list",
            "params": {}
        }))
        .unwrap();

        let err = transport
            .send(request)
            .await
            .expect_err("send must fail without an access token");
        assert!(
            err.to_string().contains("Missing access token"),
            "unexpected error: {err}"
        );

        unsafe {
            env::remove_var(TOKEN_PATH_ENV);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn send_propagates_http_error_status() {
        use rmcp::service::TxJsonRpcMessage;
        use rmcp::transport::Transport;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _guard = token_env_guard().lock();
        let tmp = tempdir().unwrap();
        let token_path = tmp.path().join("err_token.json");
        unsafe {
            env::set_var(TOKEN_PATH_ENV, token_path.to_str().unwrap());
        }
        let token = StoredToken {
            access_token: "any".to_string(),
            refresh_token: None,
            expires_in: None,
            scopes: None,
        };
        save_token(&token).unwrap();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .expect(1)
            .mount(&server)
            .await;

        let url = Url::parse(&format!("{}/mcp", server.uri())).unwrap();
        let mut transport = HttpMcpTransport::new(url, None, None).unwrap();

        let request: TxJsonRpcMessage<RoleClient> = serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "ping",
            "params": {}
        }))
        .unwrap();

        let err = transport
            .send(request)
            .await
            .expect_err("send must surface a non-2xx HTTP status");
        let msg = err.to_string();
        assert!(msg.contains("HTTP Error"), "unexpected error: {msg}");
        assert!(msg.contains("500"), "error should include status: {msg}");

        unsafe {
            env::remove_var(TOKEN_PATH_ENV);
        }
    }
}
