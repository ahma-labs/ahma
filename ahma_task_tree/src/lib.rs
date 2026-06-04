//! # ahma_task_tree — Recursive task decomposition and execution
//!
//! This crate implements depth-first task tree execution, interleaving planning
//! reasoning with local tool execution, while dynamically scoping filesystem,
//! tool, and network access.
//!
//! ## License
//!
//! This crate is licensed under **AGPL-3.0-or-later**.

pub mod config;
pub mod handler;
pub mod orchestrator;
pub mod parser;
pub mod prompt;
pub mod tree;

pub use config::{LlmProviderConfig, TaskTreeConfig};
pub use handler::TaskTreeExtensionHandler;
pub use orchestrator::TaskTreeOrchestrator;
pub use tree::{NodeId, NodeResult, NodeState, TaskNode, TaskTree, TaskType};
