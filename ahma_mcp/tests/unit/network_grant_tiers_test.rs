//! `network_grant` saves only what the human chose to save (SPEC R-WEB.16.11,
//! R-PERM.2). REGRESSION: every allow answer — "session" and "once" included —
//! was appended to `[network].allow` and kept forever, although the prompt
//! itself said "'session' for this session only".

use ahma_mcp::test_utils::in_process::create_in_process_mcp_with_broker;
use ahma_mcp::test_utils::recording_client::RecordingClient;
use rmcp::model::CallToolRequestParams;
use serde_json::json;
use tempfile::TempDir;

async fn grant_with_answer(answer: Option<&str>) -> (String, String) {
    let home = TempDir::new().unwrap();
    // SAFETY: nextest runs each test in its own process.
    unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
    let workspace = TempDir::new().unwrap();
    let client = RecordingClient::new("claude-code").with_elicitation(answer);
    let (mcp, _broker) = create_in_process_mcp_with_broker(client, workspace.path(), None)
        .await
        .unwrap();
    let args = json!({"host": "crates.io", "confirm": true});
    let result = mcp
        .client
        .call_tool(
            CallToolRequestParams::new("network_grant")
                .with_arguments(args.as_object().unwrap().clone()),
        )
        .await
        .expect("network_grant answers");
    let text = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<String>();
    let saved = std::fs::read_to_string(home.path().join(".ahma").join("settings.toml"))
        .unwrap_or_default();
    (text, saved)
}

#[tokio::test]
async fn a_session_answer_is_not_saved() {
    let (text, saved) = grant_with_answer(Some("session")).await;
    assert!(
        !saved.contains("crates.io"),
        "a session answer is never written: {saved}"
    );
    assert!(text.contains("this session only"), "{text}");
}

#[tokio::test]
async fn an_always_answer_is_saved() {
    let (text, saved) = grant_with_answer(Some("always")).await;
    assert!(saved.contains("crates.io"), "{saved}");
    assert!(!text.contains("this session only"), "{text}");
}

#[tokio::test]
async fn a_decline_does_not_invite_asking_again() {
    let (text, saved) = grant_with_answer(None).await;
    assert!(!saved.contains("crates.io"));
    assert!(text.contains("declined"), "{text}");
    assert!(
        !text.contains("to prompt again"),
        "a no is an answer, not an invitation to ask again: {text}"
    );
}
