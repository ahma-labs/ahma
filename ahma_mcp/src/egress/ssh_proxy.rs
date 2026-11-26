//! `ahma ssh-proxy`: SSH through the egress proxy (SPEC R-WEB.16.14).
//!
//! Under `--restrict-network` a command may connect to the egress proxy and
//! nothing else off this machine. SSH does not read `HTTP_PROXY`, so `git push`
//! to a `git@host:` remote could not connect at all. While restriction is on,
//! every command gets a `GIT_SSH_COMMAND` whose `ProxyCommand` is this helper:
//! it asks the proxy for a `CONNECT host:port` tunnel and relays ssh's stdin
//! and stdout through it. The proxy decides, as for any HTTPS request: an
//! allowed host is tunnelled, an unlisted one is asked about or refused with
//! the command that allows it.
//!
//! Like the link helper it is dispatched first in `main`, before any runtime,
//! settings or CLI parsing ([`dispatch`]), and is std-only: ssh starts it for
//! every connection, inside the command's sandbox, which may read nothing of
//! ahma's. Landlock lets a command run ahma's own binary (R-LINK.1), and its
//! one connection is to the proxy's port, which every sandbox allows.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};

/// The subcommand: `ahma ssh-proxy <proxy-addr> <host> <port>`.
pub const SUBCOMMAND: &str = "ssh-proxy";

/// The most a proxy's response head may hold before the helper gives up.
const MAX_HEAD_BYTES: usize = 16 * 1024;

/// The helper's exit status when `argv` selects it, `None` otherwise.
pub fn dispatch(argv: &[OsString]) -> Option<i32> {
    if argv.get(1).is_none_or(|a| a != SUBCOMMAND) {
        return None;
    }
    let args: Vec<String> = argv[2..]
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return None;
    }
    Some(run(&args))
}

fn run(args: &[String]) -> i32 {
    let [proxy, host, port] = args else {
        eprintln!(
            "usage: ahma ssh-proxy <proxy-addr> <host> <port> (ssh's ProxyCommand under \
             --restrict-network)"
        );
        return 2;
    };
    let Ok(port) = port.parse::<u16>() else {
        eprintln!("ahma ssh-proxy: {port} is not a port");
        return 2;
    };
    match open_tunnel(proxy, host, port) {
        Ok(stream) => match relay(stream) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("ahma ssh-proxy: the tunnel to {host}:{port} broke: {e}");
                1
            }
        },
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// A `host` ssh may pass: a name or an address, nothing a request line could
/// be split on. ssh already refuses most metacharacters in a host; this keeps
/// a stray `\r\n` from writing a second header.
fn plain_host(host: &str) -> bool {
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
}

/// Ask the proxy at `proxy` for a tunnel to `host:port`. Ok once it answers
/// `200`; otherwise an error carrying what the proxy said (its refusal names
/// `ahma network allow <host>`).
pub fn open_tunnel(proxy: &str, host: &str, port: u16) -> io::Result<TcpStream> {
    if !plain_host(host) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("ahma ssh-proxy: refusing a host with unexpected characters: {host:?}"),
        ));
    }
    let mut stream = TcpStream::connect(proxy).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "ahma ssh-proxy: cannot reach ahma's egress proxy at {proxy} ({e}). It runs \
                 while the ahma session that started this command does."
            ),
        )
    })?;
    write!(
        stream,
        "CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n"
    )?;
    let head = read_head(&mut stream)?;
    let status_line = head.lines().next().unwrap_or_default();
    if status_line.split_whitespace().nth(1) == Some("200") {
        return Ok(stream);
    }
    let body = read_body(&mut stream, &head);
    let said = body.trim();
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        if said.is_empty() {
            format!("ahma ssh-proxy: the egress proxy refused {host}:{port}: {status_line}")
        } else {
            said.to_string()
        },
    ))
}

/// The response head, up to and including the blank line, read a byte at a
/// time so nothing of the tunnel behind it is consumed.
fn read_head(stream: &mut TcpStream) -> io::Result<String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_HEAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ahma ssh-proxy: the egress proxy's answer has no end",
            ));
        }
        if stream.read(&mut byte)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "ahma ssh-proxy: the egress proxy closed the connection without answering",
            ));
        }
        head.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

/// A refusal's body, by its `Content-Length` (empty when it has none).
fn read_body(stream: &mut TcpStream, head: &str) -> String {
    let len = head
        .lines()
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0)
        .min(MAX_HEAD_BYTES);
    let mut body = vec![0u8; len];
    let _ = stream.read_exact(&mut body);
    String::from_utf8_lossy(&body).into_owned()
}

/// Relay stdin to the tunnel and the tunnel to stdout until the server closes.
fn relay(stream: TcpStream) -> io::Result<()> {
    let mut upstream = stream.try_clone()?;
    let writer = std::thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut upstream);
        let _ = upstream.shutdown(Shutdown::Write);
    });
    let mut downstream = stream;
    io::copy(&mut downstream, &mut io::stdout().lock())?;
    io::stdout().flush()?;
    // stdin may stay open after the server closed; the process exits anyway.
    drop(writer);
    Ok(())
}

/// The `GIT_SSH_COMMAND` that sends git's ssh through the proxy at
/// `proxy_addr`, with `ahma` at `exe` as its `ProxyCommand`. `None` when the
/// path holds a character that would need shell quoting beyond single quotes:
/// git runs the value through `sh -c`, and ssh runs `ProxyCommand` through the
/// shell again.
pub fn git_ssh_command(exe: &std::path::Path, proxy_addr: std::net::SocketAddr) -> Option<String> {
    let exe = exe.to_str()?;
    if exe
        .chars()
        .any(|c| matches!(c, '\'' | '"' | '\\' | '$' | '`' | '%') || c.is_control())
    {
        return None;
    }
    Some(format!(
        "ssh -o ProxyCommand=\"'{exe}' {SUBCOMMAND} {proxy_addr} %h %p\""
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::{EgressAllowlist, EgressProxy, EgressProxyConfig};
    use std::path::Path;

    async fn proxy(allow: &str) -> EgressProxy {
        EgressProxy::start(EgressProxyConfig {
            allowlist: EgressAllowlist::from_str(allow),
            block_private: false,
            ..Default::default()
        })
        .await
        .unwrap()
    }

    /// R-WEB.16.14: an allowed host is tunnelled, byte for byte both ways.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_allowed_host_is_tunnelled_through_the_proxy() {
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = server.accept().unwrap();
            s.write_all(b"SSH-2.0-test\r\n").unwrap();
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).unwrap();
            s.write_all(&buf).unwrap();
        });
        let proxy = proxy("localhost").await;
        let addr = proxy.local_addr.to_string();
        let got = tokio::task::spawn_blocking(move || {
            let mut tunnel = open_tunnel(&addr, "localhost", port).expect("tunnel opens");
            let mut banner = [0u8; 14];
            tunnel.read_exact(&mut banner).unwrap();
            tunnel.write_all(b"ping").unwrap();
            let mut echo = [0u8; 4];
            tunnel.read_exact(&mut echo).unwrap();
            (banner, echo)
        })
        .await
        .unwrap();
        assert_eq!(&got.0, b"SSH-2.0-test\r\n");
        assert_eq!(&got.1, b"ping");
    }

    /// An unlisted host is refused with the proxy's own words, which name the
    /// command that allows it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unlisted_host_is_refused_with_the_command_that_allows_it() {
        let proxy = proxy("allowed.example").await;
        let addr = proxy.local_addr.to_string();
        let err = tokio::task::spawn_blocking(move || {
            open_tunnel(&addr, "github.com", 22).expect_err("github.com is not allowed")
        })
        .await
        .unwrap();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            err.to_string().contains("ahma network allow github.com"),
            "{err}"
        );
    }

    #[test]
    fn a_host_that_could_split_the_request_is_refused_before_connecting() {
        // Port 9 on loopback: nothing is contacted, the check comes first.
        let err = open_tunnel("127.0.0.1:9", "evil\r\nX: y", 22).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(plain_host("github.com") && plain_host("[::1]") && plain_host("10.0.0.1"));
    }

    #[test]
    fn git_ssh_command_names_the_helper_and_the_proxy() {
        let addr: std::net::SocketAddr = "127.0.0.1:34567".parse().unwrap();
        assert_eq!(
            git_ssh_command(Path::new("/Users/me/.local/bin/ahma"), addr).as_deref(),
            Some(
                "ssh -o ProxyCommand=\"'/Users/me/.local/bin/ahma' ssh-proxy 127.0.0.1:34567 \
                 %h %p\""
            )
        );
        for unsafe_path in [
            "/a b'c/ahma",
            "/x/$HOME/ahma",
            "/x/`id`/ahma",
            "/x/100%/ahma",
        ] {
            assert_eq!(
                git_ssh_command(Path::new(unsafe_path), addr),
                None,
                "{unsafe_path}"
            );
        }
    }

    #[test]
    fn dispatch_selects_only_its_own_subcommand() {
        let argv = |words: &[&str]| words.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(dispatch(&argv(&["ahma", "serve"])), None);
        assert_eq!(dispatch(&argv(&["ahma", "ssh-proxy", "--help"])), None);
        assert_eq!(dispatch(&argv(&["ahma", "ssh-proxy", "only-one"])), Some(2));
    }
}
