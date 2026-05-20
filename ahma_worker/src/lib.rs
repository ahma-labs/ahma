//! # ahma_worker — Ephemeral worker code synthesis
//!
//! The `worker` tool type compiles and runs synthesized Rust or Python programs
//! inside a sub-vault.  Because the synthesized code is deterministic and
//! executed without an LLM in the loop, it cannot be re-injected mid-run.
//!
//! ## License
//!
//! This crate is licensed under **GPL-3.0-or-later**.

pub mod config;
pub mod runner;

pub use config::{WorkerConfig, WorkerLanguage};
pub use runner::{WorkerResult, WorkerRunner};
