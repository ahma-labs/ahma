//! # Decompose Tool Type
//!
//! The `decompose` tool type splits a complex business question into a DAG of
//! smaller sub-questions, dispatches each to a local LLM (e.g. `gemma:4b` via
//! Ollama), and aggregates the results with a deterministic Rust reducer.
//!
//! ## Design
//!
//! ```text
//! User question
//!       │
//!       ▼
//! DecomposeOrchestrator
//!       │ splits into sub-tasks (up to max_subtasks)
//!       ├── SubTask[0] ──► LlmClient ──► result_0
//!       ├── SubTask[1] ──► LlmClient ──► result_1
//!       └── SubTask[N] ──► LlmClient ──► result_N
//!                                              │
//!                                      Reducer::reduce()
//!                                              │
//!                                      aggregated answer
//! ```
//!
//! Each sub-task runs in its own Tokio task; the concurrency is bounded by
//! `max_concurrent` to avoid overwhelming a single-machine Ollama instance.
//!
//! ## MTDF tool_type: decompose
//!
//! Set `"tool_type": "decompose"` in an MTDF JSON file to activate this handler.
//! See `.ahma/decompose.json` for a ready-to-use example.

pub mod orchestrator;
pub mod reducer;

pub use orchestrator::{DecomposeOrchestrator, SubTaskResult};
pub use reducer::{ReduceMode, Reducer};
