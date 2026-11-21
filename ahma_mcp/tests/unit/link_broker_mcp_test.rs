//! The link broker on the MCP path (SPEC R-LINK.1, R-LINK.6, R-LINK.10):
//! every command ahma runs for an MCP client — asynchronous, PTY, either
//! lane — gets its own broker, and an ungranted link is asked in the
//! client's own prompt while the command waits. Nobody to ask: refused with
//! one line naming `ahma permissions grant open-url <site>`.
//!
//! Questions go through a real in-process MCP peer to a
//! [`RecordingClient`] that answers `elicitation/create`, so the form, the
//! words and the answer are the wire's. The broker's own checks (the cap,
//! local names) are in `links::broker`'s tests; the helper end to end, with
//! the real `ahma` binary, in the e2e `link_broker_e2e_test`.
#![cfg(unix)]

use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use ahma_mcp::AhmaMcpService;
use ahma_mcp::adapter::{Adapter, AsyncExecOptions, ExecutionMode};
use ahma_mcp::links::broker::LinkBroker;
use ahma_mcp::links::host::{LinkBrokerFactory, LinkLease};
use ahma_mcp::links::live::LiveLinks;
use ahma_mcp::operation_monitor::{MonitorConfig, Operation, OperationMonitor};
use ahma_mcp::sandbox::{Sandbox, SandboxMode};
use ahma_mcp::shell_pool::{ShellPoolConfig, ShellPoolManager, platform_shell_program};
use ahma_mcp::test_utils::in_process::create_in_process_mcp_with_links;
use ahma_mcp::test_utils::recording_client::RecordingClient;
use parking_lot::Mutex;
use serde_json::{Map, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A private home (settings, audit, opened links, broker directory) and a
/// workspace in it.
struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("AHMA_TEST_HOME", dir.path()) };
        let home = Self { dir };
        std::fs::create_dir_all(home.ws()).unwrap();
        home
    }

    fn ws(&self) -> PathBuf {
        dunce::canonicalize(self.dir.path()).unwrap().join("repo")
    }

    fn settings(&self) -> PathBuf {
        self.dir.path().join(".ahma").join("settings.toml")
    }

    fn opened(&self) -> String {
        std::fs::read_to_string(self.dir.path().join(".ahma").join("opened-links.jsonl"))
            .unwrap_or_default()
    }

    fn audit(&self) -> String {
        std::fs::read_to_string(ahma_common::permissions::audit_path().unwrap()).unwrap_or_default()
    }

    /// A stand-in for the link helper that records the arguments it was run
    /// with: the real helper needs the `ahma` binary (the e2e suite has it).
    fn fake_helper(&self) -> (PathBuf, PathBuf) {
        let argv = self.ws().join("argv");
        let helper = self.ws().join("fake-helper");
        std::fs::write(
            &helper,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", argv.display()),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        (helper, argv)
    }

    /// The MCP path's link brokers, with the user's settings file here.
    fn links(&self, helper: PathBuf) -> LiveLinks {
        LiveLinks::new(Some(self.settings()), vec![self.ws()], None).with_helper(helper)
    }
}

/// Hands out `inner`'s leases and keeps each broker, so a test can ask it
/// for a link while the command runs, as the helper would.
#[derive(Debug)]
struct Spy {
    inner: LiveLinks,
    brokers: Mutex<Vec<Arc<LinkBroker>>>,
}

impl LinkBrokerFactory for Spy {
    fn lease_for_command(&self, working_dir: &Path, command: Option<&str>) -> Option<LinkLease> {
        let lease = self.inner.lease_for_command(working_dir, command)?;
        self.brokers.lock().push(Arc::clone(lease.broker()));
        Some(lease)
    }

    fn helper(&self) -> Option<PathBuf> {
        self.inner.helper()
    }
}

fn adapter(ws: &Path, links: Arc<dyn LinkBrokerFactory>) -> (Adapter, Arc<OperationMonitor>) {
    let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
        TestTimeouts::get(TimeoutCategory::ToolCall),
    )));
    let sandbox = Arc::new(
        Sandbox::new(
            vec![ws.to_path_buf()],
            SandboxMode::Test,
            Default::default(),
        )
        .unwrap(),
    );
    let adapter = Adapter::new(
        monitor.clone(),
        Arc::new(ShellPoolManager::new(ShellPoolConfig::default())),
        sandbox,
    )
    .unwrap()
    .with_link_brokers(links);
    (adapter, monitor)
}

fn shell_args(command: &str) -> Map<String, serde_json::Value> {
    let mut args = Map::new();
    args.insert("command".into(), json!(command));
    args.insert("c_flag".into(), json!(true));
    args
}

/// Start `command` on the asynchronous `run_terminal_command` path.
async fn start_async(adapter: &Adapter, ws: &Path, command: &str) -> String {
    let sc = AhmaMcpService::build_shell_subcommand_config(None, &ExecutionMode::AsyncResultPush);
    adapter
        .execute_async_in_dir_with_options(
            "run_terminal_command",
            platform_shell_program(),
            ws.to_str().unwrap(),
            AsyncExecOptions {
                id: None,
                args: Some(shell_args(command)),
                timeout: Some(TestTimeouts::get(TimeoutCategory::ToolCall).as_secs()),
                subcommand_config: Some(&sc),
                log_monitor_config: None,
            },
        )
        .await
        .expect("dispatch")
}

async fn finished(monitor: &OperationMonitor, id: &str) -> Operation {
    tokio::time::timeout(
        TestTimeouts::get(TimeoutCategory::ToolCall),
        monitor.wait_for_operation(id),
    )
    .await
    .expect("the command finishes")
    .expect("the operation is known")
}

fn stdout_of(op: &Operation) -> String {
    op.result
        .as_ref()
        .and_then(|r| r.get("stdout"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Prints what the command was given, then waits until the test says `go`
/// (bounded, so a failing test cannot leave it running).
const PRINT_AND_WAIT: &str = "printf 'browser=%s gh=%s\\n' \"$BROWSER\" \"$GH_BROWSER\"; \
     [ -S \"$AHMA_LINK_SOCK\" ] && echo served; \
     i=0; while [ ! -e go ] && [ $i -lt 1200 ]; do sleep 0.05; i=$((i+1)); done";

/// SPEC R-LINK.1, R-LINK.6: an asynchronous command is served its own link
/// broker — `BROWSER` and `GH_BROWSER` name the helper, its socket is live —
/// and a link nobody can be asked about (no client attached) is refused with
/// one line naming the grant and saying the question was not put; that line
/// is an alert on the running operation, once. Before, only the synchronous
/// path was served a broker: an MCP command had no `BROWSER` from ahma.
#[tokio::test]
async fn an_async_command_link_is_refused_with_one_line_when_nobody_can_be_asked() {
    let home = Home::new();
    let ws = home.ws();
    let (helper, _) = home.fake_helper();
    let spy = Arc::new(Spy {
        inner: home.links(helper.clone()),
        brokers: Mutex::new(Vec::new()),
    });
    let (adapter, monitor) = adapter(&ws, spy.clone());
    let id = start_async(&adapter, &ws, PRINT_AND_WAIT).await;

    let leased = ahma_mcp::test_utils::concurrency::wait_for_condition(
        TestTimeouts::get(TimeoutCategory::ToolCall),
        TestTimeouts::poll_interval(),
        || {
            let leased = !spy.brokers.lock().is_empty();
            async move { leased }
        },
    )
    .await;
    if !leased {
        std::fs::write(ws.join("go"), "").unwrap();
        panic!("the async command was served no link broker");
    }
    let broker = spy.brokers.lock()[0].clone();
    // What the helper asks for once the command runs `$BROWSER <url>`.
    let reply = broker.handle("https://github.com/o/r/pull/1").await;
    std::fs::write(ws.join("go"), "").unwrap();
    assert!(!reply.opened, "{}", reply.line);
    assert!(reply.line.contains("could not ask"), "{}", reply.line);
    assert!(
        reply
            .line
            .contains("`ahma permissions grant open-url github.com`"),
        "{}",
        reply.line
    );

    let op = finished(&monitor, &id).await;
    let out = stdout_of(&op);
    let helper = helper.display();
    assert!(
        out.contains(&format!("browser={helper} gh={helper}")),
        "{out}"
    );
    assert!(out.contains("served"), "its socket is live: {out}");
    assert_eq!(
        op.alerts.iter().filter(|a| **a == reply.line).count(),
        1,
        "the refusal is one alert: {:?}",
        op.alerts
    );
    assert_eq!(home.opened(), "", "nothing opened");
}

/// SPEC R-LINK.1: a PTY command is served its own link broker too: its
/// sandbox is built knowing it, and it gets the helper and the socket.
#[tokio::test]
async fn the_pty_path_gets_the_link_broker_too() {
    if !ahma_mcp::adapter::pty_available() {
        eprintln!("skipping: PTY allocation denied in this environment");
        return;
    }
    let home = Home::new();
    let ws = home.ws();
    let (helper, _) = home.fake_helper();
    let (adapter, monitor) = adapter(&ws, Arc::new(home.links(helper.clone())));
    let id = adapter
        .execute_pty_async(
            "run_terminal_command",
            "printf 'browser=%s gh=%s\\n' \"$BROWSER\" \"$GH_BROWSER\"; \
             [ -S \"$AHMA_LINK_SOCK\" ] && echo served",
            ws.to_str().unwrap(),
            Some(TestTimeouts::get(TimeoutCategory::ToolCall).as_secs()),
            None,
        )
        .await
        .unwrap();
    let out = stdout_of(&finished(&monitor, &id).await);
    let helper = helper.display();
    assert!(
        out.contains(&format!("browser={helper} gh={helper}")),
        "{out}"
    );
    assert!(out.contains("served"), "its socket is live: {out}");
}

/// SPEC R-LINK.10: on the asynchronous and PTY paths a whole-line
/// `open <URL>` runs as the link helper, given the checked address, and the
/// operation says so in one alert; the operation keeps the line as written.
/// Before, it ran `open` itself there, which the sandbox refuses.
#[tokio::test]
async fn a_whole_line_open_runs_as_the_helper_on_the_async_and_pty_paths() {
    let home = Home::new();
    let ws = home.ws();
    let (helper, argv) = home.fake_helper();
    let (adapter, monitor) = adapter(&ws, Arc::new(home.links(helper)));
    let line = "open 'HTTPS://Example.com/z?a=1&b=2'";
    let said = "ahma: `open 'HTTPS://Example.com/z?a=1&b=2'` ran as `ahma open-url \
                'https://example.com/z?a=1&b=2'` (SPEC R-LINK.10).";

    let id = start_async(&adapter, &ws, line).await;
    let op = finished(&monitor, &id).await;
    assert!(op.alerts.iter().any(|a| a == said), "{:?}", op.alerts);
    assert_eq!(op.command.as_deref(), Some(line), "kept as written");
    assert_eq!(
        std::fs::read_to_string(&argv).unwrap(),
        "https://example.com/z?a=1&b=2\n",
        "one argument, the address as checked"
    );

    if !ahma_mcp::adapter::pty_available() {
        eprintln!("skipping the PTY half: PTY allocation denied in this environment");
        return;
    }
    std::fs::remove_file(&argv).unwrap();
    let id = adapter
        .execute_pty_async(
            "run_terminal_command",
            line,
            ws.to_str().unwrap(),
            Some(TestTimeouts::get(TimeoutCategory::ToolCall).as_secs()),
            None,
        )
        .await
        .unwrap();
    let op = finished(&monitor, &id).await;
    assert!(op.alerts.iter().any(|a| a == said), "{:?}", op.alerts);
    assert_eq!(
        std::fs::read_to_string(&argv).unwrap(),
        "https://example.com/z?a=1&b=2\n"
    );
}

/// An in-process server whose commands get the MCP path's link brokers,
/// asking `client`; and one command's lease.
async fn served(
    home: &Home,
    client: RecordingClient,
) -> (
    ahma_mcp::test_utils::in_process::InProcessMcp<RecordingClient>,
    LinkLease,
) {
    let (helper, _) = home.fake_helper();
    let links = Arc::new(home.links(helper));
    let mcp = create_in_process_mcp_with_links(client, &home.ws(), links.clone())
        .await
        .expect("in-process server");
    let lease = links
        .lease_for_command(&home.ws(), Some("gh pr view 1 --web"))
        .expect("a lease");
    (mcp, lease)
}

/// SPEC R-LINK.6, R-LINK.7, R-LINK.9: the MCP client is asked while the
/// command waits, with the site, the address and the guidance, offering deny
/// first; "this time" opens this link now, is audited as a once grant
/// answered in the agent's own prompt, saves nothing, and the next link to
/// the site is asked again.
#[tokio::test]
async fn a_link_opens_this_time_when_the_client_answers_once() {
    let home = Home::new();
    let client = RecordingClient::new("link-test").with_elicitation(Some("once"));
    let prompts = client.elicitation().unwrap();
    let (_mcp, lease) = served(&home, client).await;

    let reply = lease.broker().handle("https://example.com/x?token=1").await;
    assert!(reply.opened, "{}", reply.line);
    assert!(
        home.opened()
            .contains("\"url\":\"https://example.com/x?token=1\""),
        "{}",
        home.opened()
    );
    let asked = prompts.messages();
    assert_eq!(asked.len(), 1);
    assert!(asked[0].contains("Site: example.com"), "{}", asked[0]);
    assert!(
        asked[0].contains("https://example.com/x?token=1"),
        "{}",
        asked[0]
    );
    assert!(
        asked[0].contains(ahma_common::open_url::GUIDANCE),
        "{}",
        asked[0]
    );
    let audit = home.audit();
    assert!(
        audit.contains("\"once\"") && audit.contains("\"harness\""),
        "{audit}"
    );
    assert!(!home.settings().exists(), "nothing saved");

    assert!(lease.broker().handle("https://example.com/y").await.opened);
    assert_eq!(prompts.messages().len(), 2, "a yes was for one link");
}

/// SPEC R-LINK.6, R-LINK.4, R-LINK.9: "always for this site — all repos"
/// saves the site for every repo, audited with the harness surface, and the
/// site is not asked about again.
#[tokio::test]
async fn always_for_this_host_is_saved_and_audited() {
    let home = Home::new();
    let client = RecordingClient::new("link-test").with_elicitation(Some("always-site"));
    let prompts = client.elicitation().unwrap();
    let (_mcp, lease) = served(&home, client).await;

    assert!(lease.broker().handle("https://github.com/o/r").await.opened);
    assert!(lease.broker().handle("https://github.com/o/s").await.opened);
    assert_eq!(prompts.messages().len(), 1, "saved, so asked once");
    let saved = ahma_common::config::AhmaSettings::load_from_result(&home.settings())
        .unwrap()
        .open_url;
    assert_eq!(saved.allow, ["github.com"]);
    let audit = home.audit();
    assert!(audit.contains("\"always\""), "{audit}");
    assert!(audit.contains("\"harness\""), "{audit}");
}

/// SPEC R-LINK.6, R5.3.1: a decline in the client is a deny, remembered for
/// the session — not asked again, and its line names only the grant — while
/// a prompt the client closed with no choice is nobody's decision: refused
/// this time, asked again next time.
#[tokio::test]
async fn a_decline_is_remembered_for_the_session_and_a_cancel_is_not() {
    let home = Home::new();
    let declines = RecordingClient::new("link-test").with_elicitation(None);
    let prompts = declines.elicitation().unwrap();
    let (_mcp, lease) = served(&home, declines).await;
    for _ in 0..2 {
        let reply = lease.broker().handle("https://github.com/o/r").await;
        assert!(!reply.opened);
        assert!(!reply.line.contains("could not ask"), "{}", reply.line);
        assert!(
            reply
                .line
                .contains("`ahma permissions grant open-url github.com`"),
            "{}",
            reply.line
        );
    }
    assert_eq!(prompts.messages().len(), 1, "declined, so asked once");

    let home = Home::new();
    let cancels = RecordingClient::new("link-test").with_elicitation_cancel();
    let prompts = cancels.elicitation().unwrap();
    let (_mcp, lease) = served(&home, cancels).await;
    for _ in 0..2 {
        let reply = lease.broker().handle("https://github.com/o/r").await;
        assert!(!reply.opened);
        assert!(
            reply.line.contains("closed without an answer"),
            "{}",
            reply.line
        );
    }
    assert_eq!(prompts.messages().len(), 2, "a dismissal decides nothing");
}

/// SPEC R-LINK.6 (rung 3), R-PERM.3.2: a client that cannot show a
/// question is not asked; the link is refused with the grant line, saying
/// the client could not be asked.
#[tokio::test]
async fn a_client_without_elicitation_is_not_asked() {
    let home = Home::new();
    let (_mcp, lease) = served(&home, RecordingClient::new("link-test")).await;
    let reply = lease.broker().handle("https://github.com/o/r").await;
    assert!(!reply.opened);
    assert!(
        reply
            .line
            .contains("could not ask: the MCP client cannot show questions"),
        "{}",
        reply.line
    );
    assert!(
        reply
            .line
            .contains("`ahma permissions grant open-url github.com`"),
        "{}",
        reply.line
    );
}
