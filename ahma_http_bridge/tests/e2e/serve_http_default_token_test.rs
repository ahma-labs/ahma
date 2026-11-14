//! `ahma serve http` requires a token by default (SPEC R-HUB.14).
//!
//! The rest of this binary runs its servers with `--allow-unauthenticated`, since
//! it tests the protocol and not the credential. This is the one that starts a
//! server the way an operator does, with no flag: it must refuse a request that
//! carries no token, serve one that carries the token the server published for
//! its port, and leave `/health` open.

use crate::common;
use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};

async fn status(url: &str, authorization: Option<&str>) -> reqwest::StatusCode {
    let client = reqwest::Client::new();
    let mut request = client
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .timeout(TestTimeouts::get(TimeoutCategory::HttpRequest))
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
    if let Some(value) = authorization {
        request = request.header("authorization", value);
    }
    request.send().await.expect("the server answers").status()
}

#[tokio::test]
async fn a_server_started_with_no_flags_requires_the_token_it_published() {
    let server = common::spawn_test_server_with_default_auth()
        .await
        .expect("a server starts");
    let mcp = format!("{}/mcp", server.base_url());

    // /health stays open for a probe.
    let health = reqwest::get(format!("{}/health", server.base_url()))
        .await
        .expect("health answers");
    assert!(health.status().is_success(), "{}", health.status());

    // Everything else wants the token.
    assert_eq!(status(&mcp, None).await, reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(
        status(&mcp, Some(&format!("Bearer {}", "0".repeat(64)))).await,
        reqwest::StatusCode::UNAUTHORIZED
    );
    let restart = reqwest::Client::new()
        .post(format!("{}/restart", server.base_url()))
        .send()
        .await
        .expect("restart answers");
    assert_eq!(
        restart.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "an unauthenticated /restart must not stop the server"
    );

    // The token the server published for its port is the credential.
    let token = server
        .published_token()
        .await
        .expect("the server publishes a token for its port");
    assert_eq!(token.len(), 64, "{token}");
    let with_token = status(&mcp, Some(&format!("Bearer {token}"))).await;
    assert_ne!(
        with_token,
        reqwest::StatusCode::UNAUTHORIZED,
        "the published token is accepted"
    );
}
