//! `ahma_command` through the real binary (SPEC R-PROC.7): the unconfined
//! ahma runs its own `doctor` and hands back the report.

use ahma_mcp::test_utils::client::ClientBuilder;
use anyhow::Result;
use rmcp::model::CallToolRequestParams;
use serde_json::json;

#[tokio::test]
async fn ahma_command_doctor_returns_the_report() -> Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let client = ClientBuilder::new()
        .working_dir(temp_dir.path())
        .build()
        .await?;

    let params = CallToolRequestParams::new("ahma_command")
        .with_arguments(json!({"command": "doctor"}).as_object().unwrap().clone());
    let result = client.call_tool(params).await?;
    let text: String = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect();

    // A developer running this suite inside ahma's own sandbox gets the
    // refusal that names the line a human runs; anywhere else, the report.
    if ahma_common::seatbelt::this_process_is_confined()
        || std::env::var_os(ahma_common::process_guard::SANDBOX_RUN_ENV).is_some()
    {
        assert!(text.contains("! ahma doctor"), "{text}");
    } else {
        assert!(
            text.contains("Nothing to fix") || text.contains("ahma doctor --fix"),
            "expected the doctor report, got: {text}"
        );
    }
    client.cancel().await?;
    Ok(())
}
