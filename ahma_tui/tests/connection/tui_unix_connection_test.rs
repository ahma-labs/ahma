//! Integration tests — TUI working with `ahma serve unix`.
//!
//! `ahma serve unix` exposes MCP Streamable HTTP over a Unix domain socket.
//!
//! These tests verify:
//! * The TUI Unix socket health probe succeeds.
//! * `resolve_connection(None)` picks the Unix socket when `[http] unix_socket_path`
//!   in `~/.ahma/settings.toml` points to a running bridge.
//! * The retired `AHMA_UNIX_SOCKET` variable (R-CFG1.2) is ignored.
//! * `resolve_connection` falls back to HTTP when no Unix socket is present.
//!
//! All tests are gated on `cfg(unix)` — Unix domain sockets are not available
//! on Windows.

#![cfg(unix)]

use crate::common;

use ahma_tui::connection::{ResolvedConnection, ResolvedTransport, probe_candidate};

/// Point `~/.ahma/settings.toml` at a temp home containing `[http] unix_socket_path`.
///
/// `AHMA_TEST_HOME` is the INTERNAL/TEST hook `ahma_common::config::ahma_home_dir`
/// honors in debug builds; it is the sanctioned way to redirect settings resolution
/// now that the retired `AHMA_UNIX_SOCKET` no longer steers it.
///
/// # Safety
/// nextest runs each test in its own OS process, so no concurrent thread can observe
/// the env mutation.
fn set_socket_setting(home: &std::path::Path, socket_path: &std::path::Path) {
    let ahma_dir = home.join(".ahma");
    std::fs::create_dir_all(&ahma_dir).expect("create .ahma");
    std::fs::write(
        ahma_dir.join("settings.toml"),
        format!(
            "[http]\nunix_socket_path = {}\n",
            toml_string(&socket_path.to_string_lossy())
        ),
    )
    .expect("write settings.toml");
    unsafe {
        std::env::set_var("AHMA_TEST_HOME", home);
    }
}

fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

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

/// `resolve_connection(None)` returns a `UnixSocket` transport when the
/// `[http] unix_socket_path` setting points to a running bridge.
///
/// Represents: TUI auto-discovers `ahma serve unix` before trying TCP, using the same
/// configuration source `ahma serve` resolves the socket from.
#[tokio::test]
async fn resolve_default_uses_unix_socket_from_settings() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let socket_path = tmp.path().join("ahma_tui_unix_resolve_test.sock");
    // Configure the path before starting the bridge so both sides agree on it.
    set_socket_setting(tmp.path(), &socket_path);
    let _bridge = common::start_bridge_unix(socket_path).await;
    let result = ahma_tui::connection::resolve_connection(None)
        .await
        .expect("resolve_connection should succeed when Unix socket is present");
    assert!(
        matches!(result.transport, ResolvedTransport::UnixSocket(_)),
        "expected UnixSocket transport from the settings key, got: {:?}",
        result.transport
    );
    assert_eq!(result.transport_label(), "Unix socket");
}

/// R-CFG1.2: `AHMA_UNIX_SOCKET` is retired and must not steer the TUI's socket
/// resolution — the settings key wins even when the env var is also set.
///
/// `ahma_mcp` already warned-and-ignored this variable while the TUI still honored it,
/// so the same name meant two different things in two binaries of one product. The two
/// binaries must now reach the same verdict.
#[test]
fn unix_socket_path_ignores_retired_env_var() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let from_settings = tmp.path().join("from-settings.sock");
    let from_env = tmp.path().join("from-env.sock");
    set_socket_setting(tmp.path(), &from_settings);
    // SAFETY: nextest isolates each test in its own OS process.
    unsafe {
        std::env::set_var("AHMA_UNIX_SOCKET", &from_env);
    }

    let resolved = ahma_tui::connection::unix_socket_default_path();

    assert_eq!(
        resolved,
        from_settings.to_string_lossy(),
        "[http] unix_socket_path must decide the socket, not the retired env var"
    );
}

/// With neither the settings key nor anything else set, the TUI falls back to the same
/// machine-global socket path `ahma_mcp` defaults to, so the two agree out of the box.
#[test]
fn unix_socket_path_falls_back_to_the_global_default() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    // A temp home with no settings.toml at all.
    // SAFETY: nextest isolates each test in its own OS process.
    unsafe {
        std::env::set_var("AHMA_TEST_HOME", tmp.path());
        std::env::remove_var("AHMA_UNIX_SOCKET");
    }
    assert_eq!(
        ahma_tui::connection::unix_socket_default_path(),
        ahma_mcp::shell::modes::server::GLOBAL_SOCKET_PATH,
    );
}

/// `resolve_connection` falls back to HTTP when the Unix socket does not exist
/// but an HTTP bridge is available.
///
/// Represents: TUI auto-discovers `ahma serve http` after the Unix socket probe
/// fails.
#[tokio::test]
async fn resolve_default_falls_back_to_http_when_no_unix_socket() {
    // Configure a socket path that will never exist.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    set_socket_setting(tmp.path(), &tmp.path().join("never-created.sock"));
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
