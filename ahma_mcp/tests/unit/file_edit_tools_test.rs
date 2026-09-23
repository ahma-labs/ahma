//! The file tools over the MCP wire: read, then edit, then patch — the order a
//! model uses them in, with the rules that make it safe (read before edit, one
//! place per edit, all or nothing).

use ahma_mcp::test_utils::in_process::{InProcessMcp, create_in_process_mcp_with_scope};
use anyhow::Result;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};

fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect()
}

async fn call(
    mcp: &InProcessMcp,
    tool: &'static str,
    args: Value,
) -> Result<CallToolResult, String> {
    let params = CallToolRequestParams::new(tool)
        .with_arguments(args.as_object().cloned().unwrap_or_default());
    mcp.client
        .call_tool(params)
        .await
        .map_err(|e| e.to_string())
}

#[tokio::test]
async fn read_edit_and_patch_over_the_wire() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = dunce::canonicalize(temp.path())?;
    let tools = root.join(".ahma");
    tokio::fs::create_dir_all(&tools).await?;
    let mcp = create_in_process_mcp_with_scope(&tools, vec![root.clone()]).await?;
    let file = root.join("lib.rs");
    tokio::fs::write(&file, "fn a() {}\nfn b() {}\nfn c() {}\n").await?;
    let path = file.to_string_lossy().to_string();

    // Editing a file this session has not read is refused.
    let err = call(
        &mcp,
        "multi_edit",
        json!({"path": path, "edits": [{"old_str": "fn a() {}", "new_str": "fn a1() {}"}]}),
    )
    .await
    .unwrap_err();
    assert!(err.contains("read_file first"), "{err}");

    let read = text(
        &call(&mcp, "read_file", json!({"path": path}))
            .await
            .unwrap(),
    );
    assert!(read.starts_with("     1\tfn a() {}"), "{read}");

    let edited = text(
        &call(
            &mcp,
            "multi_edit",
            json!({"path": path, "edits": [
                {"old_str": "fn a() {}", "new_str": "fn a1() {}"},
                {"old_str": "fn c() {}", "new_str": "fn c1() {}"}
            ]}),
        )
        .await
        .unwrap(),
    );
    assert!(edited.contains("2 replacements"), "{edited}");
    assert!(
        edited.contains("fn a1() {}"),
        "the result shows the edited lines: {edited}"
    );

    // Our own edit refreshed the stamp: a follow-up patch needs no re-read.
    let patch = "*** Begin Patch\n*** Update File: lib.rs\n-fn b() {}\n+fn b1() {}\n*** Add File: new.rs\n+pub fn n() {}\n*** End Patch";
    let applied = text(
        &call(
            &mcp,
            "apply_patch",
            json!({"patch": patch, "base_dir": root.to_string_lossy()}),
        )
        .await
        .unwrap(),
    );
    assert!(
        applied.contains("M lib.rs") && applied.contains("A new.rs"),
        "{applied}"
    );
    assert_eq!(
        tokio::fs::read_to_string(&file).await?,
        "fn a1() {}\nfn b1() {}\nfn c1() {}\n"
    );
    Ok(())
}
