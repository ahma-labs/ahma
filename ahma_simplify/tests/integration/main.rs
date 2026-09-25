//! `ahma_simplify::integration` — report and analysis tests, one binary.
//!
//! One executable per `tests/*.rs` relinks the whole dependency closure each time;
//! a single root file links once. cargo-nextest still runs every test in its own
//! process. Pure-unit (no subprocess, no network), so no `.config/nextest.toml`
//! override applies. Add suites as `tests/integration/<name>.rs` + `mod` below.

mod altitude_lens_test;
mod auto_test;
mod dead_code_lens_test;
mod report_test;
mod reuse_lens_test;
