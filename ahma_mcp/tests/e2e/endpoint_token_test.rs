//! The endpoints a sandboxed command can reach authenticate it, end to end
//! (SPEC R-HUB.13, R-HUB.14).
//!
//! Real processes, because what is under test is what a client of the socket
//! sees: a `connect(2)` succeeds, and the first request decides.

use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use ahma_mcp::test_utils::cli::{build_binary_cached, test_command};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A private, owner-only directory for one test's socket (SPEC R-ISO.1).
fn private_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod 700");
    }
    dir
}

fn spawn(binary: &Path, args: &[&str], log: &Path) -> std::process::Child {
    let mut cmd = test_command(binary);
    cmd.args(args);
    cmd.arg("--log-to-stderr");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::from(std::fs::File::create(log).expect("log file")));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Die with the test rather than outliving it as an orphan.
        cmd.process_group(0);
    }
    cmd.spawn().expect("spawn ahma")
}

async fn wait_for_path(path: &Path) -> bool {
    let deadline = std::time::Instant::now() + TestTimeouts::get(TimeoutCategory::ProcessSpawn);
    while std::time::Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        tokio::time::sleep(TestTimeouts::poll_interval()).await;
    }
    false
}

/// One raw request to the socket; the status line of the answer.
async fn status_line(socket: &Path, request_line: &str, authorization: Option<&str>) -> String {
    let mut stream = ahma_common::local_socket::LocalStream::connect(socket)
        .await
        .expect("a connect to the socket succeeds: that is the premise");
    let auth = authorization
        .map(|value| format!("Authorization: {value}\r\n"))
        .unwrap_or_default();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let request = format!(
        "{request_line} HTTP/1.0\r\nHost: localhost\r\n{auth}Content-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    let _ = tokio::time::timeout(
        TestTimeouts::get(TimeoutCategory::HttpRequest),
        stream.read_to_string(&mut response),
    )
    .await;
    response.lines().next().unwrap_or_default().to_string()
}

fn kill(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn tail(path: PathBuf) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// The escape this closes: the hub turns a session URL's query into the flags
/// of the worker it spawns, `no_sandbox` among them. A sandboxed command can
/// connect to the socket, so without the token it could ask for an unconfined
/// worker in one request. It gets a 401 before anything is spawned — whatever
/// the query asks for — and so does every other route but `/health`.
#[tokio::test]
async fn a_session_without_the_hubs_token_cannot_ask_for_no_sandbox() {
    let binary = build_binary_cached("ahma_bin", "ahma");
    let dir = private_dir();
    let sock = dir.path().join("hub.sock");
    let log = dir.path().join("hub.log");
    let mut hub = spawn(
        &binary,
        &[
            "hub",
            "--unix-socket-path",
            &sock.to_string_lossy(),
            "--idle-timeout",
            "30",
        ],
        &log,
    );
    if !wait_for_path(&sock).await {
        let said = tail(log);
        kill(&mut hub);
        panic!("the hub never bound its socket; it said:\n{said}");
    }

    for request_line in [
        "POST /mcp?no_sandbox",
        "POST /mcp?sandbox_scope=/",
        "POST /mcp",
        "POST /restart",
        "POST /restart?mode=drain",
    ] {
        let without = status_line(&sock, request_line, None).await;
        let wrong = status_line(&sock, request_line, Some("Bearer not-the-token")).await;
        assert!(
            without.contains(" 401"),
            "{request_line} with no token: {without}"
        );
        assert!(
            wrong.contains(" 401"),
            "{request_line} with a wrong token: {wrong}"
        );
    }
    let health = status_line(&sock, "GET /health", None).await;
    assert!(health.contains(" 200"), "/health stays open: {health}");

    // The token the hub published is the credential: a request that carries it
    // is routed (an unknown path is a 404, not a refusal).
    let authorization = ahma_common::hub::authorization_header_for(&sock)
        .await
        .expect("the hub published its token beside its socket");
    let routed = status_line(&sock, "GET /nope", Some(&authorization)).await;
    assert!(routed.contains(" 404"), "{routed}");
    assert!(
        ahma_common::hub::list_instances_at(&sock).await.is_ok(),
        "a client that reads the token is served"
    );
    kill(&mut hub);
}

/// An operator-started `ahma serve unix` makes its own token and requires it
/// (SPEC R-HUB.14): ahma's own clients find it beside the socket and need no
/// configuration, and a command that merely connects does not have it.
#[tokio::test]
async fn serve_unix_requires_the_token_it_publishes_beside_its_socket() {
    let binary = build_binary_cached("ahma_bin", "ahma");
    let dir = private_dir();
    let scope = dir.path().join("scope");
    std::fs::create_dir(&scope).unwrap();
    let sock = dir.path().join("mcp.sock");
    let log = dir.path().join("serve.log");
    let mut server = spawn(
        &binary,
        &[
            "--sandbox-scope",
            &scope.to_string_lossy(),
            "serve",
            "unix",
            "--unix-socket-path",
            &sock.to_string_lossy(),
        ],
        &log,
    );
    if !wait_for_path(&sock).await {
        let said = tail(log);
        kill(&mut server);
        panic!("serve unix never bound its socket; it said:\n{said}");
    }

    let without = status_line(&sock, "POST /mcp", None).await;
    assert!(without.contains(" 401"), "{without}");
    let health = status_line(&sock, "GET /health", None).await;
    assert!(health.contains(" 200"), "/health stays open: {health}");

    let token_file = ahma_common::hub::token_path_for(&sock);
    assert!(token_file.exists(), "published beside the socket");
    let authorization = ahma_common::hub::authorization_header_for(&sock)
        .await
        .expect("a client finds it");
    let routed = status_line(&sock, "GET /nope", Some(&authorization)).await;
    assert!(routed.contains(" 404"), "{routed}");

    kill(&mut server);
}
