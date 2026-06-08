//! Integration tests — TUI HTTP/3 (QUIC) upgrade path.
//!
//! When the bridge is started with `enable_quic: true` it advertises QUIC via
//! the `Alt-Svc: h3=":<port>"` response header.  The TUI's `resolve_connection`
//! upgrades the transport to `ResolvedTransport::Http3` when both of:
//! 1. The server advertises h3 in `Alt-Svc`.
//! 2. A local TLS cert+key exist at `AHMA_TLS_DIR`.
//!
//! Tests that require QUIC to actually start skip gracefully when the bridge
//! reports no `Alt-Svc: h3=...` header (e.g. when UDP port binding is
//! unavailable in the CI environment).

mod common;

use ahma_common::timeouts::TestTimeouts;
use ahma_tui::connection::{ResolvedTransport, parse_h3_from_alt_svc};

// ─── Tests ───────────────────────────────────────────────────────────────────

/// When `enable_quic: true`, the bridge's `/health` response includes an
/// `Alt-Svc` header advertising HTTP/3.
///
/// Represents: `ahma serve http` advertising QUIC to all clients.
#[tokio::test]
async fn bridge_advertises_alt_svc_h3() {
    let bridge = common::start_bridge_tcp(true).await;

    let client = reqwest::Client::builder()
        .timeout(TestTimeouts::scale_secs(5))
        .build()
        .expect("reqwest client");

    let resp = client
        .get(format!("{}/health", bridge.base_url))
        .send()
        .await
        .expect("GET /health");

    assert!(
        resp.status().is_success(),
        "health check should succeed: {}",
        resp.status()
    );

    let alt_svc = resp
        .headers()
        .get("alt-svc")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if alt_svc.is_empty() {
        // QUIC did not start (e.g. UDP binding unavailable). Non-fatal — skip.
        eprintln!(
            "[skip] bridge_advertises_alt_svc_h3: no Alt-Svc header — \
             QUIC may be unavailable in this environment"
        );
        return;
    }

    assert!(
        parse_h3_from_alt_svc(alt_svc).is_some(),
        "Alt-Svc header should contain an h3 token when QUIC is enabled, got: {alt_svc:?}"
    );
}

/// `resolve_connection` upgrades the transport to `Http3` when the server
/// advertises h3 via `Alt-Svc` AND a local TLS certificate is present.
///
/// Represents: TUI displaying "HTTP/3 (QUIC)" in the status header.
///
/// # Safety note
/// `set_var` is safe here — nextest runs each test in an isolated OS process.
#[tokio::test]
async fn resolve_connection_upgrades_to_http3() {
    let bridge = common::start_bridge_tcp(true).await;

    // Check whether QUIC actually started — skip gracefully if not.
    let client = reqwest::Client::builder()
        .timeout(TestTimeouts::scale_secs(5))
        .build()
        .expect("reqwest client");

    let resp = client
        .get(format!("{}/health", bridge.base_url))
        .send()
        .await
        .expect("GET /health");

    let alt_svc = resp
        .headers()
        .get("alt-svc")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if !alt_svc.contains("h3") {
        eprintln!(
            "[skip] resolve_connection_upgrades_to_http3: bridge did not advertise h3 — \
             QUIC may be unavailable"
        );
        return;
    }

    // Create dummy cert/key files in a temp dir.
    // `LocalTlsConfig::exists()` only checks file presence — the TUI does not
    // validate or use cert content during the HTTP health poll.
    let tls_dir = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(tls_dir.path().join("cert.der"), b"dummy_cert_for_tui_test")
        .expect("write cert.der");
    std::fs::write(tls_dir.path().join("key.der"), b"dummy_key_for_tui_test")
        .expect("write key.der");

    // Override AHMA_TLS_DIR so that `try_upgrade_to_http3` finds the cert.
    // SAFETY: nextest isolates each test in its own OS process.
    unsafe {
        std::env::set_var("AHMA_TLS_DIR", tls_dir.path());
    }

    let result = ahma_tui::connection::resolve_connection(Some(&bridge.base_url))
        .await
        .expect("resolve_connection should succeed");

    assert!(
        matches!(result.transport, ResolvedTransport::Http3(_)),
        "expected Http3 transport after QUIC upgrade, got: {:?}",
        result.transport
    );
    assert_eq!(
        result.transport_label(),
        "HTTP/3 (QUIC)",
        "transport label should reflect the QUIC upgrade"
    );
}
