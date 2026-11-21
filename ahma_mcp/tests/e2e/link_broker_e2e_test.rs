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
    use std::io::Write as _;
    let bin = ahma_mcp::test_utils::cli::build_binary_cached("ahma_bin", "ahma");
    let home = tempfile::tempdir().unwrap();
    let (_ws, ws) = workspace();
    let open = r#""$BROWSER" 'https://example.com/x'"#;

    let refused = hooked(&bin, home.path(), &ws, open);
    let said = text(&refused);
    assert!(!refused.status.success(), "no grant yet: {said}");
    assert!(said.contains("re-run this command and ahma asks"), "{said}");

    let mut child = Command::new(&bin)
        .args(["hooks", "exec", "--platform", "claude", "--scope", "user"])
        .env("AHMA_HOOKS", "on")
        .env("AHMA_TEST_HOME", home.path())
        .current_dir(&ws)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn `ahma hooks exec`");
    let payload = serde_json::json!({
        "session_id": "e2e-session",
        "cwd": ws,
        "tool_name": "Bash",
        "tool_input": {"command": open},
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
    let decision: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("a JSON decision, got {stdout:?}: {e}"));
    let hook = &decision["hookSpecificOutput"];
    assert_eq!(hook["permissionDecision"], "ask", "{stdout}");
    let reason = hook["permissionDecisionReason"]
        .as_str()
        .unwrap_or_default();
    assert!(reason.contains("Site: example.com"), "{reason}");
    assert!(reason.contains("Yes — this time"), "{reason}");
    assert!(
        reason.contains("`ahma permissions grant open-url example.com`"),
        "{reason}"
    );
    let rewritten = hook["updatedInput"]["command"].as_str().unwrap_or_default();
    assert!(rewritten.contains("--approve-grant"), "{rewritten}");
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
