//! # ahma_decompose — Local-LLM task decomposition
//!
//! The `decompose` tool type splits a complex business question into a DAG of
//! smaller sub-questions, dispatches each to a local LLM (e.g. `gemma:4b` via
//! Ollama), and aggregates the results with a deterministic Rust reducer.
//!
//! ## License
//!
//! This crate is licensed under **AGPL-3.0-or-later**.

pub mod config;
pub mod orchestrator;
pub mod reducer;

pub use config::DecomposeConfig;
pub use orchestrator::{DecomposeOrchestrator, SubTaskResult};
pub use reducer::{ReduceMode, Reducer};
