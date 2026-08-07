//! The set of AI harnesses ahma can configure, and the facts that describe each.
//!
//! Setup and uninstall are mirror images: whatever setup writes, uninstall must
//! find and remove. They used to hold two independent copies of the same
//! `Platform` enum, the same platform table, and the same per-platform config
//! paths, so that symmetry was a matter of discipline rather than structure —
//! and the copies had already drifted (setup resolved the home directory with
//! `dirs::home_dir()` while uninstall used [`ahma_common::config::ahma_home_dir`],
//! so under `AHMA_TEST_HOME` the two could target *different files*).
//!
//! Everything a harness *is* lives here; what setup and uninstall *do* with it
//! stays in their own modules. Adding a harness means adding one variant and
//! filling in the matches the compiler then flags — uninstall cannot be
//! forgotten.

use std::path::{Path, PathBuf};

use crate::hooks::HookPlatform;

/// An AI tool ahma can configure. Listed in alphabetical order (by label) for
/// uniform, simple presentation. Not every harness supports every action:
/// GitHub Copilot has no MCP config target here; VS Code, Claude Desktop, and
/// LM Studio are configured via MCP only (no terminal hook wrapper).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Platform {
    Antigravity,
    ClaudeCode,
    ClaudeDesktop,
    Codex,
    Cursor,
    Copilot,
    LmStudio,
    VsCode,
}

/// Every harness, in the order the wizards present them.
pub const PLATFORMS: &[Platform] = &[
    Platform::Antigravity,
    Platform::ClaudeCode,
    Platform::ClaudeDesktop,
    Platform::Codex,
    Platform::Cursor,
    Platform::Copilot,
    Platform::LmStudio,
    Platform::VsCode,
];

/// How a harness stores its MCP server map.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum McpConfigFormat {
    /// A JSON file whose server map lives under this top-level key.
    Json(&'static str),
    /// Codex's `config.toml`, which needs TOML-aware merging.
    Toml,
}

impl Platform {
    /// Name shown in the wizards' platform menus.
    pub fn label(self) -> &'static str {
        match self {
            Platform::Antigravity => "Antigravity",
            Platform::ClaudeCode => "Claude Code",
            Platform::ClaudeDesktop => "Claude Desktop",
            Platform::Codex => "Codex",
            Platform::Cursor => "Cursor",
            Platform::Copilot => "GitHub Copilot CLI",
            Platform::LmStudio => "LM Studio",
            Platform::VsCode => "VS Code (GitHub Copilot Chat)",
        }
    }

    /// Value accepted by `--platform`.
    pub fn cli_name(self) -> &'static str {
        match self {
            Platform::Antigravity => "antigravity",
            Platform::ClaudeCode => "claude",
            Platform::ClaudeDesktop => "claude-desktop",
            Platform::Codex => "codex",
            Platform::Cursor => "cursor",
            Platform::Copilot => "copilot",
            Platform::LmStudio => "lmstudio",
            Platform::VsCode => "vscode",
        }
    }

    /// Name reported after an MCP entry is written or removed.
    ///
    /// Identical to [`Self::label`] except for Codex, which both wizards have
    /// always reported as "Codex CLI" to distinguish it from the Codex IDE
    /// extension.
    pub fn mcp_display_name(self) -> &'static str {
        match self {
            Platform::Codex => "Codex CLI",
            other => other.label(),
        }
    }

    pub fn supports_mcp(self) -> bool {
        !matches!(self, Platform::Copilot)
    }

    pub fn supports_hooks(self) -> bool {
        !matches!(
            self,
            Platform::VsCode | Platform::ClaudeDesktop | Platform::LmStudio
        )
    }

    /// The terminal-hook flavour for this harness, if it has one.
    pub fn hook_platform(self) -> Option<HookPlatform> {
        match self {
            Platform::Antigravity => Some(HookPlatform::Antigravity),
            Platform::ClaudeCode => Some(HookPlatform::Claude),
            Platform::ClaudeDesktop => None,
            Platform::Codex => Some(HookPlatform::Codex),
            Platform::Cursor => Some(HookPlatform::Cursor),
            Platform::Copilot => Some(HookPlatform::Copilot),
            Platform::LmStudio => None,
            Platform::VsCode => None,
        }
    }

    /// Whether this harness answers `roots/list`.
    ///
    /// Harnesses that don't must be given an explicitly scoped server entry —
    /// ahma cannot discover the workspace from them.
    pub fn sends_roots_list(self) -> bool {
        !matches!(self, Platform::Antigravity | Platform::LmStudio)
    }

    /// Where this harness keeps its MCP configuration, and in what format.
    ///
    /// `home` is the user's home directory, already resolved by the caller —
    /// resolving it here would reintroduce the drift this module exists to
    /// remove. Returns `None` for harnesses with no MCP config target.
    pub fn mcp_config(self, home: &Path) -> Option<(PathBuf, McpConfigFormat)> {
        let path = match self {
            Platform::Antigravity => home.join(".gemini").join("config").join("mcp_config.json"),
            Platform::ClaudeCode => home.join(".claude.json"),
            Platform::ClaudeDesktop => home.join(claude_desktop_relative_path()),
            Platform::Codex => home.join(".codex").join("config.toml"),
            Platform::Cursor => home.join(".cursor").join("mcp.json"),
            Platform::Copilot => return None,
            Platform::LmStudio => home.join(".lmstudio").join("mcp.json"),
            Platform::VsCode => home.join(vscode_relative_path()),
        };
        let format = match self {
            Platform::Codex => McpConfigFormat::Toml,
            // VS Code names the map "servers"; every other harness "mcpServers".
            Platform::VsCode => McpConfigFormat::Json("servers"),
            _ => McpConfigFormat::Json("mcpServers"),
        };
        Some((path, format))
    }
}

/// Claude Desktop's config location relative to the home directory.
fn claude_desktop_relative_path() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "Library/Application Support/Claude/claude_desktop_config.json"
    }
    #[cfg(target_os = "windows")]
    {
        "AppData/Roaming/Claude/claude_desktop_config.json"
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        ".config/Claude/claude_desktop_config.json"
    }
}

/// VS Code's user MCP config location relative to the home directory.
fn vscode_relative_path() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "Library/Application Support/Code/User/mcp.json"
    }
    #[cfg(target_os = "windows")]
    {
        "AppData/Roaming/Code/User/mcp.json"
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        ".config/Code/User/mcp.json"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the two wizards depend on: whatever setup can write,
    /// uninstall can find. One table means they cannot disagree.
    #[test]
    fn every_mcp_platform_has_exactly_one_config_target() {
        let home = Path::new("/home/tester");
        for p in PLATFORMS.iter().copied() {
            let cfg = p.mcp_config(home);
            assert_eq!(
                cfg.is_some(),
                p.supports_mcp(),
                "{}: supports_mcp() and mcp_config() must agree",
                p.label()
            );
            if let Some((path, _)) = cfg {
                assert!(
                    path.starts_with(home),
                    "{}: config path must sit under the caller's home, got {}",
                    p.label(),
                    path.display()
                );
            }
        }
    }

    #[test]
    fn config_paths_are_distinct_per_platform() {
        let home = Path::new("/home/tester");
        let mut seen: Vec<PathBuf> = Vec::new();
        for p in PLATFORMS.iter().copied() {
            if let Some((path, _)) = p.mcp_config(home) {
                assert!(
                    !seen.contains(&path),
                    "{} shares a config path with another harness: {}",
                    p.label(),
                    path.display()
                );
                seen.push(path);
            }
        }
    }

    #[test]
    fn codex_is_the_only_toml_harness() {
        let home = Path::new("/home/tester");
        for p in PLATFORMS.iter().copied() {
            let Some((_, format)) = p.mcp_config(home) else {
                continue;
            };
            assert_eq!(
                format == McpConfigFormat::Toml,
                p == Platform::Codex,
                "{}: only Codex stores MCP config as TOML",
                p.label()
            );
        }
    }

    #[test]
    fn vscode_is_the_only_servers_key_harness() {
        let home = Path::new("/home/tester");
        for p in PLATFORMS.iter().copied() {
            if let Some((_, McpConfigFormat::Json(key))) = p.mcp_config(home) {
                let expected = if p == Platform::VsCode {
                    "servers"
                } else {
                    "mcpServers"
                };
                assert_eq!(key, expected, "{}: unexpected servers key", p.label());
            }
        }
    }

    #[test]
    fn cli_names_and_labels_are_unique() {
        for (i, a) in PLATFORMS.iter().enumerate() {
            for b in &PLATFORMS[i + 1..] {
                assert_ne!(a.cli_name(), b.cli_name());
                assert_ne!(a.label(), b.label());
            }
        }
    }

    #[test]
    fn mcp_display_name_matches_label_except_codex() {
        for p in PLATFORMS.iter().copied() {
            if p == Platform::Codex {
                assert_eq!(p.mcp_display_name(), "Codex CLI");
            } else {
                assert_eq!(p.mcp_display_name(), p.label());
            }
        }
    }

    #[test]
    fn scoped_entry_harnesses_are_the_ones_without_roots_list() {
        assert!(!Platform::Antigravity.sends_roots_list());
        assert!(!Platform::LmStudio.sends_roots_list());
        for p in PLATFORMS
            .iter()
            .copied()
            .filter(|p| !matches!(p, Platform::Antigravity | Platform::LmStudio))
        {
            assert!(p.sends_roots_list(), "{} should send roots/list", p.label());
        }
    }

    #[test]
    fn hook_platforms_exist_exactly_for_hook_capable_harnesses() {
        for p in PLATFORMS.iter().copied() {
            assert_eq!(
                p.hook_platform().is_some(),
                p.supports_hooks(),
                "{}: supports_hooks() and hook_platform() must agree",
                p.label()
            );
        }
    }
}
