//! Integration tests for /health and /restart endpoint.

mod common;

use ahma_common::timeouts::TestTimeouts;
use common::spawn_test_server;
use serde_json::Value;

#[tokio::test]
async fn test_health_check_version_and_restart() {
    let server = spawn_test_server()
        .await
        .expect("Failed to spawn test server");
    let client = common::make_h2_client();

    // 1. Probe /health
    let resp = client
        .get(format!("{}/health", server.base_url()))
        .send()
        .await
        .expect("HTTP request should complete");
    assert_eq!(resp.status().as_u16(), 200);

    let txt = resp.text().await.expect("Response should have text");
    println!("DEBUG: /health response text: {:?}", txt);
    let body: Value = serde_json::from_str(&txt).expect("Response should be JSON");
    assert_eq!(body.get("status").unwrap().as_str().unwrap(), "OK");
    assert_eq!(
        body.get("version").unwrap().as_str().unwrap(),
        env!("CARGO_PKG_VERSION")
    );

    // 2. Probe /restart
    let resp = client
        .post(format!("{}/restart", server.base_url()))
        .send()
        .await
        .expect("HTTP request should complete");
    assert_eq!(resp.status().as_u16(), 200);

    let body: Value = resp.json().await.expect("Response should be JSON");
    assert_eq!(body.get("status").unwrap().as_str().unwrap(), "restarting");
    assert_eq!(
        body.get("version").unwrap().as_str().unwrap(),
        env!("CARGO_PKG_VERSION")
    );

    // 3. Wait/poll to verify server process exited
    let start = std::time::Instant::now();
    let mut exited = false;
    let poll_interval = TestTimeouts::poll_interval();
    while start.elapsed() < TestTimeouts::scale_secs(5) {
        tokio::time::sleep(poll_interval).await;
        if client
            .get(format!("{}/health", server.base_url()))
            .send()
            .await
            .is_err()
        {
            exited = true;
            break;
        }
    }
    assert!(exited, "Server should have exited after restart request");
}
