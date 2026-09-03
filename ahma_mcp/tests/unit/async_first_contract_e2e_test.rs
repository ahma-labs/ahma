//! What a client can actually observe about ahma's async-first contract
//! (SPEC R2.6), asserted over the MCP wire rather than on helper functions.
//!
//! Each rule here exists because of a specific failure in a captured session
//! with Gemini Flash 3.6 driving Antigravity:
//!
//! * `cargo fmt --check` on clean code returned `{"text": ""}`. The model could
//!   not distinguish "passed" from "the tool is broken", concluded the latter,
//!   and stopped using ahma. → R2.6.2, every result states its outcome.
//! * The model sent `"sync": true` on its *first* call, before it had seen any
//!   result at all — so the parameter was invented, not inferred. ahma dropped
//!   it silently, and the empty result above then "confirmed" the theory.
//!   → R2.6.4, ignored arguments are disclosed.
//! * A synchronous mode would have been actively harmful: the same session's
//!   `cargo clippy` took 89 seconds against a client that abandons its
//!   transport in well under a minute. → R2.6.3, no caller-selectable sync.
//!
//! The helper-level unit tests for these are necessary but not sufficient: they
//! cannot catch a notice that is built correctly and then never attached to the
//! result, or a parameter removed from one schema builder and left in another.

use ahma_mcp::test_utils::in_process::create_in_process_mcp_with_scope;
use ahma_mcp::utils::logging::init_test_logging;
use anyhow::Result;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use serde_json::{Map, json};
use std::time::Duration;

use ahma_common::timeouts::TestTimeouts;

fn budget() -> Duration {
    TestTimeouts::scale_secs(30)
}

/// A command that succeeds while printing absolutely nothing — the shape of
/// `cargo fmt --check` on clean code.
fn silent_success() -> &'static str {
    if cfg!(windows) { "$null = 1" } else { "true" }
}

/// Fails with a distinctive code and prints nothing. `exit N` means the same
/// thing to both `sh` and PowerShell.
fn silent_failure() -> &'static str {
    "exit 3"
}

fn result_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect()
}

/// Build a server scoped to a fresh temp dir, with the built-in tools only.
async fn server() -> Result<(
    ahma_mcp::test_utils::in_process::InProcessMcp,
    tempfile::TempDir,
)> {
    let temp = tempfile::tempdir()?;
    let ahma_dir = temp.path().join(".ahma");
    tokio::fs::create_dir_all(&ahma_dir).await?;
    let mcp = create_in_process_mcp_with_scope(&ahma_dir, vec![temp.path().to_path_buf()]).await?;
    Ok((mcp, temp))
}

async fn run(
    mcp: &ahma_mcp::test_utils::in_process::InProcessMcp,
    args: serde_json::Value,
) -> Result<CallToolResult> {
    let params = CallToolRequestParams::new("run_terminal_command")
        .with_arguments(args.as_object().cloned().unwrap_or_default());
    Ok(tokio::time::timeout(budget(), mcp.client.call_tool(params))
        .await
        .expect("run_terminal_command must answer")?)
}

/// R2.6.2 — a silent success is still an answer.
#[tokio::test]
async fn a_command_that_prints_nothing_still_reports_its_outcome() -> Result<()> {
    init_test_logging();
    let (mcp, _temp) = server().await?;

    let text = result_text(&run(&mcp, json!({ "command": silent_success() })).await?);
    assert!(
        !text.trim().is_empty(),
        "an empty result is indistinguishable from a broken tool — that is the \
         defect R2.6.2 exists to prevent"
    );
    assert!(
        text.contains("exit 0"),
        "the result must state the outcome, got: {text:?}"
    );

    let _ = mcp.client.cancel().await;
    Ok(())
}

/// R2.6.2 — and a silent *failure* must be just as legible, with its code.
#[tokio::test]
async fn a_failure_that_prints_nothing_still_reports_its_exit_code() -> Result<()> {
    init_test_logging();
    let (mcp, _temp) = server().await?;

    let text = result_text(&run(&mcp, json!({ "command": silent_failure() })).await?);
    assert!(
        text.contains("exit 3"),
        "a failing command must report its exit code even with no output, got: {text:?}"
    );

    let _ = mcp.client.cancel().await;
    Ok(())
}

/// R2.6.4 — an invented parameter is ignored *and said so*.
#[tokio::test]
async fn an_unknown_argument_is_disclosed_in_the_result() -> Result<()> {
    init_test_logging();
    let (mcp, _temp) = server().await?;

    let text = result_text(
        &run(
            &mcp,
            json!({ "command": silent_success(), "sync": true, "wait": 5 }),
        )
        .await?,
    );

    assert!(
        text.contains("exit 0"),
        "the disclosure must be appended to a real result, not replace it: {text:?}"
    );
    assert!(
        text.contains("sync") && text.contains("wait"),
        "every ignored argument must be named, got: {text:?}"
    );
    assert!(
        text.contains("no caller-selectable synchronous mode"),
        "the disclosure must say what happens instead, so the model stops \
         reaching for the parameter; got: {text:?}"
    );

    let _ = mcp.client.cancel().await;
    Ok(())
}

/// A known argument is *not* reported as ignored — otherwise the disclosure is
/// noise and gets tuned out.
#[tokio::test]
async fn known_arguments_produce_no_disclosure() -> Result<()> {
    init_test_logging();
    let (mcp, _temp) = server().await?;

    let text = result_text(
        &run(
            &mcp,
            json!({ "command": silent_success(), "timeout_seconds": 30 }),
        )
        .await?,
    );
    assert!(
        !text.contains("ignored unknown argument"),
        "`timeout_seconds` is a real parameter and must not be reported as \
         ignored, got: {text:?}"
    );

    let _ = mcp.client.cancel().await;
    Ok(())
}

/// R2.6.3 — the schema offers the model no way to choose synchronous execution.
///
/// Asserted against the schema the client actually receives from `tools/list`,
/// because that is the only copy the model ever sees.
#[tokio::test]
async fn the_advertised_schema_offers_no_synchronous_mode() -> Result<()> {
    init_test_logging();
    let (mcp, _temp) = server().await?;

    let tools = mcp.client.list_all_tools().await?;
    let shell = tools
        .iter()
        .find(|t| &*t.name == "run_terminal_command")
        .expect("run_terminal_command must be advertised");

    let properties = shell
        .input_schema
        .get("properties")
        .and_then(|v| v.as_object())
        .expect("the schema must declare properties");

    for forbidden in [
        "sync",
        "synchronous",
        "execution_mode",
        "timeout_ms",
        "blocking",
    ] {
        assert!(
            !properties.contains_key(forbidden),
            "`{forbidden}` must not be advertised: a model given the choice takes \
             it by default, and a blocking call outlives what several clients \
             tolerate on one request (SPEC R2.6.3). Advertised: {:?}",
            properties.keys().collect::<Vec<_>>()
        );
    }

    // The parameters that *are* advertised still have to be there — a schema
    // that lost `command` would pass the loop above.
    assert!(properties.contains_key("command"));
    assert!(properties.contains_key("timeout_seconds"));

    let _ = mcp.client.cancel().await;
    Ok(())
}

/// R2.6.1 — an idle session answers a short command inline, with no operation
/// id and therefore no `await` round-trip. This is the low-context path the
/// whole design exists to protect; if it regressed to always-async, every fast
/// command would cost the model two extra turns.
#[tokio::test]
async fn an_idle_session_answers_a_fast_command_inline() -> Result<()> {
    init_test_logging();
    let (mcp, _temp) = server().await?;

    let echo = if cfg!(windows) {
        "Write-Output inline-please"
    } else {
        "echo inline-please"
    };
    let text = result_text(&run(&mcp, json!({ "command": echo })).await?);

    assert!(
        text.contains("inline-please"),
        "an idle session must return the output itself, got: {text:?}"
    );
    assert!(
        !text.contains("AHMA ID:"),
        "a command far shorter than the {}s idle window must not cost an \
         `await` round-trip, got: {text:?}",
        ahma_mcp::constants::INLINE_WINDOW_IDLE_SECS
    );

    let _ = mcp.client.cancel().await;
    Ok(())
}

/// Unknown arguments are disclosed on the *async* path too. The notice is
/// appended by a wrapper around dispatch, so a refactor that returns early from
/// one branch loses it silently — and that branch is the one a slow `cargo`
/// build takes.
#[tokio::test]
async fn the_disclosure_survives_the_async_path() -> Result<()> {
    init_test_logging();
    let (mcp, _temp) = server().await?;

    let sleep = if cfg!(windows) {
        format!(
            "Start-Sleep -Seconds {}",
            ahma_mcp::constants::INLINE_WINDOW_IDLE_SECS + 2
        )
    } else {
        format!("sleep {}", ahma_mcp::constants::INLINE_WINDOW_IDLE_SECS + 2)
    };

    let mut args = Map::new();
    args.insert("command".to_string(), json!(sleep));
    args.insert("sync".to_string(), json!(true));
    let params = CallToolRequestParams::new("run_terminal_command").with_arguments(args);
    let result = tokio::time::timeout(budget(), mcp.client.call_tool(params))
        .await
        .expect("the call must hand back an id once the window elapses")?;

    let text = result_text(&result);
    assert!(
        text.contains("AHMA ID:"),
        "a command outliving the idle window must hand back an operation id, \
         got: {text:?}"
    );
    assert!(
        text.contains("sync"),
        "the ignored argument must be disclosed on the async path too, got: {text:?}"
    );

    // Do not leave the operation running past the test.
    let mut cancel_args = Map::new();
    cancel_args.insert("all".to_string(), json!(true));
    let _ = mcp
        .client
        .call_tool(CallToolRequestParams::new("cancel").with_arguments(cancel_args))
        .await;

    let _ = mcp.client.cancel().await;
    Ok(())
}
