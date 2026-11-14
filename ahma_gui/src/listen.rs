//! The sockets (SPEC R-GUI.1).
//!
//! `127.0.0.1` and, where it exists, `[::1]`, on one port, and nothing else.
//! There is no address to choose: a listener on every interface would be
//! reachable from the network and from anything that can route to this machine,
//! and the way to reach it from elsewhere is an SSH port-forward to loopback.
//! `localhost` can resolve to either family, so both are served: otherwise a
//! browser could reach the other one on this port, where a command in the
//! sandbox may be listening.

use anyhow::{Context, Result};
use axum::Router;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use tokio::net::TcpListener;
use tokio::sync::watch;

/// The loopback listeners for `port` (0 picks a free one), and the port.
///
/// IPv4 is bound first and decides the port; IPv6 follows on the same one and is
/// skipped, not failed, where the machine has none. A port that is taken fails
/// closed: this does not look for another.
pub async fn bind(port: u16) -> Result<(Vec<TcpListener>, u16)> {
    let v4 = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
        .await
        .with_context(|| format!("cannot listen on 127.0.0.1:{port}"))?;
    let port = v4.local_addr()?.port();
    let mut listeners = vec![v4];
    match TcpListener::bind(SocketAddr::from((Ipv6Addr::LOCALHOST, port))).await {
        Ok(v6) => listeners.push(v6),
        Err(e) => tracing::debug!("gui: no IPv6 loopback listener on port {port}: {e}"),
    }
    Ok((listeners, port))
}

/// Serve `app` on every listener until `shutdown` changes. The peer address
/// reaches the guard, which refuses anything not on this machine.
pub async fn serve(listeners: Vec<TcpListener>, app: Router, shutdown: watch::Receiver<bool>) {
    let mut tasks = tokio::task::JoinSet::new();
    for listener in listeners {
        let app = app
            .clone()
            .into_make_service_with_connect_info::<SocketAddr>();
        let mut stop = shutdown.clone();
        tasks.spawn(async move {
            let result = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = stop.changed().await;
                })
                .await;
            if let Err(e) = result {
                tracing::warn!("gui: a listener stopped: {e}");
            }
        });
    }
    while tasks.join_next().await.is_some() {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn raw(addr: SocketAddr, request: &str) -> String {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut out = String::new();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream.read_to_string(&mut out),
        )
        .await;
        out
    }

    /// Only loopback is ever bound, on one port for both families, and a taken
    /// port is an error, not a quiet move to another one.
    #[tokio::test]
    async fn only_loopback_is_bound_on_one_port_and_a_taken_port_fails() {
        let (listeners, port) = bind(0).await.unwrap();
        assert!(!listeners.is_empty());
        for l in &listeners {
            let addr = l.local_addr().unwrap();
            assert!(addr.ip().is_loopback(), "{addr}");
            assert_eq!(addr.port(), port, "both families share the port");
        }
        assert!(
            bind(port).await.is_err(),
            "a port that is taken fails closed"
        );
    }

    /// Over a real socket: the page is served under this server's own names, and
    /// a request that names this machine by another name — a rebinding page —
    /// is refused before anything is read, with the policy on the refusal.
    #[tokio::test]
    async fn over_a_real_socket_the_page_is_served_and_a_foreign_host_is_refused() {
        let (listeners, port) = bind(0).await.unwrap();
        let (stop_tx, stop_rx) = watch::channel(false);
        let app = Router::new()
            .route("/", axum::routing::get(|| async { "page" }))
            .layer(axum::middleware::from_fn(crate::guard::guard))
            .layer(axum::middleware::from_fn(crate::headers::security_headers));
        let addrs: Vec<SocketAddr> = listeners.iter().map(|l| l.local_addr().unwrap()).collect();
        let server = tokio::spawn(serve(listeners, app, stop_rx));

        for addr in &addrs {
            let ok = raw(
                *addr,
                &format!("GET / HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n"),
            )
            .await;
            assert!(ok.starts_with("HTTP/1.1 200"), "{addr}: {ok}");
            assert!(ok.contains("content-security-policy"), "{ok}");

            let rebound = raw(
                *addr,
                &format!(
                    "GET / HTTP/1.1\r\nHost: rebound.example:{port}\r\nConnection: close\r\n\r\n"
                ),
            )
            .await;
            assert!(rebound.starts_with("HTTP/1.1 403"), "{addr}: {rebound}");
            assert!(rebound.contains("forbidden_host"), "{rebound}");
            assert!(
                rebound.contains("content-security-policy"),
                "the refusal carries the policy"
            );
            assert!(
                !rebound.contains("\r\n\r\npage"),
                "nothing of the page leaked: {rebound}"
            );

            let no_host = raw(*addr, "GET / HTTP/1.0\r\n\r\n").await;
            assert!(
                no_host.starts_with("HTTP/1.0 403") || no_host.starts_with("HTTP/1.1 403"),
                "{no_host}"
            );
        }

        stop_tx.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("the server stops when told to")
            .unwrap();
    }
}
