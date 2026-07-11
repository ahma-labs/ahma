//! # ahma_task_tree — Task planning prompt + step parsing
//!
//! This crate provides the planning-prompt builder and the LLM-plan step
//! parser used by the `ahma tui` local-model planning flow. The recursive
//! task-tree execution orchestrator that once lived here was removed because
//! nothing in the shipped product invoked it (no `tool_type: task_tree` config
//! ships and no handler is registered); recover it from git history if that
//! roadmap feature is revived.
//!
//! ## License
//!
//! This crate is licensed under **AGPL-3.0-or-later**.

pub mod parser;
pub mod prompt;

pub use parser::{ParsedStep, parse_steps};
pub use prompt::build_planning_prompt;
