//! End-to-end coverage for `notifications/progress` — what a *client* receives.
//!
//! Everything here was previously only checked from the server's side: the
//! router's target map was unit-tested, and no test ever looked at the wire.
//! That is a poor place to stop, because the bug these rules exist to prevent
//! is invisible from inside. Progress used to be pushed under the token of the
//! `tools/call` that *started* an operation. That bookkeeping is self-consistent
//! — the router has a target, the forwarder sends to it, every internal
//! assertion passes — while the client, blocked in `await` on a *different*
//! request, receives nothing for 85 seconds and drops the transport.
//!
//! So these tests assert on delivery and, above all, on *addressing*: which
//! request each notification was sent to (SPEC R2.5.3).
//!
//! Note that a client mints its own progress tokens — rmcp assigns one per
//! request from a per-peer counter and overwrites anything the caller set — so
//! these tests *observe* the token a request carried rather than choosing it.
//! See [`call_tool_observing_token`].

use std::collections::HashMap;
use std::time::Duration;

use ahma_common::timeouts::TestTimeouts;
use ahma_mcp::test_utils::in_process::create_in_process_mcp_with_client;
use ahma_mcp::test_utils::recording_client::{
    ProgressLog, RecordingClient, call_tool_observing_token,
};
use ahma_mcp::utils::logging::init_test_logging;
use anyhow::Result;
use rmcp::model::{CallToolRequestParams, CallToolResult, Root};
use serde_json::{Map, json};

/// A command that occupies the shell for `secs` seconds on either shell.
///
/// Deliberately **not** scaled by [`TestTimeouts`]: these durations are compared
/// against `INLINE_WINDOW_{IDLE,BUSY}_SECS`, which are wall-clock constants in
/// the server and are not scaled either. Scaling one side and not the other
/// would silently invert the relationship the test is asserting. Assertion
/// deadlines *are* scaled — see [`wait_budget`].
fn sleep_cmd(secs: u64) -> String {
    if cfg!(windows) {
        format!("Start-Sleep -Seconds {secs}")
    } else {
        format!("sleep {secs}")
    }
}

fn echo_cmd() -> &'static str {
    if cfg!(windows) {
        "Write-Output hello"
    } else {
        "echo hello"
    }
}

/// How long to wait for a notification before declaring it lost. Scaled,
/// because this is a test deadline rather than a duration under test.
fn wait_budget() -> Duration {
    TestTimeouts::scale_secs(30)
}

fn run_terminal(command: &str) -> CallToolRequestParams {
    let mut args = Map::new();
    args.insert("command".to_string(), json!(command));
    CallToolRequestParams::new("run_terminal_command").with_arguments(args)
}

fn result_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect()
}

/// Extract the `AHMA ID: op_…` an async call handed back.
fn operation_id(text: &str) -> Option<String> {
    let start = text.find("op_")?;
    let end = text[start..]
        .find(|c: char| !c.is_alphanumeric() && c != '_')
        .map(|i| start + i)
        .unwrap_or(text.len());
    Some(text[start..end].to_string())
}

fn describe(log: &ProgressLog) -> String {
    log.all()
        .iter()
        .map(|p| {
            format!(
                "  token={:?} progress={} message={:?}",
                p.progress_token,
                p.progress,
                p.message.as_deref().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The regression this whole mechanism exists for, end to end.
///
/// Three rules are observable in one run, and none of them were observable from
/// outside the server before:
///
/// 1. **The adaptive inline window (R2.6.1)** — a command that would be answered
///    inline in an idle session hands back an operation id instead while other
///    work is in flight.
/// 2. **Progress is delivered (R2.2)** — notifications reach the client's
///    `on_progress`, having gone through real serialization.
/// 3. **Progress follows the waiting request (R2.5.3)** — the terminal
///    notification is addressed to the `await`'s token, *not* to the token of
///    the long-retired `tools/call` that started the operation.
#[tokio::test]
async fn completion_progress_is_addressed_to_the_await_that_is_waiting() -> Result<()> {
    init_test_logging();
    let temp = tempfile::tempdir()?;
    let client = RecordingClient::new("claude-ai");
    let log = client.progress();
    let mcp =
        create_in_process_mcp_with_client(client, HashMap::new(), vec![temp.path().to_path_buf()])
            .await?;
    let peer = mcp.client.peer().clone();

    // Occupy the session. This command is short enough to be answered inline, so
    // the call cleans up after itself; what matters is that its operation is
    // *active* when the next call computes its window.
    let keepalive = {
        let peer = peer.clone();
        let busy_secs = ahma_mcp::constants::INLINE_WINDOW_IDLE_SECS
            .saturating_sub(6)
            .max(3);
        let params = run_terminal(&sleep_cmd(busy_secs));
        tokio::spawn(async move { peer.call_tool(params).await })
    };
    // Let the keepalive operation reach the monitor before the next call reads
    // the active count.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // (1) Busy session: this returns an operation id even though its command
    // would fit comfortably inside the idle inline window.
    let (call_token, started) = tokio::time::timeout(
        wait_budget(),
        call_tool_observing_token(&peer, run_terminal(&sleep_cmd(2))),
    )
    .await
    .expect("the call must return within the busy inline window")?;

    let text = result_text(&started);
    let op_id = operation_id(&text).unwrap_or_else(|| {
        panic!(
            "a busy session must hand back an operation id for a 2s command \
             (the idle window is {}s); got: {text:?}",
            ahma_mcp::constants::INLINE_WINDOW_IDLE_SECS
        )
    });

    // (3) Await it. This is the request the client is now blocked on, so this is
    // where liveness has to appear.
    let mut await_args = Map::new();
    await_args.insert("id".to_string(), json!(op_id));
    let (await_token, awaited) = tokio::time::timeout(
        wait_budget(),
        call_tool_observing_token(
            &peer,
            CallToolRequestParams::new("await").with_arguments(await_args),
        ),
    )
    .await
    .expect("await must return")?;
    assert_ne!(
        call_token, await_token,
        "the harness must exercise two distinct requests"
    );
    assert!(
        result_text(&awaited).to_lowercase().contains("completed"),
        "await must report completion, got: {:?}",
        result_text(&awaited)
    );

    // (2) The completion notification arrived...
    let terminal = log
        .wait_for(wait_budget(), |p| {
            p.progress >= 100.0
                && p.message
                    .as_deref()
                    .unwrap_or_default()
                    .contains(op_id.as_str())
        })
        .await
        .map_err(|seen| {
            anyhow::anyhow!(
                "no terminal progress notification for {op_id} reached the client. \
                 Received {} notification(s):\n{}",
                seen.len(),
                describe(&log)
            )
        })?;

    // ...addressed to the await, not to the call that started the operation.
    assert_eq!(
        terminal.progress_token,
        await_token,
        "the completion must be addressed to the request that was waiting for it \
         (SPEC R2.5.3); it went to {:?} instead. All notifications:\n{}",
        terminal.progress_token,
        describe(&log)
    );
    assert!(
        log.for_token(&call_token)
            .iter()
            .all(|p| p.progress < 100.0),
        "the originating request was over before the operation finished, so it \
         must not receive the completion — that is the exact defect R2.5.3 \
         exists to prevent:\n{}",
        describe(&log)
    );

    let _ = keepalive.await;
    let _ = mcp.client.cancel().await;
    Ok(())
}

/// The other half of R2.5.3: before any `await`, progress belongs to the call
/// that started the operation. Redirection has to *move* the stream, not
/// misdirect it from the start.
#[tokio::test]
async fn the_starting_notification_goes_to_the_call_that_started_it() -> Result<()> {
    init_test_logging();
    let temp = tempfile::tempdir()?;
    let client = RecordingClient::new("claude-ai");
    let log = client.progress();
    let mcp =
        create_in_process_mcp_with_client(client, HashMap::new(), vec![temp.path().to_path_buf()])
            .await?;
    let peer = mcp.client.peer().clone();

    let (call_token, result) = tokio::time::timeout(
        wait_budget(),
        call_tool_observing_token(&peer, run_terminal(echo_cmd())),
    )
    .await
    .expect("a fast command must answer inline")?;
    assert!(
        result_text(&result).contains("exit 0"),
        "a fast command must report its outcome inline (R2.6.2), got: {:?}",
        result_text(&result)
    );

    let started = log
        .wait_for(wait_budget(), |p| {
            p.progress_token == call_token && p.progress == 0.0
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "the originating call received no start notification:\n{}",
                describe(&log)
            )
        })?;
    assert!(
        started
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("run_terminal_command"),
        "the start notification must name the tool, got: {:?}",
        started.message
    );

    // Nothing was addressed anywhere else: one request, one token.
    let strays: Vec<_> = log
        .all()
        .into_iter()
        .filter(|p| p.progress_token != call_token)
        .collect();
    assert!(
        strays.is_empty(),
        "only the originating request may receive this operation's progress:\n{}",
        describe(&log)
    );

    let _ = mcp.client.cancel().await;
    Ok(())
}

/// Cursor logs errors for progress notifications even when it supplied a valid
/// token, so ahma suppresses them for that client
/// (`McpClientType::supports_progress`). That rule was only ever asserted
/// against the router's internal map; here it is asserted where it matters —
/// nothing arrives on the wire — and, just as importantly, that suppressing
/// progress does not suppress the *result*.
#[tokio::test]
async fn a_client_that_mishandles_progress_is_never_sent_any() -> Result<()> {
    init_test_logging();
    let temp = tempfile::tempdir()?;
    let client = RecordingClient::new("cursor");
    let log = client.progress();
    let mcp =
        create_in_process_mcp_with_client(client, HashMap::new(), vec![temp.path().to_path_buf()])
            .await?;

    let result = tokio::time::timeout(
        wait_budget(),
        mcp.client.call_tool(run_terminal(echo_cmd())),
    )
    .await
    .expect("call must return")?;
    assert!(
        result_text(&result).contains("exit 0"),
        "suppressing progress must not suppress the result itself, got: {:?}",
        result_text(&result)
    );

    // The client did send a token (rmcp always does), and the operation ran to
    // completion — so anything that was going to be pushed has been pushed.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        log.is_empty(),
        "Cursor supplied a progress token but must not be sent progress:\n{}",
        describe(&log)
    );

    let _ = mcp.client.cancel().await;
    Ok(())
}

/// Guard the harness itself: a silent handshake or dispatch change must not be
/// able to turn every test above into a vacuous pass.
#[tokio::test]
async fn the_recording_harness_answers_roots_and_serves_the_builtins() -> Result<()> {
    init_test_logging();
    let temp = tempfile::tempdir()?;
    let root = Root::new(format!("file://{}", temp.path().display()));
    let client = RecordingClient::new("antigravity").with_roots(vec![root]);
    let mcp =
        create_in_process_mcp_with_client(client, HashMap::new(), vec![temp.path().to_path_buf()])
            .await?;

    let tools = mcp.client.list_all_tools().await?;
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(names.contains(&"await"), "built-ins must be listed");
    assert!(names.contains(&"run_terminal_command"));

    let _ = mcp.client.cancel().await;
    Ok(())
}
