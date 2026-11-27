//! The link broker end to end, through the real `ahma hooks run-shell`
//! (SPEC R-LINK.1–R-LINK.4): a hooked command's `$BROWSER` is ahma's own
//! helper, which opens a link only once a human allowed its site, and before
//! that fails with one line naming the grant. Under a test harness the opener
//! records the address in the test home and launches nothing (R-LINK.3), so
//! no browser opens.
#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Output};

fn hooked(bin: &Path, home: &Path, ws: &Path, command: &str) -> Output {
    Command::new(bin)
        .args(["hooks", "run-shell", "--cwd"])
        .arg(ws)
        .args(["--session-id", "e2e-session", "--command", command])
        .env("AHMA_TEST_HOME", home)
        .env_remove("BROWSER")
        .env_remove("GH_BROWSER")
        .current_dir(ws)
        .output()
        .expect("run ahma hooks run-shell")
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn opened(home: &Path) -> String {
    std::fs::read_to_string(home.join(".ahma").join("opened-links.jsonl")).unwrap_or_default()
}

fn workspace() -> (tempfile::TempDir, std::path::PathBuf) {
    let ws = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(ws.path().join(".git")).unwrap();
    let path = dunce::canonicalize(ws.path()).unwrap();
    (ws, path)
}

#[test]
fn a_hooked_command_opens_a_link_only_once_allowed() {
    let bin = ahma_mcp::test_utils::cli::build_binary_cached("ahma_bin", "ahma");
    let home = tempfile::tempdir().unwrap();
    let (_ws, ws) = workspace();
    // `gh … --web` runs `$GH_BROWSER <url>`; both name the helper.
    let open =
        r#"test "$BROWSER" = "$GH_BROWSER" && "$GH_BROWSER" 'https://example.com/x?token=1'"#;

    let refused = hooked(&bin, home.path(), &ws, open);
    let said = text(&refused);
    assert!(!refused.status.success(), "no grant yet: {said}");
    assert!(said.contains("Blocked until a human approves"), "{said}");
    assert!(
        said.contains("`ahma permissions grant open-url example.com`"),
        "{said}"
    );
    assert_eq!(opened(home.path()), "", "nothing opened before a grant");

    grant(home.path(), "example.com");

    let allowed = hooked(&bin, home.path(), &ws, open);
    let said = text(&allowed);
    assert!(allowed.status.success(), "allowed: {said}");
    assert!(said.contains("ahma opened https://example.com/x"), "{said}");
    let record = opened(home.path());
    assert!(
        record.contains("\"url\":\"https://example.com/x?token=1\""),
        "{record}"
    );
}

/// What `ahma permissions grant open-url <site> --yes` writes.
fn grant(home: &Path, site: &str) {
    ahma_common::open_url::persist_open_url_allow(
        &home.join(".ahma").join("settings.toml"),
        site,
        &ahma_common::open_url::OpenUrlBinding::AllRepos,
        &ahma_common::approver::ApproverSurface::Cli,
    )
    .unwrap();
}

/// SPEC R-LINK.10: a whole-line `open <URL>` runs as `ahma open-url`, inside
/// the sandbox, and says so; the link opens through the broker like any
/// other. Before, the line ran `open` itself, which the sandbox refuses on
/// macOS.
#[test]
fn a_whole_line_open_runs_as_the_helper() {
    let bin = ahma_mcp::test_utils::cli::build_binary_cached("ahma_bin", "ahma");
    let home = tempfile::tempdir().unwrap();
    let (_ws, ws) = workspace();
    grant(home.path(), "example.com");

    let out = hooked(
        &bin,
        home.path(),
        &ws,
        "open 'https://Example.com/z?a=1&b=2'",
    );
    let said = text(&out);
    assert!(out.status.success(), "{said}");
    assert!(
        said.contains("ran as `ahma open-url 'https://example.com/z?a=1&b=2'`"),
        "{said}"
    );
    assert!(
        opened(home.path()).contains("\"url\":\"https://example.com/z?a=1&b=2\""),
        "{}",
        opened(home.path())
    );
}

/// SPEC R-LINK.6: a hooked command's refused link is asked about in Claude
/// Code's dialog when the command runs again: the hook answers `ask`, with
/// the site, the guidance and the answers, and rewrites the command to carry
/// the one-use token a yes spends.
#[test]
fn a_rerun_after_a_refusal_is_asked() {
    let bin = ahma_mcp::test_utils::cli::build_binary_cached("ahma_bin", "ahma");
    let home = tempfile::tempdir().unwrap();
    let (_ws, ws) = workspace();
    let open = r#""$BROWSER" 'https://example.com/x'"#;

    let refused = hooked(&bin, home.path(), &ws, open);
    let said = text(&refused);
    assert!(!refused.status.success(), "no grant yet: {said}");
    assert!(said.contains("re-run this command and ahma asks"), "{said}");

    let decision = pre_tool_use(&bin, home.path(), &ws, open);
    let hook = &decision["hookSpecificOutput"];
    assert_eq!(hook["permissionDecision"], "ask", "{decision}");
    let reason = hook["permissionDecisionReason"]
        .as_str()
        .unwrap_or_default();
    assert!(reason.contains("when this command last ran"), "{reason}");
    assert!(reason.contains("Site: example.com"), "{reason}");
    // In a repo, yes saves this site for it (R-LINK.6); the wider lasting
    // answers follow, each naming its binding.
    let this_repo = reason
        .find("Yes — always for example.com — this repo only")
        .unwrap_or_else(|| panic!("{reason}"));
    let all_repos = reason
        .find("`ahma permissions grant open-url example.com --all-repos`")
        .unwrap_or_else(|| panic!("{reason}"));
    assert!(this_repo < all_repos, "{reason}");
    let rewritten = hook["updatedInput"]["command"].as_str().unwrap_or_default();
    assert!(rewritten.contains("--approve-grant"), "{rewritten}");
}

/// What `ahma hooks exec --platform claude` answers for a Bash call running
/// `command` in `ws`: the hook's JSON decision.
fn pre_tool_use(bin: &Path, home: &Path, ws: &Path, command: &str) -> serde_json::Value {
    use std::io::Write as _;
    let mut child = Command::new(bin)
        .args(["hooks", "exec", "--platform", "claude", "--scope", "user"])
        .env("AHMA_HOOKS", "on")
        .env("AHMA_TEST_HOME", home)
        .env_remove("GH_HOST")
        .current_dir(ws)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn `ahma hooks exec`");
    let payload = serde_json::json!({
        "session_id": "e2e-session",
        "cwd": ws,
        "tool_name": "Bash",
        "tool_input": {"command": command},
    });
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child
        .wait_with_output()
        .expect("wait for `ahma hooks exec`");
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("a JSON decision, got {stdout:?}: {e}"))
}

/// SPEC R-LINK.6: a command whose text says it will open a link to a site
/// no grant allows (`gh … --web`) is asked about in Claude Code's dialog
/// before its first run, with no refused run first; a yes there saves the
/// site for this repo.
#[test]
fn a_first_web_link_command_is_asked_before_it_runs() {
    let bin = ahma_mcp::test_utils::cli::build_binary_cached("ahma_bin", "ahma");
    let home = tempfile::tempdir().unwrap();
    let (_ws, ws) = workspace();
    let decision = pre_tool_use(&bin, home.path(), &ws, "gh pr view 12 --web");
    let hook = &decision["hookSpecificOutput"];
    assert_eq!(hook["permissionDecision"], "ask", "{decision}");
    let reason = hook["permissionDecisionReason"]
        .as_str()
        .unwrap_or_default();
    assert!(
        reason.contains("this command opens a github.com link"),
        "{reason}"
    );
    assert!(
        reason.contains("Yes — always for github.com — this repo only"),
        "{reason}"
    );
    let rewritten = hook["updatedInput"]["command"].as_str().unwrap_or_default();
    assert!(rewritten.contains("--approve-grant"), "{rewritten}");

    // A command that opens nothing is not held.
    let plain = pre_tool_use(&bin, home.path(), &ws, "gh pr view 12");
    assert_ne!(
        plain["hookSpecificOutput"]["permissionDecision"], "ask",
        "{plain}"
    );
}

/// `cargo doc --open` runs `$BROWSER <file>`: a file is never opened and
/// never asked about, and the line tells the agent to hand the human its
/// path (SPEC R-LINK.2).
#[test]
fn a_browser_given_a_file_says_whose_it_is() {
    let bin = ahma_mcp::test_utils::cli::build_binary_cached("ahma_bin", "ahma");
    let home = tempfile::tempdir().unwrap();
    let (_ws, ws) = workspace();
    let page = ws.join("target/doc/demo/index.html");
    let out = hooked(
        &bin,
        home.path(),
        &ws,
        &format!("\"$BROWSER\" '{}'", page.display()),
    );
    let said = text(&out);
    assert!(!out.status.success(), "{said}");
    assert!(said.contains("It is a local file"), "{said}");
    assert!(said.contains(&page.display().to_string()), "{said}");
    assert_eq!(opened(home.path()), "");
}

/// SPEC R-LINK.1, R-LINK.6: on the MCP path the command's `$BROWSER` is the
/// real helper, which asks the broker; the broker asks the MCP client while
/// the command waits, and "this time" opens the link in that same run — the
/// helper prints the broker's line and the opener records the address. The
/// server is in process, its client a [`RecordingClient`] that answers
/// `elicitation/create`; the helper is the real `ahma` binary, under its
/// helper name. Before, an MCP command was served no broker.
///
/// [`RecordingClient`]: ahma_mcp::test_utils::recording_client::RecordingClient
#[tokio::test]
async fn an_mcp_command_opens_a_link_once_the_client_answers_this_time() {
    use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
    use ahma_mcp::test_utils::recording_client::RecordingClient;
    let bin = ahma_mcp::test_utils::cli::build_binary_cached("ahma_bin", "ahma");
    let home = tempfile::tempdir().unwrap();
    // SAFETY: nextest runs each test in its own process.
    unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
    let (_ws, ws) = workspace();
    let helper = ahma_mcp::links::host::helper_link_in(home.path(), &bin).unwrap();
    let links = std::sync::Arc::new(
        ahma_mcp::links::live::LiveLinks::new(
            Some(home.path().join(".ahma").join("settings.toml")),
            vec![ws.clone()],
            None,
        )
        .with_helper(helper),
    );
    let client = RecordingClient::new("link-e2e").with_elicitation(Some("once"));
    let prompts = client.elicitation().unwrap();
    let mcp =
        ahma_mcp::test_utils::in_process::create_in_process_mcp_with_links(client, &ws, links)
            .await
            .expect("in-process server");

    let sc = ahma_mcp::AhmaMcpService::build_shell_subcommand_config(
        None,
        &ahma_mcp::adapter::ExecutionMode::AsyncResultPush,
    );
    let mut args = serde_json::Map::new();
    args.insert(
        "command".into(),
        serde_json::json!(r#""$BROWSER" 'https://example.com/x?a=1'"#),
    );
    args.insert("c_flag".into(), serde_json::json!(true));
    let id = mcp
        .service
        .adapter
        .execute_async_in_dir_with_options(
            "run_terminal_command",
            ahma_mcp::shell_pool::platform_shell_program(),
            ws.to_str().unwrap(),
            ahma_mcp::adapter::AsyncExecOptions {
                id: None,
                args: Some(args),
                timeout: Some(TestTimeouts::get(TimeoutCategory::ToolCall).as_secs()),
                subcommand_config: Some(&sc),
                log_monitor_config: None,
            },
        )
        .await
        .expect("dispatch");
    let op = tokio::time::timeout(
        TestTimeouts::get(TimeoutCategory::ToolCall),
        mcp.service.operation_monitor.wait_for_operation(&id),
    )
    .await
    .expect("the command finishes")
    .expect("the operation is known");
    let out = op
        .result
        .as_ref()
        .and_then(|r| r.get("stdout"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        out.contains("ahma opened https://example.com/x?a=1"),
        "{out} {:?}",
        op.result
    );
    assert!(
        opened(home.path()).contains("\"url\":\"https://example.com/x?a=1\""),
        "{}",
        opened(home.path())
    );
    let asked = prompts.messages();
    assert_eq!(asked.len(), 1, "asked once: {asked:?}");
    assert!(asked[0].contains("Site: example.com"), "{}", asked[0]);
}
