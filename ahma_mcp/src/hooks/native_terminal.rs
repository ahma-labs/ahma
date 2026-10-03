//! Whether a client's own terminal runs inside ahma's sandbox (SPEC R7.8).
//!
//! ahma confines what it runs: its MCP tools, and a harness's native shell
//! tool when ahma's terminal hook is installed and active for that harness.
//! Everything else a client runs in its own terminal is outside every
//! boundary ahma has. That was stated only in the SPEC; `status` and the TUI
//! session list now say it for the client actually connected.

use std::path::Path;

use ahma_common::harness::Harness;

use crate::client_type::McpClientType;

use super::{HookPlatform, HookScope};

/// What confines a client's own terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeTerminal {
    /// ahma's terminal hook is installed and active for this client: its own
    /// shell commands run inside ahma's sandbox too.
    Hooked,
    /// This client's own terminal runs outside ahma's sandbox: the client has
    /// no hook ahma can install, or it is not installed or not active.
    Unconfined,
}

impl NativeTerminal {
    /// The word carried to the hub and the TUI.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Hooked => "hooked",
            Self::Unconfined => "unconfined",
        }
    }

    /// Read the word carried to the hub back.
    pub fn from_wire(word: &str) -> Option<Self> {
        match word {
            "hooked" => Some(Self::Hooked),
            "unconfined" => Some(Self::Unconfined),
            _ => None,
        }
    }

    /// The `status` line for this process's connected client, when known.
    pub fn status_line_for_this_session() -> Option<String> {
        let id = crate::hub_reporter::current_identity();
        let native = Self::from_wire(id.native_terminal.as_deref()?)?;
        let who = McpClientType::from_client_name(id.client.as_deref()?).display_name();
        Some(native.status_line(who))
    }

    /// The `status` line for client `who`.
    pub fn status_line(self, who: &str) -> String {
        match self {
            Self::Hooked => format!(
                "{who}'s own terminal: inside ahma's sandbox too (ahma's terminal hook is \
                 installed and active)."
            ),
            Self::Unconfined => format!(
                "{who}'s own terminal: OUTSIDE ahma's sandbox. Only commands sent through \
                 ahma are confined; a command {who} runs in its own terminal is not."
            ),
        }
    }
}

/// The hook platform for an MCP client: `Some(None)` for a client ahma has no
/// hook for, `None` for a client it cannot identify (nothing is claimed).
///
/// ahma itself is not described either: it has no terminal of its own for
/// ahma to confine.
fn hook_platform_of(client: McpClientType) -> Option<Option<HookPlatform>> {
    match client.harness()? {
        Harness::Ahma => None,
        harness => Some(HookPlatform::of(harness)),
    }
}

/// Classify from the facts, so the rule is testable without real files.
fn classify(
    client: McpClientType,
    installed: &dyn Fn(HookPlatform) -> bool,
    active: bool,
) -> Option<NativeTerminal> {
    let confined = match hook_platform_of(client)? {
        Some(platform) => active && installed(platform),
        None => false,
    };
    Some(if confined {
        NativeTerminal::Hooked
    } else {
        NativeTerminal::Unconfined
    })
}

/// What confines `client`'s own terminal, for a session in `workspace`: its
/// hook installed at user or project scope, and hooks active.
pub fn native_terminal_for(client: McpClientType, workspace: &Path) -> Option<NativeTerminal> {
    let home = dirs::home_dir()?;
    let installed = |platform: HookPlatform| {
        [
            (HookScope::User, home.as_path()),
            (HookScope::Project, workspace),
        ]
        .into_iter()
        .any(|(scope, root)| {
            super::load_hook_document(&platform.config_path(root, scope))
                .is_ok_and(|doc| super::platform_hook_installed(&doc, platform))
        })
    };
    classify(client, &installed, super::is_ahma_hooks_active())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_without_a_hook_runs_its_terminal_outside_the_sandbox() {
        for client in [
            McpClientType::VSCode,
            McpClientType::ClaudeDesktop,
            McpClientType::Zed,
            McpClientType::LmStudio,
            McpClientType::Ollama,
        ] {
            assert_eq!(
                classify(client, &|_| true, true),
                Some(NativeTerminal::Unconfined),
                "{client:?}"
            );
        }
    }

    #[test]
    fn a_hooked_client_is_confined_only_when_installed_and_active() {
        let claude = McpClientType::ClaudeCode;
        assert_eq!(
            classify(claude, &|_| true, true),
            Some(NativeTerminal::Hooked)
        );
        assert_eq!(
            classify(claude, &|_| false, true),
            Some(NativeTerminal::Unconfined),
            "not installed"
        );
        assert_eq!(
            classify(claude, &|_| true, false),
            Some(NativeTerminal::Unconfined),
            "installed but inactive passes commands through"
        );
        assert_eq!(
            classify(McpClientType::Cursor, &|p| p == HookPlatform::Cursor, true),
            Some(NativeTerminal::Hooked),
            "the client's own hook counts, not another's"
        );
        assert_eq!(
            classify(McpClientType::Cursor, &|p| p == HookPlatform::Claude, true),
            Some(NativeTerminal::Unconfined)
        );
    }

    /// The pre-unification table, pinned for every client type.
    #[test]
    fn hook_platform_of_every_client_type_is_pinned() {
        let expected = [
            (McpClientType::ClaudeCode, Some(Some(HookPlatform::Claude))),
            (McpClientType::Cursor, Some(Some(HookPlatform::Cursor))),
            (
                McpClientType::Antigravity,
                Some(Some(HookPlatform::Antigravity)),
            ),
            (McpClientType::VSCode, Some(None)),
            (McpClientType::ClaudeDesktop, Some(None)),
            (McpClientType::Zed, Some(None)),
            (McpClientType::LmStudio, Some(None)),
            (McpClientType::Ollama, Some(None)),
            (McpClientType::Ahma, None),
            (McpClientType::Unknown, None),
        ];
        for (client, hook) in expected {
            assert_eq!(hook_platform_of(client), hook, "{client:?}");
        }
    }

    #[test]
    fn an_unknown_client_is_not_described() {
        assert_eq!(classify(McpClientType::Unknown, &|_| true, true), None);
        assert_eq!(classify(McpClientType::Ahma, &|_| true, true), None);
    }

    #[test]
    fn the_unconfined_line_says_outside_plainly() {
        let line = NativeTerminal::Unconfined.status_line("VS Code");
        assert!(line.contains("OUTSIDE ahma's sandbox"), "{line}");
        assert!(
            line.contains("Only commands sent through ahma are confined"),
            "{line}"
        );
    }
}
