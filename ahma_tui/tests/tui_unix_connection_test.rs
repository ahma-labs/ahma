//! Integration tests — TUI working with `ahma serve unix`.
//!
//! `ahma serve unix` exposes MCP Streamable HTTP over a Unix domain socket.
//!
//! These tests verify:
//! * The TUI Unix socket health probe succeeds.
//! * `resolve_connection(None)` picks the Unix socket when `AHMA_UNIX_SOCKET`
//!   points to a running bridge.
//! * `resolve_connection` falls back to HTTP when no Unix socket is present.
//!
//! All tests are gated on `cfg(unix)` — Unix domain sockets are not available
//! on Windows.

#![cfg(unix)]

mod common;

use ahma_tui::connection::{ResolvedConnection, ResolvedTransport, probe_candidate};

// ─── Tests ───────────────────────────────────────────────────────────────────

/// `probe_candidate` with a `UnixSocket` transport returns `true` when the
/// bridge is running on that socket.
///
/// Represents: TUI detects an `ahma serve unix` instance and can poll /health.
#[tokio::test]
async fn unix_health_probe_succeeds() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let socket_path = tmp.path().join("ahma_tui_test.sock");
    let _bridge = common::start_bridge_unix(socket_path.clone()).await;
    let candidate = ResolvedConnection {
        display_url: format!("unix://{}", socket_path.display()),
        transport: ResolvedTransport::UnixSocket(socket_path.to_string_lossy().into_owned()),
    };
    assert!(
        probe_candidate(&candidate).await,
        "probe_candidate should return true for a live Unix socket bridge"
    );
}

/// `resolve_connection(None)` returns a `UnixSocket` transport when
/// `AHMA_UNIX_SOCKET` points to a running bridge.
///
/// Represents: TUI auto-discovers `ahma serve unix` before trying TCP.
///
/// # Safety note
/// `set_var` is safe here — nextest runs each test in its own isolated OS
/// process, so no concurrent threads can observe the env mutation.
#[tokio::test]
async fn resolve_default_uses_unix_socket_when_present() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let socket_path = tmp.path().join("ahma_tui_unix_resolve_test.sock");
    // Set the env var before starting the bridge so both sides agree on the path.
    // SAFETY: nextest isolates each test in its own OS process — no concurrent
    // threads can observe the env mutation.
    unsafe {
        std::env::set_var("AHMA_UNIX_SOCKET", &socket_path);
    }
    let _bridge = common::start_bridge_unix(socket_path).await;
    let result = ahma_tui::connection::resolve_connection(None)
        .await
        .expect("resolve_connection should succeed when Unix socket is present");
    assert!(
        matches!(result.transport, ResolvedTransport::UnixSocket(_)),
        "expected UnixSocket transport when AHMA_UNIX_SOCKET is set, got: {:?}",
        result.transport
    );
    assert_eq!(result.transport_label(), "Unix socket");
}

/// `resolve_connection` falls back to HTTP when the Unix socket does not exist
/// but an HTTP bridge is available.
///
/// Represents: TUI auto-discovers `ahma serve http` after the Unix socket probe
/// fails.
///
/// # Safety note
/// `set_var` is safe here — see `resolve_default_uses_unix_socket_when_present`.
#[tokio::test]
async fn resolve_default_falls_back_to_http_when_no_unix_socket() {
    // Point AHMA_UNIX_SOCKET to a path that will never exist.
    // SAFETY: nextest isolates each test in its own OS process.
    unsafe {
        std::env::set_var(
            "AHMA_UNIX_SOCKET",
            "/tmp/ahma_tui_test_nonexistent_socket_fallback.sock",
        );
    }
    let bridge = common::start_bridge_tcp(false).await;
    // Use an explicit URL so we don't accidentally hit localhost:3000.
    let result = ahma_tui::connection::resolve_connection(Some(&bridge.base_url))
        .await
        .expect("resolve_connection should succeed via HTTP");
    assert!(
        matches!(
            result.transport,
            ResolvedTransport::Http(_) | ResolvedTransport::Http3(_)
        ),
        "expected Http transport as fallback, got: {:?}",
        result.transport
    );
}
