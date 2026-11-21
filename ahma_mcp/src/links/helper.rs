//! The link helper, inside the sandbox (SPEC R-LINK.1).
//!
//! A sandboxed command reaches it two ways: `$BROWSER` / `$GH_BROWSER` (a
//! link named [`HELPER_NAME`] to ahma's own binary, so a tool that runs
//! `$BROWSER <url>` with no shell and no extra words, as `cargo doc --open`
//! does, still reaches it) and `ahma open-url <URL>…` by name. It opens
//! nothing and decides nothing: it forwards each argument to the broker on
//! [`SOCKET_ENV`] and prints the broker's one line. Sync and std-only: a
//! short-lived process with a request or two, dispatched in `main` before any
//! runtime or CLI parsing ([`dispatch`]).

use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

/// The file name that selects the helper when ahma is run under it.
pub const HELPER_NAME: &str = "ahma-open-url";

/// The subcommand that selects the helper by name: `ahma open-url <URL>…`.
pub const SUBCOMMAND: &str = "open-url";

/// The broker socket of this command (INTERNAL, docs/environment-variables.md).
pub const SOCKET_ENV: &str = "AHMA_LINK_SOCK";

/// Protocol version. A broker refuses a version it does not know.
pub const PROTOCOL: u32 = 1;

/// The most a request line may hold: the URL limit plus framing.
pub const MAX_REQUEST_BYTES: usize = super::policy::MAX_URL_BYTES + 256;

/// The most a reply line may hold.
const MAX_REPLY_BYTES: u64 = 64 * 1024;

/// One request: the argument as the tool passed it. The broker checks it;
/// the helper checks nothing, so there is one checker (SPEC R-LINK.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkRequest {
    pub v: u32,
    pub url: String,
}

/// The broker's answer: whether it opened, and the one line to print.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkReply {
    pub opened: bool,
    pub line: String,
}

/// Whether this process is the helper by its name (`argv[0]`'s file name).
pub fn invoked_as_helper(argv0: &std::ffi::OsStr) -> bool {
    Path::new(argv0)
        .file_name()
        .is_some_and(|n| n == HELPER_NAME)
}

/// The helper's exit status when `argv` selects it, `None` otherwise: called
/// first in `main`, before the runtime starts and before clap, as the
/// `askpass` and Windows launcher markers are. Selected by `argv[0]`'s file
/// name, or by `open-url` as the first argument unless `--help`/`-h` asks
/// for clap's help text.
pub fn dispatch(argv: &[OsString]) -> Option<i32> {
    let words = |rest: &[OsString]| -> Vec<String> {
        rest.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    };
    if argv.first().is_some_and(|a| invoked_as_helper(a)) {
        return Some(run(&words(&argv[1..]), socket_from_env()));
    }
    if argv.get(1).is_some_and(|a| a == SUBCOMMAND) {
        let rest = words(&argv[2..]);
        if rest.iter().any(|a| a == "--help" || a == "-h") {
            return None;
        }
        return Some(run(&rest, socket_from_env()));
    }
    None
}

/// The broker socket this command was given, if any.
pub fn socket_from_env() -> Option<PathBuf> {
    std::env::var_os(SOCKET_ENV)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Run the helper for `args` (everything after `argv[0]`, or after
/// `open-url`): one link per argument, in order, stopping at the first that
/// was not opened. Returns the exit status: 0 when every link opened, 1
/// otherwise, so `gh` and `cargo` report the failure under the line printed.
pub fn run(args: &[String], socket: Option<PathBuf>) -> i32 {
    if args.is_empty() {
        eprintln!("usage: ahma open-url <URL>…");
        return 2;
    }
    let Some(socket) = socket else {
        eprintln!(
            "ahma open-url works inside a command ahma runs on macOS or Linux; here no ahma \
             link broker is reachable, so nothing was opened. You may show the human the \
             address: {}",
            args[0]
        );
        return 1;
    };
    for url in args {
        match ask(&socket, url) {
            Ok(reply) => {
                eprintln!("{}", reply.line);
                if !reply.opened {
                    return 1;
                }
            }
            Err(e) => {
                eprintln!(
                    "ahma: the link broker did not answer ({e}); nothing was opened. You may \
                     show the human the address: {url}"
                );
                return 1;
            }
        }
    }
    0
}

/// Send one request on `socket` and read the one reply line.
#[cfg(unix)]
pub fn ask(socket: &Path, url: &str) -> std::io::Result<LinkReply> {
    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    let mut line = serde_json::to_vec(&LinkRequest {
        v: PROTOCOL,
        url: url.to_string(),
    })?;
    line.push(b'\n');
    stream.write_all(&line)?;
    stream.shutdown(std::net::Shutdown::Write)?;
    // No read timeout: on the MCP path the broker may wait for a human
    // (SPEC R-LINK.6), and that wait is bounded there; the command's own time
    // limit bounds the rest.
    let mut reply = String::new();
    BufReader::new(stream)
        .take(MAX_REPLY_BYTES)
        .read_line(&mut reply)?;
    serde_json::from_str(&reply).map_err(std::io::Error::other)
}

/// No link broker serves Windows yet (SPEC R-LINK.12).
#[cfg(not(unix))]
pub fn ask(_socket: &Path, _url: &str) -> std::io::Result<LinkReply> {
    Err(std::io::Error::other(
        "no link broker serves this platform (SPEC R-LINK.12)",
    ))
}

/// Read one request from a broker connection: at most
/// [`MAX_REQUEST_BYTES`], one line, the known version.
pub fn parse_request(line: &[u8]) -> Result<LinkRequest, String> {
    if line.len() > MAX_REQUEST_BYTES {
        return Err(format!("a request of {} bytes is too long", line.len()));
    }
    let req: LinkRequest =
        serde_json::from_slice(line).map_err(|e| format!("not a link request: {e}"))?;
    if req.v != PROTOCOL {
        return Err(format!("protocol {} is not {PROTOCOL}", req.v));
    }
    Ok(req)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn the_helper_is_chosen_by_its_name_only() {
        assert!(invoked_as_helper(
            "/run/user/501/ahma/agent/bin-0123456789ab/ahma-open-url".as_ref()
        ));
        assert!(invoked_as_helper(HELPER_NAME.as_ref()));
        assert!(!invoked_as_helper("/usr/local/bin/ahma".as_ref()));
        assert!(!invoked_as_helper("ahma-open-url-x".as_ref()));
    }

    /// `ahma open-url --help` is clap's (R-DOC.7); every other `open-url`
    /// invocation, and any under the helper's name, is the helper's, before
    /// clap could parse a URL as a flag.
    #[test]
    fn open_url_is_dispatched_before_clap_except_for_help() {
        let argv = |a: &[&str]| a.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(dispatch(&argv(&["ahma", "open-url", "--help"])), None);
        assert_eq!(dispatch(&argv(&["ahma", "ps"])), None);
        assert_eq!(dispatch(&argv(&["ahma"])), None);
        // No URL: usage, whatever the socket.
        assert_eq!(dispatch(&argv(&["ahma", "open-url"])), Some(2));
        assert_eq!(dispatch(&argv(&["/x/ahma-open-url"])), Some(2));
    }

    #[test]
    fn a_request_and_its_reply_round_trip_over_a_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("l.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).unwrap();
            let req = parse_request(buf.trim_ascii_end()).unwrap();
            let reply = LinkReply {
                opened: false,
                line: format!("refused {}", req.url),
            };
            let mut out = serde_json::to_vec(&reply).unwrap();
            out.push(b'\n');
            s.write_all(&out).unwrap();
        });
        let reply = ask(&path, "https://example.com/").unwrap();
        server.join().unwrap();
        assert_eq!(
            reply,
            LinkReply {
                opened: false,
                line: "refused https://example.com/".into()
            }
        );
    }

    #[test]
    fn a_request_the_broker_does_not_know_is_refused() {
        assert!(parse_request(br#"{"v":2,"url":"https://x/"}"#).is_err());
        assert!(parse_request(br#"{"v":1,"url":"https://x/","app":"Safari"}"#).is_err());
        assert!(parse_request(&vec![b'a'; MAX_REQUEST_BYTES + 1]).is_err());
        assert_eq!(
            parse_request(br#"{"v":1,"url":"https://x/"}"#).unwrap().url,
            "https://x/"
        );
    }

    #[test]
    fn without_a_broker_the_helper_says_so_and_fails() {
        assert_eq!(run(&["https://example.com/".into()], None), 1);
    }
}
