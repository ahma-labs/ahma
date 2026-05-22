//! Configuration types for `tool_type: worker` tools.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Language for an ephemeral synthesized worker.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerLanguage {
    /// Compile and run a Rust program (`rustc` must be on PATH).
    #[default]
    Rust,
    /// Run a Python 3 script (`python3` must be on PATH).
    Python,
}

/// Configuration for a `tool_type: worker` tool.
///
/// A worker tool accepts synthesized source code, compiles or runs it inside a
/// sub-vault, captures the output to `outputs/`, and by default deletes the
/// source after execution.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkerConfig {
    /// Programming language for the synthesized worker.
    #[serde(default)]
    pub language: WorkerLanguage,
    /// Additional compiler / interpreter arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_args: Option<Vec<String>>,
    /// Keep the synthesized source file after execution.
    /// Defaults to `false` — source is deleted after the run.
    #[serde(default)]
    pub keep_source: bool,
    /// Execution timeout in seconds.  Defaults to 60.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
}
