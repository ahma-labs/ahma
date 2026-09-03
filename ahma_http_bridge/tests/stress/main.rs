//! `ahma_http_bridge::stress` — the `#[ignore]`d stress suites, in one binary.
//!
//! Every test here is `#[ignore]` and is run by `.github/workflows/ignored-tests.yml`
//! (`--run-ignored all`) and `scripts/stress-test.sh`, never by the merge gate.
//! They are kept out of `e2e` so that binary's fast path is undisturbed, and
//! consolidated with each other so `.config/nextest.toml` can give them their
//! own slow-timeout headroom via `binary_id(ahma_http_bridge::stress)` — which
//! must stay ABOVE the `e2e` override in every profile (first match wins per
//! setting). cargo-nextest still runs every test in its own process.
//!
//! Add a new stress suite as `tests/stress/<name>.rs` plus a `mod` line below.

#[path = "../common/mod.rs"]
mod common;

mod bridge_stress_tests;
mod sandbox_roots_handshake_stress_test;
mod session_stress_test;
