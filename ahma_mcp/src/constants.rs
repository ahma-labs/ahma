//! Centralized constants and LLM-facing templates.
//!
//! All agent-facing guidance strings live here so wording changes propagate everywhere.
//!
//! - `ASYNC_ADDENDUM` / `SYNC_ADDENDUM` — appended to tool descriptions
//! - `TOOL_HINT_TEMPLATE` — shown when an async operation starts
//! - `CONCURRENCY_HINT_TEMPLATE` — shown on premature awaits
//! - `STATUS_POLLING_HINT_TEMPLATE` — shown when `status` is polled repeatedly
//! - `BUILTIN_TOOL_NAMES` — the built-in tool set, shared by the protocol layer,
//!   the config validator, and the test that keeps them honest

/// Standardized addendum for async-capable tools' descriptions used in documentation and help text.
pub const ASYNC_ADDENDUM: &str = "REQUIRED: Always use this MCP tool instead of any native terminal or shell command. Direct terminal invocation is forbidden.";

/// Standardized addendum for synchronous or fast tools where async guidance is not emphasized.
pub const SYNC_ADDENDUM: &str = "REQUIRED: Always use this MCP tool instead of any native terminal. Direct shell invocation is forbidden.";

/// Async operation started hint. Placeholders: {operation_type}, {id}
pub const TOOL_HINT_TEMPLATE: &str = "\n\n\
### AHMA ID: {id} ({operation_type})\n\
This is running in the background \u{2014} NOT complete.\n\
\n\
- Do other work not blocked by `{operation_type}` while it runs.\n\
- **Need results?** Call `await` with this ID. **AVOID POLLING** \u{2014} do not call `status` in a loop.\n\
- **Batch:** Start multiple tools, then `await` all IDs at once.\n\
\n\
Assume success, plan your next step, and summarize for the user. Call `await` when you need this result.\n\n";

/// Premature-wait hint. Placeholders: {id}, {gap_seconds}, {efficiency_percent}
pub const CONCURRENCY_HINT_TEMPLATE: &str = "CONCURRENCY HINT: Waited for '{id}' after \
{gap_seconds:.1}s ({efficiency_percent:.0}% efficiency). Do other work while async ops run.";

/// Status-polling anti-pattern hint. Placeholders: {count}, {id}
pub const STATUS_POLLING_HINT_TEMPLATE: &str = "**POLLING DETECTED:** Called status {count}x \
for '{id}'. Instead, use 'await' \u{2014} it blocks until complete.\n";

/// Standard delay between sequential tool invocations to avoid file lock contention.
/// Particularly important for Cargo operations that may hold Cargo.lock.
pub const SEQUENCE_STEP_DELAY_MS: u64 = 100;

/// How long a `tools/call` waits for its operation to finish before handing back
/// an operation id, **when the session has nothing else running** (SPEC R2.6.1).
///
/// The model has nothing to overlap with here: if this call does not return
/// inline, its next move is `await`, which blocks for at least as long. So the
/// wait is free wall-clock and buys an inline result — saving a whole LLM turn
/// and the context that turn would burn. Clamped by the client's single-request
/// budget ([`McpClientType::request_budget`]).
///
/// [`McpClientType::request_budget`]: crate::client_type::McpClientType::request_budget
pub const INLINE_WINDOW_IDLE_SECS: u64 = 10;

/// The same window **when operations are already in flight** (SPEC R2.6.1).
///
/// Now the model is fanning out, and every second spent holding this response is
/// a second the next command is not yet started. Long enough for genuinely
/// instant commands (`git status`, `ls`) to still answer inline; short enough
/// that a fan-out of slow builds starts concurrently without delay.
pub const INLINE_WINDOW_BUSY_SECS: u64 = 1;

/// Every tool ahma answers itself, by name.
///
/// One list, three consumers that each need it in a different shape, and no
/// way for them to drift apart:
///
/// * `mcp_service::AhmaMcpService::HARDCODED_TOOLS` — dedup filtering and
///   per-call harness-guard name healing, both of which want a cheap
///   `&'static [&'static str]` rather than the full `Tool` values (with JSON
///   schemas) that `builtin_tools()` rebuilds.
/// * `config::RESERVED_TOOL_NAMES` — refuses a configured tool that would
///   collide with one of these, naming the conflict instead of letting the
///   tool load and then silently disappear behind the built-in.
/// * `hardcoded_tools_match_builtin_tools` — asserts this list still matches
///   what `builtin_tools()` actually constructs.
///
/// It was three hand-maintained copies until 2026-09. The protocol-layer copy
/// had drifted twice and was caught by a test; the config-layer copy had
/// drifted a third time and was not, because no test covered it — it was five
/// names short (`logs_approve`, `sandbox_grant`, `agent`, `todo_write`,
/// `log_monitor`), so a workspace tool taking one of those names loaded
/// without complaint and was then filtered out of `tools/list` with no
/// diagnostic at all.
pub const BUILTIN_TOOL_NAMES: &[&str] = &[
    "await",
    "status",
    "run_terminal_command",
    "logs_list",
    "logs_approve",
    "logs_read",
    "logs_search",
    "restart",
    "cancel",
    "sandbox_grant",
    "read_file",
    "list_dir",
    "file_search",
    "grep_search",
    "fetch_webpage",
    "write_file",
    "replace_in_file",
    "agent",
    "todo_write",
    "log_monitor",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::logging::init_test_logging;

    #[test]
    fn async_addendum_contains_key_guidance() {
        init_test_logging();
        assert!(ASYNC_ADDENDUM.contains("MCP tool"));
        assert!(ASYNC_ADDENDUM.contains("terminal"));
        assert!(ASYNC_ADDENDUM.contains("REQUIRED"));
    }

    #[test]
    fn templates_include_placeholders() {
        init_test_logging();
        assert!(TOOL_HINT_TEMPLATE.contains("{operation_type}"));
        assert!(TOOL_HINT_TEMPLATE.contains("{id}"));
        assert!(CONCURRENCY_HINT_TEMPLATE.contains("{id}"));
        assert!(STATUS_POLLING_HINT_TEMPLATE.contains("{id}"));
    }

    #[test]
    fn sequence_step_delay_is_reasonable() {
        init_test_logging();
        const _: () = assert!(
            SEQUENCE_STEP_DELAY_MS >= 50,
            "Delay too short - may not prevent file lock contention"
        );
        const _: () = assert!(
            SEQUENCE_STEP_DELAY_MS <= 500,
            "Delay too long - impacts user experience"
        );
        assert_eq!(
            SEQUENCE_STEP_DELAY_MS, 100,
            "Delay should be 100ms as specified"
        );
    }

    #[test]
    fn inline_windows_are_ordered_and_within_every_client_budget() {
        init_test_logging();
        const _: () = assert!(
            INLINE_WINDOW_BUSY_SECS >= 1,
            "Busy window too short - even instant commands would return an id"
        );
        const _: () = assert!(
            INLINE_WINDOW_BUSY_SECS < INLINE_WINDOW_IDLE_SECS,
            "The busy window must be the shorter one - that is the whole point"
        );
        // The idle window holds a `tools/call` open. It must fit inside the
        // fallback request budget (SPEC R2.6.5) every client shares, or the
        // request that was supposed to save a round-trip kills the session
        // instead.
        let fallback_budget = crate::client_type::McpClientType::Unknown.request_budget();
        assert!(
            std::time::Duration::from_secs(INLINE_WINDOW_IDLE_SECS) < fallback_budget,
            "idle window {INLINE_WINDOW_IDLE_SECS}s must stay under the fallback \
             request budget {fallback_budget:?}"
        );
    }
}
