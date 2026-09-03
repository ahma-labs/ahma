//! `ahma_tui::connection` — the TUI connection suites (HTTP, HTTP/3 QUIC, Unix
//! socket), in one binary.
//!
//! WHY: each used to be its own executable, each recompiling `tests/common/` and
//! linking the full closure. The bridge they talk to is in-process, but they bind
//! real loopback sockets and assert against wall-clock timeouts, so
//! `.config/nextest.toml` grants `binary_id(ahma_tui::connection)` CI retries
//! (no throttle). cargo-nextest still runs every test in its own process.
//!
//! Add a new suite as `tests/connection/<name>.rs` plus a `mod` line below.

#[path = "../common/mod.rs"]
mod common;

mod tui_http3_quic_connection_test;
mod tui_http_connection_test;
mod tui_unix_connection_test;
