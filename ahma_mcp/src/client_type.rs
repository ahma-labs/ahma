//! # MCP Client Type Detection
//!
//! This module provides client type detection based on the `Implementation.name` field
//! sent by MCP clients during initialization. It allows the server to adjust behavior
//! based on known client quirks.
//!
//! ## Known Client Issues
//!
//! - **Cursor**: Believed to log errors for progress notifications with unknown
//!   tokens, even when the server correctly uses the client-provided
//!   `progressToken` — asserted, not measured (see
//!   [`McpClientType::supports_progress`](crate::client_type::McpClientType::supports_progress) for why it can't be). To avoid noisy
//!   error logs in Cursor, ahma skips sending progress notifications for this
//!   client by default; `tools.force_progress_notifications` overrides it.
//!
//! - **VSCode/Copilot**: Handles progress notifications correctly.
//!
//! - **Claude Desktop** (`claude-ai`) and **Claude Code** (`claude-code`): handle
//!   progress notifications correctly.

use rmcp::service::{Peer, RoleServer};

/// Represents known MCP client types with their behavioral quirks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpClientType {
    /// Ahma CLI or Ahma IDE integration - supports enhanced heartbeat and payloads.
    Ahma,
    /// Cursor IDE - has issues with progress notification token handling.
    /// We skip progress notifications for this client.
    Cursor,
    /// VS Code / GitHub Copilot - handles progress notifications correctly.
    VSCode,
    /// Claude Desktop (`clientInfo.name` `claude-ai`) - handles progress
    /// notifications correctly.
    ClaudeDesktop,
    /// Claude Code (`clientInfo.name` `claude-code`) - handles progress
    /// notifications correctly and ships its own file tools.
    ClaudeCode,
    /// Zed editor
    Zed,
    /// LM Studio - handles progress notifications correctly.
    LmStudio,
    /// Ollama - handles progress notifications correctly.
    Ollama,
    /// Google Antigravity - abandons the transport partway through a long
    /// `tools/call` (see [`McpClientType::request_budget`]).
    Antigravity,
    /// Unknown client - optimistically assume progress is supported.
    #[default]
    Unknown,
}

impl McpClientType {
    /// Detect client type from the `Implementation.name` field sent during MCP initialization.
    ///
    /// The matching is case-insensitive and looks for known substrings.
    pub fn from_client_name(name: &str) -> Self {
        let name_lower = name.to_lowercase();

        if name_lower.contains("antigravity") || name_lower.starts_with("local-agent-mode") {
            McpClientType::Antigravity
        } else if name_lower == "ahma"
            || name_lower.starts_with("ahma-")
            || name_lower.starts_with("ahma_")
            || name_lower.contains("ahma-cli")
            || name_lower.contains("ahma-ide")
        {
            McpClientType::Ahma
        } else if name_lower.contains("cursor") {
            McpClientType::Cursor
        } else if name_lower.contains("claude-code") || name_lower.contains("claude code") {
            McpClientType::ClaudeCode
        } else if name_lower.contains("claude") {
            McpClientType::ClaudeDesktop
        } else if name_lower.contains("vscode") || name_lower.contains("copilot") {
            McpClientType::VSCode
        } else if name_lower.contains("zed") {
            McpClientType::Zed
        } else if name_lower.contains("lm studio") || name_lower.contains("lmstudio") {
            McpClientType::LmStudio
        } else if name_lower.contains("ollama") {
            McpClientType::Ollama
        } else if name_lower.contains("ahma") {
            McpClientType::Ahma
        } else {
            McpClientType::Unknown
        }
    }

    /// Detect client type from an MCP peer's stored client info.
    ///
    /// Returns `Unknown` if no client info is available.
    pub fn from_peer(peer: &Peer<RoleServer>) -> Self {
        let name = peer.peer_info().map(|info| info.client_info.name.clone());
        let detected = name
            .as_deref()
            .map(Self::from_client_name)
            .unwrap_or(McpClientType::Unknown);
        if detected == McpClientType::Unknown {
            warn_once_unrecognized_client(name.as_deref().unwrap_or("<no client info>"));
        }
        detected
    }

    /// Whether this client correctly handles MCP progress notifications.
    ///
    /// Returns `false` for Cursor, `true` for all other clients (optimistic
    /// default). Unlike [`request_budget`](Self::request_budget) and
    /// [`elicitation_budget`](Self::elicitation_budget) — both set from a
    /// captured session with a timestamped, reproducible measurement — the
    /// Cursor claim ("logs errors for valid progress tokens") has no such
    /// evidence attached anywhere in this codebase's history: it is asserted,
    /// not measured, and MCP notifications are one-way (no response, no ack),
    /// so ahma has no way to observe the failure it's working around even if
    /// it wanted to. It may also be stale — nothing has re-verified it since
    /// it was added. `tools.force_progress_notifications` /
    /// `--force-progress-notifications` let an operator override this
    /// suppression once a given Cursor version is known to have fixed it.
    pub fn supports_progress(&self) -> bool {
        !matches!(self, McpClientType::Cursor)
    }

    /// Whether this client already ships native file read/write/search tools
    /// (`read_file`, `write_file`, `list_dir`, …), so ahma's harness-file
    /// built-ins duplicate a capability the harness itself provides.
    ///
    /// Advertising these unconditionally cost a real incident: a Claude Code
    /// plan-mode subagent that lacked native `Write` used ahma's instead —
    /// see [`crate::builtin_tool::BuiltinTool::is_harness_file_tool`]. The
    /// gate is only ever applied to that subset of tools.
    pub fn has_native_file_tools(&self) -> bool {
        matches!(
            self,
            McpClientType::ClaudeDesktop
                | McpClientType::ClaudeCode
                | McpClientType::Cursor
                | McpClientType::VSCode
        )
    }

    /// Human-readable name for logging.
    pub fn display_name(&self) -> &'static str {
        match self {
            McpClientType::Ahma => "Ahma",
            McpClientType::Cursor => "Cursor",
            McpClientType::VSCode => "VSCode/Copilot",
            McpClientType::ClaudeDesktop => "Claude Desktop",
            McpClientType::ClaudeCode => "Claude Code",
            McpClientType::Zed => "Zed",
            McpClientType::LmStudio => "LM Studio",
            McpClientType::Ollama => "Ollama",
            McpClientType::Antigravity => "Antigravity",
            McpClientType::Unknown => "Unknown",
        }
    }

    /// How long ahma may hold **one** MCP request open for this client before
    /// assuming it has given up, when there is no better signal available
    /// (SPEC R2.6.5, R2.6.5.3).
    ///
    /// This is deliberately no longer a per-`clientInfo.name` guess. It used to
    /// be: Antigravity got 20s from one measured incident (its transport
    /// stopped answering server pings partway through an 85-second `await`,
    /// between 0 and 34 seconds after the request went out — the result was
    /// written into a dead connection), everyone else got 300s on the
    /// optimistic assumption that a named, recognized product is well-behaved.
    /// That was always a proxy for the thing that actually matters — is the
    /// connection *right now* still alive — and a proxy keyed on product name
    /// is exactly as reliable as the guess that produced it: wrong for any
    /// client whose name doesn't match, silently, forever.
    ///
    /// The real answer is `AhmaMcpService::push_channel_open`: when a live
    /// push channel is confirmed, `await` verifies liveness directly with
    /// periodic pings instead of guessing a deadline for it up front (SPEC
    /// R2.6.5.3), and this budget is not consulted at all. This value is only
    /// the fallback for the narrow window before that confirmation arrives, or
    /// for a session that never opens one (a configured default sandbox scope
    /// lets a client skip it entirely, by design) — every client gets the same
    /// conservative number there, because guessing which *product* deserves
    /// more trust was never the right question once a real liveness signal
    /// exists. An operator who knows a specific deployment's fallback window
    /// can actually tolerate longer overrides it via
    /// `tools.request_budget_override_secs` / `--request-budget-secs` (SPEC
    /// R2.6.5.2) rather than ahma guessing on their behalf.
    ///
    pub fn request_budget(&self) -> std::time::Duration {
        const FALLBACK_REQUEST_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);
        FALLBACK_REQUEST_BUDGET
    }

    /// How long ahma may leave an `elicitation/create` prompt open for this
    /// client before the client cancels it out from under us (SPEC R5.3.1).
    ///
    /// Separate from [`request_budget`](Self::request_budget) because the two
    /// deadlines are unrelated: a tool call is bounded by how long the client
    /// will wait on a *response*, while an elicitation is bounded by how long it
    /// will leave a *dialog* up. A human needs far longer to read a path and
    /// choose than a `tools/call` is allowed to take, so reusing the request
    /// budget here would give an answering user seconds and guarantee timeouts.
    ///
    /// Antigravity's 60s is measured: in a captured session it cancelled an
    /// `elicitation/create` at 60.005s while ahma's broker was still waiting on
    /// its own flat 120s. The client resolved the prompt, ahma recorded a
    /// failure, and the harness was demoted for the rest of the session over a
    /// deadline it had never disclosed. Each budget below is therefore set
    /// **strictly under** the client's own, so ahma is the one that resolves the
    /// prompt.
    pub fn elicitation_budget(&self) -> std::time::Duration {
        use std::time::Duration;
        match self {
            // No observed client-side cancellation; a human gets two minutes.
            McpClientType::Ahma
            | McpClientType::ClaudeDesktop
            | McpClientType::ClaudeCode
            | McpClientType::Cursor
            | McpClientType::VSCode
            | McpClientType::Zed => Duration::from_secs(120),
            // Measured cancelling at 60.005s — stay comfortably inside it.
            McpClientType::Antigravity => Duration::from_secs(45),
            // Unmeasured: assume the shortest deadline we have seen anywhere.
            McpClientType::LmStudio | McpClientType::Ollama | McpClientType::Unknown => {
                Duration::from_secs(45)
            }
        }
    }
}

/// Warn, once per distinct unrecognized `clientInfo.name` per process, that a
/// session fell back to the conservative `Unknown` request budget (SPEC
/// R2.6.5). Without this, a client whose self-reported name stops matching
/// any known pattern (a rename, a proxy that rewrites it, a new product)
/// silently drops to a 20s budget with no visible signal anywhere — exactly
/// the kind of degradation a caller only discovers by way of a confusing,
/// truncated `await`. `tools.request_budget_override_secs` is the fix once
/// this is noticed.
///
/// Returns whether this call actually emitted the warning (i.e. `name` was
/// not already seen this process), so the dedup itself is unit-testable
/// without a tracing subscriber.
fn warn_once_unrecognized_client(name: &str) -> bool {
    use parking_lot::Mutex;
    use std::sync::OnceLock;
    static WARNED: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    let warned = WARNED.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
    let mut warned = warned.lock();
    let first_time = warned.insert(name.to_string());
    if first_time {
        tracing::warn!(
            client_name = name,
            "Unrecognized MCP client — falling back to the conservative 20s single-request \
             budget (SPEC R2.6.5). If this client can actually hold a request open longer, \
             set tools.request_budget_override_secs in settings.toml (or --request-budget-secs)."
        );
    }
    first_time
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC R2.6.5.2: the degradation to the conservative default must be
    /// visible, but not spammed — one warning per distinct unrecognized name.
    #[test]
    fn unrecognized_client_warns_once_per_distinct_name_not_per_call() {
        assert!(
            warn_once_unrecognized_client("totally-novel-client-a"),
            "first sighting of a name must warn"
        );
        assert!(
            !warn_once_unrecognized_client("totally-novel-client-a"),
            "repeat sightings of the same name must not warn again"
        );
        assert!(
            warn_once_unrecognized_client("totally-novel-client-b"),
            "a different unrecognized name is a distinct degradation and must warn"
        );
    }

    /// SPEC R5.3.1: the elicitation wait must be strictly under the client's own
    /// undisclosed deadline. Antigravity's is measured at 60.005s; anything at or
    /// above it lets the client resolve the prompt out from under ahma, which is
    /// the failure that demoted a working surface for the rest of the session.
    #[test]
    fn elicitation_budgets_stay_under_the_measured_client_deadline() {
        const ANTIGRAVITY_MEASURED_CANCEL: std::time::Duration =
            std::time::Duration::from_millis(60_005);

        for client in [
            McpClientType::Antigravity,
            McpClientType::LmStudio,
            McpClientType::Ollama,
            McpClientType::Unknown,
        ] {
            let budget = client.elicitation_budget();
            assert!(
                budget < ANTIGRAVITY_MEASURED_CANCEL,
                "{client:?} waits {budget:?}, which is not under the only client \
                 deadline we have actually measured"
            );
        }
    }

    /// The two budgets are deliberately independent: an elicitation is bounded by
    /// how long a client leaves a *dialog* up, a tool call by how long it waits
    /// for a *response*. Reusing the request budget would give an answering human
    /// Antigravity's 20 seconds and guarantee timeouts.
    #[test]
    fn elicitation_budget_is_not_the_request_budget() {
        assert!(
            McpClientType::Antigravity.elicitation_budget()
                > McpClientType::Antigravity.request_budget(),
            "a human needs longer to answer a prompt than a tool call may take"
        );
    }

    #[test]
    fn test_ahma_detection() {
        assert_eq!(McpClientType::from_client_name("ahma"), McpClientType::Ahma);
        assert_eq!(
            McpClientType::from_client_name("ahma-cli"),
            McpClientType::Ahma
        );
    }

    #[test]
    fn test_cursor_detection() {
        assert_eq!(
            McpClientType::from_client_name("cursor"),
            McpClientType::Cursor
        );
        assert_eq!(
            McpClientType::from_client_name("Cursor IDE"),
            McpClientType::Cursor
        );
        assert_eq!(
            McpClientType::from_client_name("CURSOR"),
            McpClientType::Cursor
        );
    }

    #[test]
    fn test_vscode_detection() {
        assert_eq!(
            McpClientType::from_client_name("vscode"),
            McpClientType::VSCode
        );
        assert_eq!(
            McpClientType::from_client_name("VSCode"),
            McpClientType::VSCode
        );
        assert_eq!(
            McpClientType::from_client_name("GitHub Copilot"),
            McpClientType::VSCode
        );
        assert_eq!(
            McpClientType::from_client_name("copilot-chat"),
            McpClientType::VSCode
        );
    }

    #[test]
    fn test_claude_detection() {
        assert_eq!(
            McpClientType::from_client_name("claude-desktop"),
            McpClientType::ClaudeDesktop
        );
        assert_eq!(
            McpClientType::from_client_name("Claude"),
            McpClientType::ClaudeDesktop
        );
        // What Claude Desktop actually sends.
        assert_eq!(
            McpClientType::from_client_name("claude-ai"),
            McpClientType::ClaudeDesktop
        );
    }

    /// Claude Code sends `claude-code`; it used to be labelled Claude Desktop
    /// because any name containing "claude" matched that first.
    #[test]
    fn claude_code_is_not_claude_desktop() {
        let t = McpClientType::from_client_name("claude-code");
        assert_eq!(t, McpClientType::ClaudeCode);
        assert_eq!(t.display_name(), "Claude Code");
        assert!(t.has_native_file_tools());
        assert!(t.supports_progress());
        assert_eq!(
            t.elicitation_budget(),
            McpClientType::ClaudeDesktop.elicitation_budget()
        );
    }

    #[test]
    fn test_zed_detection() {
        assert_eq!(McpClientType::from_client_name("zed"), McpClientType::Zed);
        assert_eq!(
            McpClientType::from_client_name("Zed Editor"),
            McpClientType::Zed
        );
    }

    #[test]
    fn test_lmstudio_detection() {
        assert_eq!(
            McpClientType::from_client_name("lmstudio"),
            McpClientType::LmStudio
        );
        assert_eq!(
            McpClientType::from_client_name("LM Studio"),
            McpClientType::LmStudio
        );
        assert_eq!(
            McpClientType::from_client_name("lmstudio-mcp"),
            McpClientType::LmStudio
        );
    }

    #[test]
    fn test_ollama_detection() {
        assert_eq!(
            McpClientType::from_client_name("ollama"),
            McpClientType::Ollama
        );
        assert_eq!(
            McpClientType::from_client_name("Ollama"),
            McpClientType::Ollama
        );
    }

    #[test]
    fn test_unknown_detection() {
        assert_eq!(
            McpClientType::from_client_name("some-other-client"),
            McpClientType::Unknown
        );
        assert_eq!(McpClientType::from_client_name(""), McpClientType::Unknown);
    }

    #[test]
    fn test_supports_progress() {
        // Cursor does NOT support progress (logs errors)
        assert!(!McpClientType::Cursor.supports_progress());

        // All others support progress
        assert!(McpClientType::VSCode.supports_progress());
        assert!(McpClientType::ClaudeDesktop.supports_progress());
        assert!(McpClientType::Zed.supports_progress());
        assert!(McpClientType::LmStudio.supports_progress());
        assert!(McpClientType::Ollama.supports_progress());
        assert!(McpClientType::Unknown.supports_progress());
    }

    #[test]
    fn test_default_is_unknown() {
        assert_eq!(McpClientType::default(), McpClientType::Unknown);
    }

    #[test]
    fn antigravity_is_detected_from_its_client_name() {
        // The name it actually sends, from a captured session.
        assert_eq!(
            McpClientType::from_client_name("antigravity-client"),
            McpClientType::Antigravity
        );
        assert_eq!(
            McpClientType::from_client_name("Antigravity"),
            McpClientType::Antigravity
        );
        assert_eq!(
            McpClientType::from_client_name("local-agent-mode-Ahma"),
            McpClientType::Antigravity
        );
    }

    #[test]
    fn every_client_shares_the_same_fallback_request_budget() {
        // SPEC R2.6.5: request_budget() is only the fallback for the window
        // before a live push channel is confirmed (or a session that never
        // opens one) — a real liveness probe (SPEC R2.6.5.3) is the actual
        // answer once one exists. Guessing which *product* deserves more trust
        // in that fallback window was never the right question, so every
        // client — recognized or not — gets the identical conservative number.
        let expected = McpClientType::Unknown.request_budget();
        for client in [
            McpClientType::Ahma,
            McpClientType::Cursor,
            McpClientType::VSCode,
            McpClientType::ClaudeDesktop,
            McpClientType::Zed,
            McpClientType::LmStudio,
            McpClientType::Ollama,
            McpClientType::Antigravity,
            McpClientType::Unknown,
        ] {
            assert_eq!(
                client.request_budget(),
                expected,
                "{} must share the uniform fallback budget",
                client.display_name()
            );
        }
    }

    #[test]
    fn every_budget_is_long_enough_to_be_useful() {
        use std::time::Duration;
        for client in [
            McpClientType::Ahma,
            McpClientType::Cursor,
            McpClientType::VSCode,
            McpClientType::ClaudeDesktop,
            McpClientType::Zed,
            McpClientType::LmStudio,
            McpClientType::Ollama,
            McpClientType::Antigravity,
            McpClientType::Unknown,
        ] {
            let budget = client.request_budget();
            assert!(
                budget >= Duration::from_secs(10),
                "{} budget {budget:?} is too small for any real command",
                client.display_name()
            );
            // The HTTP bridge guillotines a tools/call at its ceiling, so a
            // budget above that would promise something the transport cannot
            // keep.
            assert!(
                budget <= Duration::from_secs(ahma_common::timeouts::BRIDGE_TOOL_CALL_CEILING_SECS),
                "{} budget {budget:?} exceeds the bridge's tools/call ceiling",
                client.display_name()
            );
        }
    }
}
