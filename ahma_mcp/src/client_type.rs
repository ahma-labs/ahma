//! # MCP Client Type Detection
//!
//! This module provides client type detection based on the `Implementation.name` field
//! sent by MCP clients during initialization. It allows the server to adjust behavior
//! based on known client quirks.
//!
//! ## Known Client Issues
//!
//! - **Cursor**: Logs errors for progress notifications with unknown tokens, even when
//!   the server correctly uses the client-provided `progressToken`. To avoid noisy
//!   error logs in Cursor, we skip sending progress notifications entirely for this client.
//!
//! - **VSCode/Copilot**: Handles progress notifications correctly.
//!
//! - **Claude Desktop**: Handles progress notifications correctly.

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
    /// Claude Desktop - handles progress notifications correctly.
    ClaudeDesktop,
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

        if name_lower.contains("ahma") {
            McpClientType::Ahma
        } else if name_lower.contains("cursor") {
            McpClientType::Cursor
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
        } else if name_lower.contains("antigravity") {
            McpClientType::Antigravity
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
    /// Returns `false` for Cursor (which logs errors for valid progress tokens),
    /// and `true` for all other clients (optimistic default).
    pub fn supports_progress(&self) -> bool {
        !matches!(self, McpClientType::Cursor)
    }

    /// Human-readable name for logging.
    pub fn display_name(&self) -> &'static str {
        match self {
            McpClientType::Ahma => "Ahma",
            McpClientType::Cursor => "Cursor",
            McpClientType::VSCode => "VSCode/Copilot",
            McpClientType::ClaudeDesktop => "Claude Desktop",
            McpClientType::Zed => "Zed",
            McpClientType::LmStudio => "LM Studio",
            McpClientType::Ollama => "Ollama",
            McpClientType::Antigravity => "Antigravity",
            McpClientType::Unknown => "Unknown",
        }
    }

    /// How long ahma may hold **one** MCP request open for this client before
    /// the client gives up on it (SPEC R2.6.5).
    ///
    /// MCP has no way to discover this, so it is a table keyed on
    /// `clientInfo.name` with a conservative default. Two behaviours derive
    /// from it: the ceiling on the inline result window (R2.6.1) and the
    /// `await` soft timeout (R2.5.1). Get it wrong high and a long call takes
    /// the session down with it; get it wrong low and the model pays extra
    /// round-trips. Prefer wrong-low.
    ///
    /// Antigravity's 20s is measured, not guessed: in a captured session its
    /// transport stopped answering server pings partway through an 85-second
    /// `await`, between 0 and 34 seconds after the request went out — the
    /// result was written into a dead connection. Clients not on this list get
    /// the same conservative budget until one is measured for them.
    pub fn request_budget(&self) -> std::time::Duration {
        use std::time::Duration;
        match self {
            // Purpose-built for long tool calls; ahma's own surfaces likewise.
            McpClientType::Ahma
            | McpClientType::ClaudeDesktop
            | McpClientType::Cursor
            | McpClientType::VSCode
            | McpClientType::Zed => Duration::from_secs(300),
            // Measured, and the reason this table exists.
            McpClientType::Antigravity => Duration::from_secs(20),
            // Local-model front-ends and anything unrecognised: assume little.
            McpClientType::LmStudio | McpClientType::Ollama | McpClientType::Unknown => {
                Duration::from_secs(20)
            }
        }
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
    use std::sync::{Mutex, OnceLock};
    static WARNED: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    let warned = WARNED.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
    let mut warned = warned.lock().unwrap_or_else(|e| e.into_inner());
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
    }

    #[test]
    fn unrecognised_clients_get_the_conservative_budget() {
        // Guessing high costs the session; guessing low costs a round-trip.
        assert_eq!(
            McpClientType::Unknown.request_budget(),
            McpClientType::Antigravity.request_budget(),
            "an unmeasured client must not be assumed tolerant"
        );
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
            // The HTTP bridge guillotines a tools/call at 600s, so a budget
            // above that would promise something the transport cannot keep.
            assert!(
                budget <= Duration::from_secs(600),
                "{} budget {budget:?} exceeds the bridge's tools/call ceiling",
                client.display_name()
            );
        }
    }
}
