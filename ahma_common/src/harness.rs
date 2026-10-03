//! Every AI harness ahma knows about, listed once, and the facts that describe
//! each.
//!
//! "Which client is this?" used to be answered by separate tables that
//! disagreed: the MCP client type (`ahma_mcp::client_type::McpClientType`),
//! the `ahma setup` targets (`ahma_mcp::harness_target::Platform`) and the
//! terminal-hook flavours (`ahma_mcp::hooks::HookPlatform`) each listed their
//! own subset with their own names. They are now views of this enum: each one
//! maps its variants onto a [`Harness`](crate::harness::Harness) and reads the facts from here, so a
//! harness cannot be "Claude Code" in one table and something else in another.
//!
//! What stays out of this module is what a *surface* does with a harness —
//! config-file paths and merge formats (`harness_target`), hook payload
//! dialects (`hooks`), sandbox-host detection (`sandbox::host_detect`). Those
//! are mechanisms; this is identity.
//!
//! Some facts below are only observable for some harnesses today: a harness
//! [`Harness::from_client_name`](crate::harness::Harness::from_client_name) never returns has no MCP-client behaviour
//! anyone can see, and one that is not an `ahma setup` target has no setup
//! label anyone can see. Their values are still filled in so every method is
//! total — and they are chosen to equal what ahma does *today* for that
//! harness, not what it ideally should (see each method).

use std::time::Duration;

/// An AI harness (MCP client, agent CLI or IDE) ahma can recognise, configure
/// or hook. Alphabetical by [`label`](Self::label), except [`Harness::Ahma`]
/// first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Harness {
    /// ahma itself: the ahma CLI, an ahma IDE integration, or ahma's
    /// in-process autonomous agent connecting to its own server.
    Ahma,
    /// Google Antigravity (IDE and the `agy` CLI).
    Antigravity,
    /// Claude Code (`clientInfo.name` `claude-code`).
    ClaudeCode,
    /// Claude Desktop (`clientInfo.name` `claude-ai`).
    ClaudeDesktop,
    /// OpenAI Codex CLI.
    Codex,
    /// GitHub Copilot CLI (the terminal agent, not Copilot Chat in VS Code).
    CopilotCli,
    /// Cursor.
    Cursor,
    /// Google Gemini CLI. Listed so it has one name; today a Gemini CLI
    /// session is *detected* as `agy` (Antigravity) — `GEMINI_CLI` in the
    /// environment makes the stdio proxy report `agy` — and
    /// [`Harness::from_client_name`](crate::harness::Harness::from_client_name) never returns it.
    GeminiCli,
    /// LM Studio.
    LmStudio,
    /// Ollama.
    Ollama,
    /// VS Code with GitHub Copilot Chat.
    VsCode,
    /// Zed.
    Zed,
}

impl Harness {
    /// Every harness, once.
    pub const ALL: &'static [Harness] = &[
        Harness::Ahma,
        Harness::Antigravity,
        Harness::ClaudeCode,
        Harness::ClaudeDesktop,
        Harness::Codex,
        Harness::CopilotCli,
        Harness::Cursor,
        Harness::GeminiCli,
        Harness::LmStudio,
        Harness::Ollama,
        Harness::VsCode,
        Harness::Zed,
    ];

    /// The product's name, as `ahma setup`, `ahma uninstall` and `ahma hooks`
    /// print it.
    pub fn label(self) -> &'static str {
        match self {
            Harness::Ahma => "Ahma",
            Harness::Antigravity => "Antigravity",
            Harness::ClaudeCode => "Claude Code",
            Harness::ClaudeDesktop => "Claude Desktop",
            Harness::Codex => "Codex",
            Harness::CopilotCli => "GitHub Copilot CLI",
            Harness::Cursor => "Cursor",
            Harness::GeminiCli => "Gemini CLI",
            Harness::LmStudio => "LM Studio",
            Harness::Ollama => "Ollama",
            Harness::VsCode => "VS Code (GitHub Copilot Chat)",
            Harness::Zed => "Zed",
        }
    }

    /// The value `--platform` accepts for this harness, in both `ahma setup`
    /// and `ahma hooks`. One spelling per harness, so the two flags cannot
    /// disagree about what `claude` means.
    pub fn cli_name(self) -> &'static str {
        match self {
            Harness::Ahma => "ahma",
            Harness::Antigravity => "antigravity",
            Harness::ClaudeCode => "claude",
            Harness::ClaudeDesktop => "claude-desktop",
            Harness::Codex => "codex",
            Harness::CopilotCli => "copilot",
            Harness::Cursor => "cursor",
            Harness::GeminiCli => "gemini",
            Harness::LmStudio => "lmstudio",
            Harness::Ollama => "ollama",
            Harness::VsCode => "vscode",
            Harness::Zed => "zed",
        }
    }

    /// The name used for a *connected MCP client* in logs, `status` and the
    /// TUI. Identical to [`label`](Self::label) except VS Code, which has
    /// always been logged as "VSCode/Copilot" because every `clientInfo.name`
    /// containing `copilot` is classified as VS Code.
    pub fn client_display_name(self) -> &'static str {
        match self {
            Harness::VsCode => "VSCode/Copilot",
            other => other.label(),
        }
    }

    /// Recognise a harness from the `clientInfo.name` it sends in MCP
    /// `initialize`. Case-insensitive substring matching; `None` for a name
    /// that matches nothing.
    ///
    /// Order matters: Antigravity before everything (it can embed other names,
    /// e.g. `local-agent-mode-Ahma`), the exact ahma spellings before `cursor`,
    /// `claude-code` before the bare `claude` of Claude Desktop, and the loose
    /// `ahma` substring last. Never returns [`Harness::Codex`],
    /// [`Harness::CopilotCli`] or [`Harness::GeminiCli`]: a name containing
    /// `copilot` is classified as [`Harness::VsCode`], and the other two have
    /// no recognised name yet.
    pub fn from_client_name(name: &str) -> Option<Self> {
        let name_lower = name.to_lowercase();

        if name_lower.contains("antigravity")
            || name_lower.starts_with("local-agent-mode")
            || name_lower == "agy"
            || name_lower.starts_with("agy-")
            || name_lower.starts_with("agy ")
        {
            Some(Harness::Antigravity)
        } else if name_lower == "ahma"
            || name_lower.starts_with("ahma-")
            || name_lower.starts_with("ahma_")
            || name_lower.contains("ahma-cli")
            || name_lower.contains("ahma-ide")
        {
            Some(Harness::Ahma)
        } else if name_lower.contains("cursor") {
            Some(Harness::Cursor)
        } else if name_lower.contains("claude-code") || name_lower.contains("claude code") {
            Some(Harness::ClaudeCode)
        } else if name_lower.contains("claude") {
            Some(Harness::ClaudeDesktop)
        } else if name_lower.contains("vscode") || name_lower.contains("copilot") {
            Some(Harness::VsCode)
        } else if name_lower.contains("zed") {
            Some(Harness::Zed)
        } else if name_lower.contains("lm studio") || name_lower.contains("lmstudio") {
            Some(Harness::LmStudio)
        } else if name_lower.contains("ollama") {
            Some(Harness::Ollama)
        } else if name_lower.contains("ahma") {
            Some(Harness::Ahma)
        } else {
            None
        }
    }

    /// Whether this harness handles MCP progress notifications. `false` only
    /// for Cursor — asserted, not measured; see
    /// `ahma_mcp::client_type::McpClientType::supports_progress` for the
    /// caveat and the override.
    pub fn supports_progress(self) -> bool {
        !matches!(self, Harness::Cursor)
    }

    /// Whether this harness ships its own file read/write/search tools, so
    /// ahma hides its harness-file built-ins from it.
    ///
    /// Codex and Gemini CLI answer `false` although both have native file
    /// tools: neither is ever recognised as a connected client today, so
    /// they get the unrecognised-client answer, which is what ahma actually
    /// does for them. Change that together with recognising them.
    pub fn has_native_file_tools(self) -> bool {
        matches!(
            self,
            Harness::ClaudeDesktop
                | Harness::ClaudeCode
                | Harness::CopilotCli
                | Harness::Cursor
                | Harness::VsCode
        )
    }

    /// How long ahma may leave an `elicitation/create` prompt open before this
    /// harness cancels it (SPEC R5.3.1): strictly under the shortest client
    /// deadline ever measured (Antigravity, 60.005s) wherever there is no
    /// evidence the client waits longer.
    pub fn elicitation_budget(self) -> Duration {
        match self {
            // No observed client-side cancellation; a human gets two minutes.
            // Copilot CLI sessions are classified as VS Code (see
            // `from_client_name`), so they get VS Code's answer.
            Harness::Ahma
            | Harness::ClaudeDesktop
            | Harness::ClaudeCode
            | Harness::CopilotCli
            | Harness::Cursor
            | Harness::VsCode
            | Harness::Zed => Duration::from_secs(120),
            // Measured cancelling at 60.005s — stay comfortably inside it.
            Harness::Antigravity => Duration::from_secs(45),
            // Unmeasured: assume the shortest deadline we have seen anywhere.
            Harness::Codex | Harness::GeminiCli | Harness::LmStudio | Harness::Ollama => {
                Duration::from_secs(45)
            }
        }
    }

    /// Whether this harness answers `roots/list`. One that does not must be
    /// given an explicitly scoped server entry by `ahma setup` — ahma cannot
    /// discover the workspace from it.
    pub fn sends_roots_list(self) -> bool {
        !matches!(self, Harness::Antigravity | Harness::LmStudio)
    }

    /// Whether ahma has a terminal hook for this harness's native shell tool
    /// (`ahma hooks install --platform <cli_name>`). The hook's payload
    /// dialect and config paths live with the hook code in `ahma_mcp`.
    pub fn has_terminal_hook(self) -> bool {
        matches!(
            self,
            Harness::Antigravity
                | Harness::ClaudeCode
                | Harness::Codex
                | Harness::CopilotCli
                | Harness::Cursor
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every user-visible string, pinned. These reach users through `ahma
    /// setup`/`uninstall`/`hooks` output, `--platform` values, `status`, the
    /// TUI and logs; a rename here is a behaviour change.
    #[test]
    fn every_name_is_pinned() {
        let expected: &[(Harness, &str, &str, &str)] = &[
            (Harness::Ahma, "Ahma", "ahma", "Ahma"),
            (
                Harness::Antigravity,
                "Antigravity",
                "antigravity",
                "Antigravity",
            ),
            (Harness::ClaudeCode, "Claude Code", "claude", "Claude Code"),
            (
                Harness::ClaudeDesktop,
                "Claude Desktop",
                "claude-desktop",
                "Claude Desktop",
            ),
            (Harness::Codex, "Codex", "codex", "Codex"),
            (
                Harness::CopilotCli,
                "GitHub Copilot CLI",
                "copilot",
                "GitHub Copilot CLI",
            ),
            (Harness::Cursor, "Cursor", "cursor", "Cursor"),
            (Harness::GeminiCli, "Gemini CLI", "gemini", "Gemini CLI"),
            (Harness::LmStudio, "LM Studio", "lmstudio", "LM Studio"),
            (Harness::Ollama, "Ollama", "ollama", "Ollama"),
            (
                Harness::VsCode,
                "VS Code (GitHub Copilot Chat)",
                "vscode",
                "VSCode/Copilot",
            ),
            (Harness::Zed, "Zed", "zed", "Zed"),
        ];
        assert_eq!(expected.len(), Harness::ALL.len());
        for &(h, label, cli, display) in expected {
            assert_eq!(h.label(), label, "{h:?} label");
            assert_eq!(h.cli_name(), cli, "{h:?} cli_name");
            assert_eq!(
                h.client_display_name(),
                display,
                "{h:?} client_display_name"
            );
        }
    }

    #[test]
    fn all_lists_every_harness_once_with_unique_names() {
        for (i, a) in Harness::ALL.iter().enumerate() {
            for b in &Harness::ALL[i + 1..] {
                assert_ne!(a, b);
                assert_ne!(a.label(), b.label());
                assert_ne!(a.cli_name(), b.cli_name());
            }
        }
    }

    /// The `clientInfo.name` table, case by case — the same cases
    /// `McpClientType`'s tests pin, plus the ordering traps.
    #[test]
    fn client_names_are_recognised() {
        let cases: &[(&str, Option<Harness>)] = &[
            ("ahma", Some(Harness::Ahma)),
            ("ahma-cli", Some(Harness::Ahma)),
            ("ahma_ide", Some(Harness::Ahma)),
            ("my-ahma-ide", Some(Harness::Ahma)),
            ("cursor", Some(Harness::Cursor)),
            ("Cursor IDE", Some(Harness::Cursor)),
            ("CURSOR", Some(Harness::Cursor)),
            ("vscode", Some(Harness::VsCode)),
            ("VSCode", Some(Harness::VsCode)),
            ("GitHub Copilot", Some(Harness::VsCode)),
            ("copilot-chat", Some(Harness::VsCode)),
            ("claude-desktop", Some(Harness::ClaudeDesktop)),
            ("Claude", Some(Harness::ClaudeDesktop)),
            ("claude-ai", Some(Harness::ClaudeDesktop)),
            ("claude-code", Some(Harness::ClaudeCode)),
            ("Claude Code", Some(Harness::ClaudeCode)),
            ("zed", Some(Harness::Zed)),
            ("Zed Editor", Some(Harness::Zed)),
            ("lmstudio", Some(Harness::LmStudio)),
            ("LM Studio", Some(Harness::LmStudio)),
            ("lmstudio-mcp", Some(Harness::LmStudio)),
            ("ollama", Some(Harness::Ollama)),
            ("Ollama", Some(Harness::Ollama)),
            ("antigravity-client", Some(Harness::Antigravity)),
            ("Antigravity", Some(Harness::Antigravity)),
            ("Antigravity IDE", Some(Harness::Antigravity)),
            ("local-agent-mode-Ahma", Some(Harness::Antigravity)),
            ("agy", Some(Harness::Antigravity)),
            ("agy-cli", Some(Harness::Antigravity)),
            ("some-other-client", None),
            ("", None),
        ];
        for &(name, want) in cases {
            assert_eq!(Harness::from_client_name(name), want, "{name:?}");
        }
    }

    /// Codex, Copilot CLI and Gemini CLI are not recognised by name today.
    /// Their own names must not quietly start matching something else: that
    /// would change their budgets and visible tools without anyone deciding.
    #[test]
    fn unrecognised_harnesses_stay_unrecognised_by_their_own_names() {
        for h in [Harness::Codex, Harness::GeminiCli] {
            assert_eq!(Harness::from_client_name(h.cli_name()), None, "{h:?}");
            assert_eq!(Harness::from_client_name(h.label()), None, "{h:?}");
        }
        // Copilot CLI's names contain "copilot", which is VS Code's.
        assert_eq!(
            Harness::from_client_name(Harness::CopilotCli.label()),
            Some(Harness::VsCode)
        );
    }

    #[test]
    fn capability_table_is_pinned() {
        // (harness, progress, native file tools, elicitation secs, roots, hook)
        let expected: &[(Harness, bool, bool, u64, bool, bool)] = &[
            (Harness::Ahma, true, false, 120, true, false),
            (Harness::Antigravity, true, false, 45, false, true),
            (Harness::ClaudeCode, true, true, 120, true, true),
            (Harness::ClaudeDesktop, true, true, 120, true, false),
            (Harness::Codex, true, false, 45, true, true),
            (Harness::CopilotCli, true, true, 120, true, true),
            (Harness::Cursor, false, true, 120, true, true),
            (Harness::GeminiCli, true, false, 45, true, false),
            (Harness::LmStudio, true, false, 45, false, false),
            (Harness::Ollama, true, false, 45, true, false),
            (Harness::VsCode, true, true, 120, true, false),
            (Harness::Zed, true, false, 120, true, false),
        ];
        assert_eq!(expected.len(), Harness::ALL.len());
        for &(h, progress, files, elicit, roots, hook) in expected {
            assert_eq!(h.supports_progress(), progress, "{h:?} supports_progress");
            assert_eq!(
                h.has_native_file_tools(),
                files,
                "{h:?} has_native_file_tools"
            );
            assert_eq!(
                h.elicitation_budget(),
                Duration::from_secs(elicit),
                "{h:?} elicitation_budget"
            );
            assert_eq!(h.sends_roots_list(), roots, "{h:?} sends_roots_list");
            assert_eq!(h.has_terminal_hook(), hook, "{h:?} has_terminal_hook");
        }
    }

    /// SPEC R5.3.1: every budget stays under the only client deadline ever
    /// measured, except where a client is known to wait longer.
    #[test]
    fn unmeasured_elicitation_budgets_stay_under_the_measured_deadline() {
        let measured = Duration::from_millis(60_005);
        for h in [
            Harness::Antigravity,
            Harness::Codex,
            Harness::GeminiCli,
            Harness::LmStudio,
            Harness::Ollama,
        ] {
            assert!(h.elicitation_budget() < measured, "{h:?}");
        }
    }
}
