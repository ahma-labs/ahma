//! `ahma_http_mcp_client::integration` — wiremock-backed transport tests, one binary.
//!
//! One executable per `tests/*.rs` relinks the whole dependency closure each time;
//! a single root file links once. cargo-nextest still runs every test in its own
//! process. No `.config/nextest.toml` override applies (in-process mock server,
//! no scheduling dependence). Add suites as `tests/integration/<name>.rs` + `mod` below.

mod http_client_transport_test;
mod streamable_client_test;
