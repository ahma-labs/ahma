use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use ahma_mcp::test_utils::http::{
    HttpMcpTestClient, spawn_http_bridge, spawn_http_bridge_with_args,
};
use serde_json::json;
use tokio::time::sleep;

fn short_sleep_command() -> &'static str {
    #[cfg(windows)]
    {
        // `run_terminal_command` executes through powershell on Windows.
        "Start-Sleep -Milliseconds 200"
    }

    #[cfg(not(windows))]
    {
        "sleep 0.2"
    }
}

#[tokio::test]
async fn test_http_no_progress_token_does_not_emit_progress_notifications() -> anyhow::Result<()> {
    let server = spawn_http_bridge().await?;
    let mut client = HttpMcpTestClient::new(server.base_url());

    // run_terminal_command is a core built-in tool - no JSON config needed

    // The bridge is spawned with an explicit `--sandbox-scope` (SPEC R5.2.2), so
    // that scope commits at startup without ever querying roots/list. Use it as
    // the client's answer too, so the working directory below is inside it.
    let client_root_dir = server.temp_dir.path().to_path_buf();
    let mut events_rx = client
        .initialize_with_roots_events(vec![client_root_dir.clone()])
        .await?;

    // Wait for sandbox to lock (platform-aware retry: Windows CI is 3-5x slower).
    let sandbox_deadline =
        tokio::time::Instant::now() + TestTimeouts::get(TimeoutCategory::SandboxReady);

    // tools/call WITHOUT _meta.progressToken
    let tool_call = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "run_terminal_command",
            "arguments": {
                "command": short_sleep_command(),
                "working_directory": client_root_dir.to_string_lossy()
            }
        }
    });
    let tool_resp = loop {
        let (resp, _) = client.send_request(&tool_call).await?;
        let is_sandbox_init = resp
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64())
            == Some(-32001);
        if !is_sandbox_init {
            break resp;
        }
        if tokio::time::Instant::now() > sandbox_deadline {
            anyhow::bail!("sandbox did not become ready in time");
        }
        sleep(TestTimeouts::poll_interval()).await;
    };
    assert!(
        tool_resp.get("error").is_none(),
        "tools/call must succeed: {}",
        serde_json::to_string_pretty(&tool_resp).unwrap_or_default()
    );

    // Assert: no notifications/progress arrive within a short window.
    let deadline = tokio::time::Instant::now() + TestTimeouts::scale_secs(2);
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(ev)) =
            tokio::time::timeout(TestTimeouts::scale_millis(200), events_rx.recv()).await
        else {
            continue;
        };

        if ev.get("method").and_then(|m| m.as_str()) == Some("notifications/progress") {
            anyhow::bail!("unexpected notifications/progress without client progressToken: {ev}");
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_http_progress_token_is_echoed_in_progress_notifications() -> anyhow::Result<()> {
    let server = spawn_http_bridge().await?;
    let mut client = HttpMcpTestClient::new(server.base_url());

    // run_terminal_command is a core built-in tool - no JSON config needed

    // The bridge is spawned with an explicit `--sandbox-scope` (SPEC R5.2.2), so
    // that scope commits at startup without ever querying roots/list. Use it as
    // the client's answer too, so the working directory below is inside it.
    let client_root_dir = server.temp_dir.path().to_path_buf();
    let mut events_rx = client
        .initialize_with_roots_events(vec![client_root_dir.clone()])
        .await?;

    // Wait for sandbox to lock (platform-aware retry: Windows CI is 3-5x slower).
    let sandbox_deadline =
        tokio::time::Instant::now() + TestTimeouts::get(TimeoutCategory::SandboxReady);

    let token = "tok_http_1";
    let tool_call = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "_meta": { "progressToken": token },
            "name": "run_terminal_command",
            "arguments": {
                "command": short_sleep_command(),
                "working_directory": client_root_dir.to_string_lossy()
            }
        }
    });
    let tool_resp = loop {
        let (resp, _) = client.send_request(&tool_call).await?;
        let is_sandbox_init = resp
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64())
            == Some(-32001);
        if !is_sandbox_init {
            break resp;
        }
        if tokio::time::Instant::now() > sandbox_deadline {
            anyhow::bail!("sandbox did not become ready in time");
        }
        sleep(TestTimeouts::poll_interval()).await;
    };
    assert!(
        tool_resp.get("error").is_none(),
        "tools/call must succeed: {}",
        serde_json::to_string_pretty(&tool_resp).unwrap_or_default()
    );

    // Expect at least one notifications/progress with matching token.
    let deadline = tokio::time::Instant::now() + TestTimeouts::scale_secs(3);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(ev)) =
            tokio::time::timeout(TestTimeouts::scale_millis(500), events_rx.recv()).await
        {
            if ev.get("method").and_then(|m| m.as_str()) != Some("notifications/progress") {
                continue;
            }
            let got = ev
                .get("params")
                .and_then(|p| p.get("progressToken"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            assert_eq!(
                got, token,
                "progressToken must be echoed from request _meta"
            );
            return Ok(());
        }
    }

    anyhow::bail!("did not observe notifications/progress with token {token}");
}

/// A command that outlives the inline result window, so the call hands back an
/// operation id and there is something left for `await` to wait on.
///
/// Sized from `INLINE_WINDOW_IDLE_SECS` rather than a literal: if the window
/// moves, a hard-coded duration would silently stop producing an async
/// operation and this test would pass while covering nothing.
fn outlives_inline_window() -> String {
    let secs = ahma_mcp::constants::INLINE_WINDOW_IDLE_SECS + 2;
    if cfg!(windows) {
        format!("Start-Sleep -Seconds {secs}")
    } else {
        format!("sleep {secs}")
    }
}

/// Poll `tools/call` until the sandbox has locked, then return the response.
async fn call_once_sandbox_ready(
    client: &mut HttpMcpTestClient,
    request: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + TestTimeouts::get(TimeoutCategory::SandboxReady);
    loop {
        let (resp, _) = client.send_request(request).await?;
        let initializing = resp
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64())
            == Some(-32001);
        if !initializing {
            return Ok(resp);
        }
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!("sandbox did not become ready in time");
        }
        sleep(TestTimeouts::poll_interval()).await;
    }
}

fn result_text(response: &serde_json::Value) -> String {
    response
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

fn operation_id(text: &str) -> Option<String> {
    let start = text.find("op_")?;
    let end = text[start..]
        .find(|c: char| !c.is_alphanumeric() && c != '_')
        .map(|i| start + i)
        .unwrap_or(text.len());
    Some(text[start..end].to_string())
}

/// SPEC R2.5.3 over the HTTP bridge: the completion notification must reach the
/// SSE stream carrying the token of the `await` that is waiting, not the token
/// of the `tools/call` that started the operation.
///
/// The in-process tests cover the service's own routing. This covers the relay:
/// the bridge reads notifications off the subprocess's stdout and republishes
/// them, so a redirect that is correct inside ahma could still arrive wrong —
/// or not at all — on the wire a real client reads.
///
/// **On dual-transport coverage (R15.5):** not applicable here, and not an
/// oversight. `dispatch_subprocess_line` routes *every* notification through one
/// per-session broadcast to all SSE subscribers, independent of how the
/// originating POST negotiated its content type; the notification never travels
/// on the POST response. The relay under test is therefore the same code in both
/// modes.
#[tokio::test]
async fn test_http_await_takes_over_the_progress_stream() -> anyhow::Result<()> {
    // Needs a call that hands back an id for `await` to take over: async mode.
    // `--async` on the bridge also proves it reaches the worker.
    let server = spawn_http_bridge_with_args(&["--async"]).await?;
    let mut client = HttpMcpTestClient::new(server.base_url());

    // The bridge is spawned with an explicit `--sandbox-scope` (SPEC R5.2.2), so
    // that scope commits at startup without ever querying roots/list. Use it as
    // the client's answer too, so the working directory below is inside it.
    let client_root_dir = server.temp_dir.path().to_path_buf();
    let mut events_rx = client
        .initialize_with_roots_events(vec![client_root_dir.clone()])
        .await?;

    let call_token = "tok_originating_call";
    let await_token = "tok_awaiting_request";

    // Start an operation that outlives the inline window, so it is still running
    // when `await` arrives.
    let started = call_once_sandbox_ready(
        &mut client,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "_meta": { "progressToken": call_token },
                "name": "run_terminal_command",
                "arguments": {
                    "command": outlives_inline_window(),
                    "working_directory": client_root_dir.to_string_lossy()
                }
            }
        }),
    )
    .await?;
    assert!(
        started.get("error").is_none(),
        "tools/call must succeed: {}",
        serde_json::to_string_pretty(&started).unwrap_or_default()
    );

    let started_text = result_text(&started);
    let op_id = operation_id(&started_text).unwrap_or_else(|| {
        panic!(
            "a command outliving the {}s inline window must hand back an \
             operation id; got: {started_text:?}",
            ahma_mcp::constants::INLINE_WINDOW_IDLE_SECS
        )
    });

    // Await it under a different token. This is the request the client is now
    // blocked on, so this is where the completion has to be addressed.
    let (awaited, _) = client
        .send_request(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "_meta": { "progressToken": await_token },
                "name": "await",
                "arguments": { "id": op_id }
            }
        }))
        .await?;
    assert!(
        awaited.get("error").is_none(),
        "await must succeed: {}",
        serde_json::to_string_pretty(&awaited).unwrap_or_default()
    );

    // Drain the SSE stream and find the terminal notification for this operation.
    let mut seen: Vec<(String, f64, String)> = Vec::new();
    let deadline = tokio::time::Instant::now() + TestTimeouts::scale_secs(5);
    let mut terminal_token: Option<String> = None;
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(ev)) =
            tokio::time::timeout(TestTimeouts::scale_millis(300), events_rx.recv()).await
        else {
            if terminal_token.is_some() {
                break;
            }
            continue;
        };
        if ev.get("method").and_then(|m| m.as_str()) != Some("notifications/progress") {
            continue;
        }
        let params = ev.get("params").cloned().unwrap_or_default();
        let token = params
            .get("progressToken")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let progress = params
            .get("progress")
            .and_then(|v| v.as_f64())
            .unwrap_or(-1.0);
        let message = params
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if progress >= 100.0 && message.contains(&op_id) && terminal_token.is_none() {
            terminal_token = Some(token.clone());
        }
        seen.push((token, progress, message));
    }

    let describe = || {
        seen.iter()
            .map(|(t, p, m)| format!("  token={t} progress={p} message={}", m.replace('\n', " ")))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let terminal_token = terminal_token.unwrap_or_else(|| {
        panic!(
            "no terminal progress notification for {op_id} reached the SSE \
             stream. Received {} notification(s):\n{}",
            seen.len(),
            describe()
        )
    });

    assert_eq!(
        terminal_token,
        await_token,
        "the bridge delivered the completion to the wrong request. It must \
         carry the awaiting request's token (SPEC R2.5.3); the originating \
         tools/call was answered long before the operation finished.\n{}",
        describe()
    );
    assert!(
        !seen
            .iter()
            .any(|(t, p, m)| t == call_token && *p >= 100.0 && m.contains(&op_id)),
        "the completion must not also be pushed to the retired request's \
         token:\n{}",
        describe()
    );

    // The starting notification, by contrast, belongs to the call that started
    // the operation — the redirect must move the stream, not misdirect it from
    // the beginning.
    assert!(
        seen.iter().any(|(t, p, _)| t == call_token && *p == 0.0),
        "the originating call must have received the start notification:\n{}",
        describe()
    );

    Ok(())
}
