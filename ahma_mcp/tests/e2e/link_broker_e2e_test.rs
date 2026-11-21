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

    // What `ahma permissions grant open-url example.com --yes` writes.
    ahma_common::open_url::persist_open_url_allow(
        &home.path().join(".ahma").join("settings.toml"),
        "example.com",
        &ahma_common::open_url::OpenUrlBinding::AllRepos,
        &ahma_common::approver::ApproverSurface::Cli,
    )
    .unwrap();

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
