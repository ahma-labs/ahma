//! `ahma_core::integration` — pure filesystem/logic integration tests, one binary.
//!
//! One executable per `tests/*.rs` relinks the whole dependency closure each time;
//! a single root file links once. cargo-nextest still runs every test in its own
//! process. No `.config/nextest.toml` override applies (no process or network
//! boundary is crossed). Add suites as `tests/integration/<name>.rs` + `mod` below.

mod approvals_migration;
mod approvals_persistence;
mod trusted_workspace;
