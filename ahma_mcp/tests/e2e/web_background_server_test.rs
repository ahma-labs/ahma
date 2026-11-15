//! SPEC R-GUI.12: the server a plain `ahma web` leaves running reports its
//! address and code to its launcher once, serves the page, and stops by
//! itself when no tab has connected for the idle timeout.
//!
//! This runs the server half directly (`--foreground --ready-pipe`, what the
//! launcher starts) so it needs no terminal and touches no clipboard
//! (`--no-copy`); how the launcher builds that command is unit tested in
//! `ahma_gui::launch`.

use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use ahma_mcp::test_utils::cli::build_binary_cached;
use ahma_mcp::test_utils::http::spawn_http_bridge;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
async fn the_background_web_server_reports_once_serves_the_page_and_stops_when_unused() {
    let bridge = spawn_http_bridge()
        .await
        .expect("an ahma server to connect to");
    let binary = build_binary_cached("ahma_bin", "ahma");

    let mut cmd = tokio::process::Command::new(&binary);
    cmd.arg("web")
        .args(["--connect", &bridge.base_url()])
        .args(["--no-open", "--no-copy", "--idle-timeout", "2"])
        .args(["--foreground", "--ready-pipe"])
        .arg(bridge.temp_dir.path())
        // The session saves its per-directory choices in its working directory:
        // keep them in the test's temporary directory, never the repository.
        .current_dir(bridge.temp_dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().expect("ahma web starts");

    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(
        TestTimeouts::get(TimeoutCategory::ProcessSpawn),
        lines.next_line(),
    )
    .await
    .expect("the ready line arrives")
    .expect("stdout is readable")
    .expect("a ready line, not end of output");
    let ready: serde_json::Value = serde_json::from_str(&line).expect("the ready line is JSON");
    let url = ready["url"].as_str().expect("a url").to_string();
    let code = ready["code"].as_str().expect("a code");
    assert!(url.starts_with("http://127.0.0.1:"), "{url}");
    assert_eq!(code.len(), 9, "two groups of four: {code}");

    let page = reqwest::Client::new()
        .get(&url)
        .timeout(TestTimeouts::get(TimeoutCategory::HttpRequest))
        .send()
        .await
        .expect("the page is served");
    assert!(page.status().is_success(), "{}", page.status());

    // Nothing more is ever written to standard output.
    let status = tokio::time::timeout(TestTimeouts::get(TimeoutCategory::Handshake), async {
        let rest = lines.next_line().await;
        (rest, child.wait().await)
    })
    .await
    .expect("the unused server stops by itself");
    assert!(
        matches!(status.0, Ok(None)),
        "one line only: {:?}",
        status.0
    );
    assert!(status.1.expect("an exit status").success());
}
