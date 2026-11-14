//! Who may talk to the listener at all (SPEC R-GUI.1, R-GUI.3).
//!
//! Checked on every request before any credential is looked at, so a page the
//! human happens to have open elsewhere cannot make this server do anything, or
//! even tell it apart from one that is not there:
//!
//! - the peer is on this machine;
//! - the `Host` is `127.0.0.1`, `localhost` or `[::1]` — by name, any port: a
//!   DNS-rebinding page reaches the port under a name of its own, and an SSH
//!   forward changes the port but not the name;
//! - an `Origin`, when there is one, is this very origin — not merely a loopback
//!   one: another dev server on another port is also a loopback origin, and a
//!   sandboxed command can run one;
//! - `Sec-Fetch-Site`, when the browser sends it, says `same-origin` (or `none`,
//!   for a document the human typed);
//! - the method is one this server uses.
//!
//! No CORS header is ever sent, so a cross-origin page cannot read an answer
//! either way.

use axum::{
    extract::{ConnectInfo, Request},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::net::{IpAddr, SocketAddr};

/// Why a request was refused before it was looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Peer,
    Method,
    Host,
    Origin,
    FetchSite,
}

impl Refusal {
    fn code(self) -> &'static str {
        match self {
            Self::Peer => "forbidden_peer",
            Self::Method => "forbidden_method",
            Self::Host => "forbidden_host",
            Self::Origin => "forbidden_origin",
            Self::FetchSite => "forbidden_origin",
        }
    }
}

/// The host part of a `Host` header, if it is one of the names this listener
/// answers to.
pub fn host_allowed(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        // `[::1]` or `[::1]:8080`
        let Some((addr, after)) = rest.split_once(']') else {
            return false;
        };
        if !(after.is_empty() || after.strip_prefix(':').is_some_and(is_port)) {
            return false;
        }
        format!("[{addr}]")
    } else {
        match host.split_once(':') {
            Some((name, port)) if is_port(port) => name.to_string(),
            Some(_) => return false,
            None => host.to_string(),
        }
    };
    matches!(name.as_str(), "127.0.0.1" | "localhost" | "[::1]")
}

fn is_port(s: &str) -> bool {
    !s.is_empty() && s.len() <= 5 && s.bytes().all(|b| b.is_ascii_digit())
}

/// Whether `origin` is exactly the origin of a request addressed to `host`.
pub fn origin_is_this_server(origin: &str, host: &str) -> bool {
    origin == format!("http://{host}")
}

/// Whether a `Sec-Fetch-Site` value is one a request to this server may carry.
pub fn fetch_site_allowed(value: &str) -> bool {
    matches!(value, "same-origin" | "none")
}

/// Every check, over what a request carries.
pub fn check(method: &Method, peer: Option<IpAddr>, headers: &HeaderMap) -> Result<(), Refusal> {
    // A request with no peer address cannot be placed; refuse rather than guess.
    if !peer.is_some_and(|ip| ip.is_loopback()) {
        return Err(Refusal::Peer);
    }
    if !matches!(*method, Method::GET | Method::POST | Method::HEAD) {
        return Err(Refusal::Method);
    }
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !host_allowed(host) {
        return Err(Refusal::Host);
    }
    if let Some(origin) = headers.get(header::ORIGIN) {
        let ok = origin
            .to_str()
            .is_ok_and(|origin| origin_is_this_server(origin, host));
        if !ok {
            return Err(Refusal::Origin);
        }
    }
    if let Some(site) = headers.get("sec-fetch-site")
        && !site.to_str().is_ok_and(fetch_site_allowed)
    {
        return Err(Refusal::FetchSite);
    }
    Ok(())
}

/// The middleware: refuse before anything else runs.
pub async fn guard(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    match check(request.method(), Some(peer.ip()), request.headers()) {
        Ok(()) => next.run(request).await,
        Err(refusal) => {
            tracing::debug!(?refusal, "gui: refused a request before looking at it");
            (
                StatusCode::FORBIDDEN,
                [(header::CONTENT_TYPE, "application/json")],
                format!(
                    r#"{{"error":{{"code":"{}","message":"This request did not come from this page."}}}}"#,
                    refusal.code()
                ),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    const LOCAL: Option<IpAddr> = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));

    /// The names this listener answers to are fixed; a name that merely starts
    /// with one, or hides behind userinfo, is another server.
    #[test]
    fn a_host_is_one_of_three_names_with_or_without_a_port() {
        for ok in [
            "127.0.0.1",
            "127.0.0.1:54321",
            "localhost",
            "localhost:3000",
            "[::1]",
            "[::1]:8080",
        ] {
            assert!(host_allowed(ok), "{ok}");
        }
        for bad in [
            "",
            "evil.com",
            "127.0.0.1.evil.com",
            "localhost.evil.com",
            "evil.com:127.0.0.1",
            "user@127.0.0.1",
            "127.0.0.1@evil.com",
            "127.0.0.1:port",
            "127.0.0.1:",
            "127.0.0.1:999999",
            "[::1",
            "[::1]x",
            "[::2]",
            "0.0.0.0",
            "127.0.0.2",
            "LOCALHOST.",
        ] {
            assert!(!host_allowed(bad), "{bad:?} must be refused");
        }
    }

    /// DNS rebinding: the page's origin is the attacker's name, whatever it
    /// resolves to, so it cannot equal this server's.
    #[test]
    fn a_request_under_another_name_is_refused_whatever_it_resolves_to() {
        let h = headers(&[
            ("host", "evil.example:54321"),
            ("origin", "http://evil.example:54321"),
        ]);
        assert_eq!(check(&Method::GET, LOCAL, &h), Err(Refusal::Host));
    }

    /// Another loopback origin is not this one: a dev server a sandboxed command
    /// started on another port is also `http://127.0.0.1:<port>`.
    #[test]
    fn another_loopback_origin_is_refused_and_this_one_is_not() {
        let this = headers(&[
            ("host", "127.0.0.1:5000"),
            ("origin", "http://127.0.0.1:5000"),
        ]);
        assert_eq!(check(&Method::POST, LOCAL, &this), Ok(()));
        for origin in [
            "http://127.0.0.1:5001",
            "http://localhost:5000",
            "https://127.0.0.1:5000",
            "null",
            "http://127.0.0.1:5000.evil.com",
            "",
        ] {
            let h = headers(&[("host", "127.0.0.1:5000"), ("origin", origin)]);
            assert_eq!(
                check(&Method::POST, LOCAL, &h),
                Err(Refusal::Origin),
                "origin {origin:?}"
            );
        }
        // No Origin at all is a same-origin GET or a non-browser client; the
        // credential decides the rest.
        let none = headers(&[("host", "127.0.0.1:5000")]);
        assert_eq!(check(&Method::GET, LOCAL, &none), Ok(()));
    }

    #[test]
    fn a_browser_that_says_it_is_cross_site_is_refused() {
        for (site, ok) in [
            ("same-origin", true),
            ("none", true),
            ("same-site", false),
            ("cross-site", false),
            ("", false),
        ] {
            let h = headers(&[("host", "127.0.0.1:5000"), ("sec-fetch-site", site)]);
            assert_eq!(check(&Method::GET, LOCAL, &h).is_ok(), ok, "{site:?}");
        }
    }

    #[test]
    fn only_the_methods_this_server_uses_are_allowed() {
        let h = headers(&[("host", "localhost")]);
        for ok in [Method::GET, Method::POST, Method::HEAD] {
            assert_eq!(check(&ok, LOCAL, &h), Ok(()));
        }
        for bad in [
            Method::PUT,
            Method::DELETE,
            Method::PATCH,
            Method::OPTIONS,
            Method::TRACE,
            Method::CONNECT,
        ] {
            assert_eq!(check(&bad, LOCAL, &h), Err(Refusal::Method), "{bad}");
        }
    }

    /// The listener binds loopback only, but a peer that is not on this machine
    /// (a forwarded connection, a misconfigured proxy) is refused as well.
    #[test]
    fn a_peer_off_this_machine_is_refused() {
        let h = headers(&[("host", "127.0.0.1")]);
        assert_eq!(
            check(&Method::GET, Some("10.0.0.5".parse().unwrap()), &h),
            Err(Refusal::Peer)
        );
        assert_eq!(check(&Method::GET, None, &h), Err(Refusal::Peer));
        assert_eq!(
            check(&Method::GET, Some(IpAddr::V6(Ipv6Addr::LOCALHOST)), &h),
            Ok(())
        );
    }
}
