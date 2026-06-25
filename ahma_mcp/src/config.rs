//! # Tool Configuration Management
//!
//! This module defines the data structures and logic for managing the configuration of
//! command-line tools. All tool configurations are loaded from `.json` files
//! located in the `.ahma/` directory. This approach allows for easy extension and
//! modification of supported tools without altering the core server code.
//!
//! ## Core Data Structures
//!
//! - **`Config`**: The main struct representing the complete configuration for a single
//!   tool. It includes the tool's name, the actual command to execute, and whether the
//!   tool is enabled. It also contains nested structures for more granular control.
//!
//! - **`ToolHints`**: A collection of strings intended to provide guidance to an AI agent
//!   on how to use the tool effectively. It includes hints for specific operations like
//!   `build` and `test`, as well as custom hints for any subcommand.
//!
//! - **`CommandOverride`**: Allows for overriding default behaviors for specific subcommands.
//!   For example, a `test` subcommand could be given a longer timeout or be forced to run
//!   synchronously, even if the CLI --async flag is set.
//!
//! ## Configuration Loading
//!
//! - The `load_from_file` function reads a specified JSON file and deserializes it
//!   into a `Config` struct.
//! - The `load_tool_config` helper function simplifies loading by constructing the path
//!   to a tool's configuration file within the `.ahma/` directory.
//!
//! ## Key Features
//!
//! - **Declarative Tool Definition**: Tools are defined entirely through JSON,
//!   making the system highly modular and easy to maintain.
//! - **Hierarchical Configuration**: Settings can be applied globally (in `Config`), per
//!   operation type (in `ToolHints`), or per specific subcommand (in `CommandOverride`),
//!   providing a flexible and powerful configuration cascade.
//! - **AI Guidance**: The `ToolHints` system is a key feature for improving the performance
//!   of AI agents using the tools, providing them with contextual advice.
//! - **Dynamic Behavior**: The server's behavior, such as whether a command runs
//!   synchronously or asynchronously, can be controlled directly from the configuration files.

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{collections::HashMap, path::Path};

const RESERVED_TOOL_NAMES: &[&str] = &[
    "await",
    "status",
    "run_terminal_command",
    "cancel",
    "restart",
    "logs_list",
    "logs_read",
    "logs_search",
    "read_file",
    "list_dir",
    "file_search",
    "grep_search",
    "fetch_webpage",
    "write_file",
    "replace_in_file",
];
const TOOL_CONFIG_READ_MAX_ATTEMPTS: usize = 8;
const TOOL_CONFIG_READ_BACKOFF_MS: u64 = 40;

/// Represents the complete configuration for a command-line tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct ToolConfig {
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
    pub name: String,
    pub description: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subcommand: Option<Vec<SubcommandConfig>>,
    /// Generated input schema (optional - auto-generated from subcommands)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    /// Default timeout for operations in seconds
    pub timeout_seconds: Option<u64>,
    /// Override the default execution mode for this tool.
    /// - `true`: Always run synchronously (blocking, returns result immediately)
    /// - `false`: Always run asynchronously (non-blocking, returns operation ID)
    /// - `null`/omitted: Use server default (async unless --sync CLI flag)
    ///
    /// Inheritance: Subcommand-level settings override tool-level settings.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "force_synchronous"
    )]
    #[deprecated(
        since = "0.11.2",
        note = "Use dynamic 'blocking' argument in tools/call instead"
    )]
    pub synchronous: Option<bool>,
    #[serde(default)]
    pub hints: ToolHints,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Key to look up hardcoded guidance
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guidance_key: Option<String>,
    /// Optional sequence of tools to execute in order (for composite tools)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<Vec<SequenceStep>>,
    /// Delay in milliseconds between sequence steps (default: SEQUENCE_STEP_DELAY_MS)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_delay_ms: Option<u64>,
    /// Runtime availability probe executed at server startup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub availability_check: Option<AvailabilityCheck>,
    /// Installation guidance displayed when the tool is unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_instructions: Option<String>,
    /// Default log monitor level for this tool. When set, all invocations of this tool
    /// will have live log monitoring enabled at this severity.
    /// Values: "error", "warn", "info", "debug", "trace"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor_level: Option<String>,
    /// Which stream to monitor for log patterns (default: "stderr").
    /// Values: "stderr", "stdout", "both"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor_stream: Option<String>,
    /// Tool type classifier. Defaults to `Command` for normal CLI tools.
    /// Set to `Livelog` for long-running log-streaming tools that pipe output through an LLM.
    /// Tool types implemented in separate AGPL-licensed crates (e.g. `decompose`, `worker`)
    /// are deserialized as `Extension` and their configurations stored in the matching
    /// opaque JSON fields below.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_type: Option<ToolType>,
    /// Live log monitoring configuration. Required when `tool_type` is `Livelog`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub livelog: Option<LivelogConfig>,
    /// Decompose orchestration configuration (opaque — parsed by `ahma_decompose` crate).
    /// Required when `tool_type` is `decompose`; stored as raw JSON for GPL-crate consumption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decompose: Option<serde_json::Value>,
    /// Worker synthesis configuration (opaque — parsed by `ahma_worker` crate).
    /// Required when `tool_type` is `worker`; stored as raw JSON for GPL-crate consumption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<serde_json::Value>,
    /// Task tree configuration (opaque — parsed by `ahma_task_tree` crate).
    /// Required when `tool_type` is `task_tree`; stored as raw JSON for GPL-crate consumption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_tree: Option<serde_json::Value>,
}

/// Classifier that determines how the MCP service routes a tool invocation.
///
/// The permissive `ahma_mcp` library handles `Command` and `Livelog` natively.
/// Tool types implemented in the AGPL-licensed sibling crates (`ahma_decompose`,
/// `ahma_worker`, etc.) are serialised to their JSON names (e.g. `"decompose"`,
/// `"worker"`) and round-trip correctly — they are just stored as `Extension`
/// in this enum so the MIT library has no compile-time dependency on AGPL code.
/// `ahma_bin` routes those calls to the appropriate AGPL crate at runtime.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolType {
    /// Normal command-line tool (default).
    #[default]
    Command,
    /// Long-running log source piped through an LLM for issue detection.
    Livelog,
    /// Any tool type implemented outside this crate (e.g. `decompose`, `worker`).
    /// The raw `tool_type` string is preserved for routing by the AGPL binary crates.
    #[serde(other)]
    Extension,
}

/// Connection details for an OpenAI-compatible LLM provider.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LlmProviderConfig {
    /// Base URL of the API endpoint, e.g. `http://localhost:11434/v1` (Ollama)
    /// or `https://api.openai.com/v1`.
    pub base_url: String,
    /// Model identifier, e.g. `llama3.2`, `gpt-4o-mini`.
    pub model: String,
    /// Optional bearer token. Supports `${ENV_VAR}` interpolation — **never
    /// store literal API keys in tool definition files**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

impl LlmProviderConfig {
    /// Resolve env-var placeholders in `api_key` and warn on literal secrets.
    ///
    /// Returns `Err` if a `${VAR}` reference is not set in the environment.
    pub fn resolve(&self) -> Result<ResolvedLlmProvider> {
        use ahma_common::config::{interpolate_env_vars, warn_if_looks_like_literal_secret};
        if let Some(key) = &self.api_key {
            warn_if_looks_like_literal_secret(key);
        }
        let api_key = self
            .api_key
            .as_deref()
            .map(interpolate_env_vars)
            .transpose()?;
        // `base_url` and `model` also support `${VAR}` / `${VAR:-default}` so an
        // operator can point a tool at a different endpoint or model via the
        // environment without editing the tool definition file.
        Ok(ResolvedLlmProvider {
            base_url: interpolate_env_vars(&self.base_url)?,
            model: interpolate_env_vars(&self.model)?,
            api_key,
        })
    }
}

/// An [`LlmProviderConfig`] with secrets resolved — ready to hand to an HTTP client.
#[derive(Debug, Clone)]
pub struct ResolvedLlmProvider {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
}

/// Configuration for a `tool_type: livelog` tool.
///
/// A livelog tool spawns a long-running source command (e.g. `adb logcat`),
/// accumulates output into chunks, and periodically asks an LLM whether the chunk
/// contains issues matching the `detection_prompt`.  When an issue is found, a
/// `ProgressUpdate::LogAlert` notification is pushed to the MCP client.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LivelogConfig {
    /// The executable to run as the log source (e.g. `"adb"`, `"ssh"`, `"tail"`).
    pub source_command: String,
    /// Arguments for the source command (e.g. `["logcat", "-v", "threadtime"]`).
    ///
    /// Each argument may reference a runtime parameter declared in
    /// [`LivelogConfig::parameters`] with `${name}` syntax. When the caller
    /// supplies the parameter, every `${name}` is substituted with its value;
    /// when the parameter is omitted (or empty), the **entire argument token**
    /// that referenced it is dropped. This lets an optional flag such as
    /// `"--pid=${pid}"` disappear cleanly when no pid is supplied, rather than
    /// expanding to an invalid `--pid=`.
    #[serde(default)]
    pub source_args: Vec<String>,
    /// Plain-English description of what to look for, passed to the LLM as the
    /// detection criteria (e.g. "Look for crashes, exceptions, or ANR errors").
    pub detection_prompt: String,
    /// LLM provider connection details.
    pub llm_provider: LlmProviderConfig,
    /// Runtime parameters the caller may pass when starting the tool. These are
    /// surfaced in the tool's MCP input schema and substituted into
    /// [`LivelogConfig::source_args`] and [`LivelogConfig::env`] via `${name}`.
    #[serde(default)]
    pub parameters: Vec<LivelogParameter>,
    /// Environment variables to set on the source (and clear) process. Values
    /// may reference runtime parameters with `${name}`; an entry whose value
    /// references a parameter the caller did not supply is dropped entirely.
    /// Example: `{ "ANDROID_SERIAL": "${serial}" }` targets a specific device
    /// only when a `serial` parameter is passed.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Optional command run once before the source process starts (e.g.
    /// `["logcat", "-c"]` to clear Android's log buffers so stale crashes are
    /// not replayed as fresh alerts). Runs with the same `env` applied and is
    /// best-effort — a failure is logged but does not abort monitoring. The
    /// caller can skip it by passing `clear: false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear_command: Option<Vec<String>>,
    /// Optional regular expression used to pre-filter lines before they are sent
    /// to the LLM. When set, only lines matching the pattern are accumulated
    /// into a chunk for analysis (the full, unfiltered output is still recorded
    /// on the operation). This keeps token cost and latency low by never sending
    /// obviously-benign noise to the model. An invalid pattern is logged and
    /// ignored (all lines pass through).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefilter_regex: Option<String>,
    /// Maximum number of lines to accumulate before sending a chunk to the LLM.
    /// Defaults to 50.
    #[serde(default = "default_chunk_max_lines")]
    pub chunk_max_lines: usize,
    /// Maximum time in seconds to wait before sending a partial chunk to the LLM.
    /// Defaults to 30.
    #[serde(default = "default_chunk_max_seconds")]
    pub chunk_max_seconds: u64,
    /// Minimum time in seconds between consecutive LLM alerts for this tool.
    /// Prevents alert storms when many lines match in quick succession.  Defaults to 60.
    #[serde(default = "default_cooldown_seconds")]
    pub cooldown_seconds: u64,
    /// Maximum time in seconds to wait for the LLM to respond to a single request.
    /// Defaults to 30.
    #[serde(default = "default_llm_timeout_seconds")]
    pub llm_timeout_seconds: u64,
    /// When `true`, instruct the LLM to return a structured JSON object instead of
    /// plain-English prose.  The pipeline parses the JSON and emits a rich alert
    /// with discrete fields (`level`, `summary`, `exception_class`, `top_frame`).
    /// If the LLM returns un-parseable JSON the response is treated as plain text.
    /// Defaults to `false` for backward-compatibility.
    #[serde(default)]
    pub structured_output: bool,
}

/// A runtime parameter a livelog tool accepts when started.
///
/// Parameters are surfaced in the tool's MCP input schema (as optional string
/// properties unless `required`) and substituted into the source command's
/// arguments and environment via `${name}`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LivelogParameter {
    /// Parameter name, referenced in `source_args`/`env` as `${name}`.
    pub name: String,
    /// Human-readable description shown to MCP clients.
    pub description: String,
    /// Whether the caller must supply this parameter. Defaults to `false`.
    #[serde(default)]
    pub required: bool,
}

/// Runtime values resolved from the caller's MCP arguments for a livelog start.
///
/// Produced by [`LivelogConfig::resolve_runtime`] and consumed by the livelog
/// pipeline. Keeps all caller-driven substitution out of the generic pipeline.
#[derive(Debug, Clone, Default)]
pub struct LivelogRuntime {
    /// `source_args` with `${param}` placeholders substituted and tokens that
    /// referenced an unsupplied parameter dropped.
    pub source_args: Vec<String>,
    /// Environment variables to set on spawned processes (placeholder values
    /// resolved; entries referencing unsupplied parameters omitted).
    pub env: Vec<(String, String)>,
    /// Whether to run `clear_command` before starting the source process.
    pub clear: bool,
}

impl LivelogConfig {
    /// Resolve the caller's runtime arguments against this config.
    ///
    /// `args` is the MCP `tools/call` argument map. Declared parameters are read
    /// from it (string values), substituted into `source_args` and `env`, and a
    /// missing **required** parameter produces an error. The boolean `clear`
    /// argument (default `true`) controls whether `clear_command` runs.
    pub fn resolve_runtime(
        &self,
        args: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<LivelogRuntime> {
        use anyhow::bail;

        // Collect supplied, non-empty parameter values keyed by declared name.
        let mut supplied: std::collections::HashMap<&str, String> =
            std::collections::HashMap::new();
        for param in &self.parameters {
            match args.get(&param.name).and_then(|v| v.as_str()) {
                Some(v) if !v.is_empty() => {
                    supplied.insert(param.name.as_str(), v.to_string());
                }
                _ => {
                    if param.required {
                        bail!("required parameter '{}' was not supplied", param.name);
                    }
                }
            }
        }

        let source_args = substitute_tokens(&self.source_args, &supplied);

        let mut env = Vec::new();
        for (key, raw) in &self.env {
            if let Some(value) = substitute_value(raw, &supplied) {
                env.push((key.clone(), value));
            }
        }

        // `clear` defaults to true; only an explicit `false` disables it.
        let clear = args.get("clear").and_then(|v| v.as_bool()).unwrap_or(true);

        Ok(LivelogRuntime {
            source_args,
            env,
            clear,
        })
    }
}

/// Substitute `${name}` placeholders in each token, dropping any token that
/// references a parameter not present in `supplied`.
fn substitute_tokens(
    tokens: &[String],
    supplied: &std::collections::HashMap<&str, String>,
) -> Vec<String> {
    tokens
        .iter()
        .filter_map(|tok| substitute_value(tok, supplied))
        .collect()
}

/// Substitute `${name}` placeholders in `raw`. Returns `None` (signalling the
/// caller to drop the value) if `raw` references any `${name}` whose parameter
/// was not supplied; returns `Some` with all placeholders replaced otherwise.
fn substitute_value(
    raw: &str,
    supplied: &std::collections::HashMap<&str, String>,
) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}')?;
        let name = &after[..end];
        match supplied.get(name) {
            Some(value) => out.push_str(value),
            None => return None,
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

fn default_chunk_max_lines() -> usize {
    50
}

fn default_chunk_max_seconds() -> u64 {
    30
}

fn default_cooldown_seconds() -> u64 {
    60
}

fn default_llm_timeout_seconds() -> u64 {
    30
}

/// Configuration for a subcommand, allowing for nested commands.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct SubcommandConfig {
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subcommand: Option<Vec<SubcommandConfig>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<CommandOption>>,
    /// Optional arguments that are not flags, but positional values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub positional_args: Option<Vec<CommandOption>>,
    /// When true, positional args are placed BEFORE options in the command line.
    /// Required for commands like `find` where path must precede expressions.
    /// Default: false (options come first, then positional args)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub positional_args_first: Option<bool>,
    /// Override timeout for this specific subcommand
    pub timeout_seconds: Option<u64>,
    /// Override the default execution mode for this subcommand.
    /// - `true`: Always run synchronously (blocking, returns result immediately)
    /// - `false`: Always run asynchronously (non-blocking, returns operation ID)
    /// - `null`/omitted: Inherit from tool level, or use server default if tool doesn't specify
    ///
    /// Inheritance: Subcommand-level settings override tool-level settings.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "force_synchronous"
    )]
    #[deprecated(
        since = "0.11.2",
        note = "Use dynamic 'blocking' argument in tools/call instead"
    )]
    pub synchronous: Option<bool>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Key to look up hardcoded guidance
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guidance_key: Option<String>,
    /// Optional sequence of tools to execute in order (for composite tools)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<Vec<SequenceStep>>,
    /// Delay in milliseconds between sequence steps (default: SEQUENCE_STEP_DELAY_MS)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_delay_ms: Option<u64>,
    /// Runtime availability probe executed at server startup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub availability_check: Option<AvailabilityCheck>,
    /// Installation guidance displayed when the subcommand is unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_instructions: Option<String>,
}

/// Configuration for a single command-line option.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandOption {
    pub name: String,
    #[serde(rename = "type")]
    pub option_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<ItemsSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_arg: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_flag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

/// Schema details for array items.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ItemsSpec {
    #[serde(rename = "type")]
    pub item_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Provides hints to an AI agent on how to use a tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct ToolHints {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependencies: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clean: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom: Option<HashMap<String, String>>,
}

/// Defines how to probe for tool or subcommand availability at runtime.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AvailabilityCheck {
    /// Optional override for the executable to invoke during the check. Defaults to the tool command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Arguments passed to the availability probe.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Working directory used for the probe (defaults to project root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    /// Exit codes considered successful (defaults to `[0]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success_exit_codes: Option<Vec<i32>>,
    /// If true, do not append derived subcommand arguments when constructing the probe command.
    #[serde(default, skip_serializing_if = "is_false")]
    pub skip_subcommand_args: bool,
}

/// Represents a single step in a tool sequence
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SequenceStep {
    /// Name of the tool to invoke
    pub tool: String,
    /// Subcommand within that tool
    pub subcommand: String,
    /// Arguments to pass to the tool
    #[serde(default)]
    pub args: Map<String, Value>,
    /// Optional description for logging/display
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Skip this step if the specified file exists
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_if_file_exists: Option<String>,
    /// Skip this step if the specified file is missing
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_if_file_missing: Option<String>,
}

/// Top-level MCP client configuration (mcp.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    /// Server entries keyed by logical name.
    pub servers: HashMap<String, ServerConfig>,
}

/// Configuration for a single MCP server entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerConfig {
    /// Spawn an MCP server as a child process over stdio.
    #[serde(rename = "child_process")]
    ChildProcess(ChildProcessConfig),
    /// Connect to an MCP server over HTTP/SSE.
    #[serde(rename = "http")]
    Http(HttpServerConfig),
}

/// Child-process transport configuration for an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildProcessConfig {
    /// Executable path or command name.
    pub command: String,
    /// Arguments passed to the MCP server process.
    pub args: Vec<String>,
}

/// HTTP transport configuration for an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpServerConfig {
    /// Base URL of the MCP HTTP endpoint.
    pub url: String,
    /// Optional OAuth client id for Atlassian flows.
    pub atlassian_client_id: Option<String>,
    /// Optional OAuth client secret for Atlassian flows.
    pub atlassian_client_secret: Option<String>,
}

// Helper functions for serde defaults
fn default_enabled() -> bool {
    true
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_reserved_tool_name(name: &str) -> bool {
    RESERVED_TOOL_NAMES.contains(&name)
}

fn builtin_tool_configs(config: &crate::shell::cli::AppConfig) -> Vec<(String, &'static str)> {
    let mut bundle_names = config.tool_bundles.clone();

    // Deduplicate bundle names
    let unique_names: std::collections::HashSet<_> = bundle_names.drain(..).collect();
    let mut sorted_names: Vec<_> = unique_names.into_iter().collect();
    sorted_names.sort();

    sorted_names
        .into_iter()
        .filter_map(|bundle_string| {
            let bundle_name = bundle_string.as_str();
            builtin_tool_definition(bundle_name).map(|json| (bundle_string, json))
        })
        .collect()
}

fn builtin_tool_definition(bundle_name: &str) -> Option<&'static str> {
    match bundle_name {
        "fileutils" => Some(include_str!("../../.ahma/file-tools.json")),
        "github" => Some(include_str!("../../.ahma/gh.json")),
        "git" => Some(include_str!("../../.ahma/git.json")),
        "python" => Some(include_str!("../../.ahma/python.json")),
        "simplify" => Some(include_str!("../../.ahma/simplify.json")),
        _ => None,
    }
}

fn parse_builtin_tool_config(json_str: &str) -> anyhow::Result<ToolConfig> {
    Ok(serde_json::from_str::<ToolConfig>(json_str)?)
}

enum ToolConfigLoadError {
    Read(std::io::Error),
    Parse(serde_json::Error),
}

async fn read_tool_config_once(path: &Path) -> Result<ToolConfig, ToolConfigLoadError> {
    let contents = tokio::fs::read_to_string(path)
        .await
        .map_err(ToolConfigLoadError::Read)?;
    serde_json::from_str::<ToolConfig>(&contents).map_err(ToolConfigLoadError::Parse)
}

fn log_tool_config_load_failure(path: &Path, error: &ToolConfigLoadError) {
    match error {
        ToolConfigLoadError::Read(error) => {
            tracing::warn!("Failed to read {}: {}", path.display(), error);
        }
        ToolConfigLoadError::Parse(error) => {
            tracing::warn!("Failed to parse {}: {}", path.display(), error);
        }
    }
}

async fn read_tool_config_with_retry(path: &Path) -> Option<ToolConfig> {
    use std::time::Duration;

    for attempt in 1..=TOOL_CONFIG_READ_MAX_ATTEMPTS {
        match read_tool_config_once(path).await {
            Ok(config) => return Some(config),
            Err(error) if attempt == TOOL_CONFIG_READ_MAX_ATTEMPTS => {
                log_tool_config_load_failure(path, &error);
                return None;
            }
            Err(_) => {}
        }

        tokio::time::sleep(Duration::from_millis(TOOL_CONFIG_READ_BACKOFF_MS)).await;
    }

    None
}

fn validate_tool_name(name: &str, source: &str) -> anyhow::Result<()> {
    if is_reserved_tool_name(name) {
        anyhow::bail!(
            "Tool name '{}' conflicts with a hardcoded system tool. Reserved tool names: {:?}. Please rename your tool in {}",
            name,
            RESERVED_TOOL_NAMES,
            source
        );
    }
    Ok(())
}

async fn load_configs_from_dir(
    dir: &Path,
    configs: &mut HashMap<String, ToolConfig>,
) -> anyhow::Result<()> {
    use tokio::fs;

    if !fs::try_exists(dir).await.unwrap_or(false) {
        return Ok(());
    }

    let mut entries = match fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(
                "Skipping inaccessible tools directory '{}': {}",
                dir.display(),
                e
            );
            return Ok(());
        }
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !is_json_config_path(&path) {
            continue;
        }

        load_single_config_path(&path, configs).await?;
    }

    Ok(())
}

fn is_json_config_path(path: &Path) -> bool {
    path.extension().and_then(|s| s.to_str()) == Some("json")
}

async fn load_single_config_path(
    path: &Path,
    configs: &mut HashMap<String, ToolConfig>,
) -> anyhow::Result<()> {
    let Some(config) = read_tool_config_with_retry(path).await else {
        return Ok(());
    };

    validate_tool_name(&config.name, &path.display().to_string())?;
    configs.insert(config.name.clone(), config);
    Ok(())
}

fn insert_built_in_config(configs: &mut HashMap<String, ToolConfig>, config: ToolConfig) {
    match configs.entry(config.name.clone()) {
        std::collections::hash_map::Entry::Occupied(_) => {
            tracing::info!(
                "Bundled tool '{}' overridden by local .ahma/ definition",
                config.name
            );
        }
        std::collections::hash_map::Entry::Vacant(e) => {
            e.insert(config);
        }
    }
}

fn log_builtin_config_parse_error(bundle_name: &str, error: &anyhow::Error) {
    tracing::error!(
        "Failed to parse built-in tool configuration for {}: {}",
        bundle_name,
        error
    );
}

fn synthetic_run_terminal_command_config() -> ToolConfig {
    ToolConfig {
        name: "run_terminal_command".to_string(),
        description: "Execute shell commands within a secure sandbox".to_string(),
        command: if cfg!(target_os = "windows") {
            "powershell -Command".to_string()
        } else {
            "bash -c".to_string()
        },
        enabled: true,
        subcommand: Some(vec![SubcommandConfig {
            name: "default".to_string(),
            description: "Run a shell command".to_string(),
            enabled: true,
            positional_args: Some(vec![CommandOption {
                name: "command".to_string(),
                option_type: "string".to_string(),
                description: Some("The shell command to execute".to_string()),
                required: Some(true),
                format: None,
                items: None,
                file_arg: None,
                file_flag: None,
                alias: None,
            }]),
            ..Default::default()
        }]),
        ..Default::default()
    }
}

pub async fn load_mcp_config(config_path: &Path) -> anyhow::Result<McpConfig> {
    if !tokio::fs::try_exists(config_path).await.unwrap_or(false) {
        return Ok(McpConfig {
            servers: HashMap::new(),
        });
    }

    let contents = tokio::fs::read_to_string(config_path).await?;
    let config: McpConfig = serde_json::from_str(&contents)?;
    Ok(config)
}

/// Load all tool configurations from a directory (async version)
///
/// This function scans the specified directory for JSON files and attempts to
/// deserialize each one into a `ToolConfig`. If the directory doesn't exist or
/// is empty, an empty HashMap is returned. It also loads built-in tools based on config flags.
///
/// # Arguments
/// * `config` - Current application configuration to determine which tool bundles are active
/// * `tools_dir` - Optional path to the directory containing tool configuration files.
///   When `None`, only bundled tools and the synthetic `run_terminal_command` config are loaded.
///
/// # Returns
/// * `Result<HashMap<String, ToolConfig>>` - Map of tool name to configuration or error
pub async fn load_tool_configs(
    config: &crate::shell::cli::AppConfig,
    tools_dir: Option<&Path>,
) -> anyhow::Result<HashMap<String, ToolConfig>> {
    let all_dirs: Vec<std::path::PathBuf> =
        tools_dir.map(|p| vec![p.to_path_buf()]).unwrap_or_default();

    let mut configs = HashMap::new();

    // When --tools-dir is explicitly provided, load all tools from it regardless
    // of bundle flags. When .ahma/ is auto-detected, also load ALL local tools
    // regardless of bundle flags. Bundle flags only control which built-in
    // (compiled-in) fallback definitions are activated; local .ahma/ definitions
    // always take precedence and are always fully loaded.

    for dir in all_dirs {
        load_configs_from_dir(&dir, &mut configs).await?;
    }

    // Load built-in tools based on configured bundles
    for (bundle_name, json_str) in builtin_tool_configs(config) {
        match parse_builtin_tool_config(json_str) {
            Ok(config) => {
                validate_tool_name(&config.name, &bundle_name)?;
                insert_built_in_config(&mut configs, config);
            }
            Err(error) => {
                log_builtin_config_parse_error(&bundle_name, &error);
            }
        }
    }

    // Inject synthetic config for `run_terminal_command` so sequences can reference it.
    // This is the single source of truth for run_terminal_command's ToolConfig shape.
    // The MCP service handler still intercepts `run_terminal_command` calls directly,
    // but sequences need a ToolConfig to resolve the command and subcommand.
    configs.insert(
        "run_terminal_command".to_string(),
        synthetic_run_terminal_command_config(),
    );

    Ok(configs)
}

/// Synchronous wrapper around `load_tool_configs` for test use only.
///
/// Creates a one-shot Tokio runtime and delegates to the async version.
/// Production code should use `load_tool_configs` directly.
pub fn load_tool_configs_sync(
    config: &crate::shell::cli::AppConfig,
    tools_dir: Option<&Path>,
) -> anyhow::Result<HashMap<String, ToolConfig>> {
    tokio::runtime::Runtime::new()?.block_on(load_tool_configs(config, tools_dir))
}
