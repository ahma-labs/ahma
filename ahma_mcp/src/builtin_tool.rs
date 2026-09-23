//! The tools ahma answers itself, named once.
//!
//! These twenty names used to be written out in four places — the `Tool::new`
//! arguments, `HARDCODED_TOOLS`, `RESERVED_TOOL_NAMES`, and the `match` in
//! `dispatch_tool_call` — plus three subset lists carved out of them. PR #617
//! collapsed the first three into one constant and added a test to keep it
//! honest. This finishes the job: a name is now a *variant*, so the remaining
//! agreements are checked by the compiler rather than by a test.
//!
//! The distinction matters most in dispatch. That `match` ends in
//! `_ => dispatch_configured_tool(...)`, so a built-in added to the list and
//! forgotten there did not fail loudly — it fell through to the configured-tool
//! lookup and returned "tool not found". Matching on this enum makes that a
//! compile error.
//!
//! The subset predicates below are the other half. Each is an exhaustive match,
//! so adding a tool forces an explicit answer to "is it exempt from the sandbox
//! gate?", "may ahma's own agent loop call it?", "is it a harness file tool?" —
//! rather than silently defaulting to "no" the way a `&[&str]` list did.

/// Declare the tool set once, and derive the variant list, `ALL`, and the
/// wire names from that single declaration.
///
/// Written as a macro for one reason: `ALL` must list every variant, and a
/// hand-written `ALL` beside a hand-written `name()` is the same two-lists
/// problem one level down. Generated from one list, they cannot disagree.
macro_rules! builtin_tools {
    ($($variant:ident => $name:literal),+ $(,)?) => {
        /// A tool implemented by ahma itself, rather than by a tool config.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum BuiltinTool {
            $(
                #[doc = concat!("The `", $name, "` tool.")]
                $variant,
            )+
        }

        impl BuiltinTool {
            /// Every built-in, in the order `tools/list` advertises them.
            pub const ALL: &'static [BuiltinTool] = &[$(BuiltinTool::$variant),+];

            /// The name this tool is called by on the wire.
            pub const fn name(self) -> &'static str {
                match self {
                    $(BuiltinTool::$variant => $name),+
                }
            }
        }
    };
}

builtin_tools! {
    Await => "await",
    Status => "status",
    RunTerminalCommand => "run_terminal_command",
    LogsList => "logs_list",
    LogsApprove => "logs_approve",
    LogsRead => "logs_read",
    LogsSearch => "logs_search",
    Restart => "restart",
    Cancel => "cancel",
    SandboxGrant => "sandbox_grant",
    ReadFile => "read_file",
    ListDir => "list_dir",
    FileSearch => "file_search",
    GrepSearch => "grep_search",
    FetchWebpage => "fetch_webpage",
    WriteFile => "write_file",
    ReplaceInFile => "replace_in_file",
    MultiEdit => "multi_edit",
    ApplyPatch => "apply_patch",
    Agent => "agent",
    TodoWrite => "todo_write",
    LogMonitor => "log_monitor",
}

impl BuiltinTool {
    /// The built-in called `name`, if there is one.
    ///
    /// A linear scan over twenty `&'static str` comparisons, run once per tool
    /// call. Deriving it from [`Self::name`] rather than writing a second
    /// `match` is the point: one declaration, no way to disagree with itself.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|tool| tool.name() == name)
    }

    /// Every built-in name, for the callers that want strings.
    pub fn names() -> impl Iterator<Item = &'static str> {
        Self::ALL.iter().copied().map(Self::name)
    }

    /// Whether this tool may run before the sandbox scope has settled
    /// (SPEC R5.1.2).
    ///
    /// The gate exists because a tool that touches the filesystem must not act
    /// on an undecided scope. These six do not: they inspect or steer ahma
    /// itself, and `sandbox_grant` is how a stuck scope gets unstuck — gating
    /// it on a settled scope would deadlock the very case it exists for.
    pub const fn is_sandbox_exempt(self) -> bool {
        match self {
            BuiltinTool::Status
            | BuiltinTool::Await
            | BuiltinTool::Cancel
            | BuiltinTool::SandboxGrant
            | BuiltinTool::Restart
            | BuiltinTool::TodoWrite => true,
            BuiltinTool::RunTerminalCommand
            | BuiltinTool::LogsList
            | BuiltinTool::LogsApprove
            | BuiltinTool::LogsRead
            | BuiltinTool::LogsSearch
            | BuiltinTool::ReadFile
            | BuiltinTool::ListDir
            | BuiltinTool::FileSearch
            | BuiltinTool::GrepSearch
            | BuiltinTool::FetchWebpage
            | BuiltinTool::WriteFile
            | BuiltinTool::ReplaceInFile
            | BuiltinTool::MultiEdit
            | BuiltinTool::ApplyPatch
            | BuiltinTool::Agent
            | BuiltinTool::LogMonitor => false,
        }
    }

    /// Whether ahma's own agent loop is denied this tool.
    ///
    /// `agent` is withheld from the agent loop so a run cannot recurse into
    /// itself; every other built-in is fair game.
    pub const fn is_denied_in_agent_loop(self) -> bool {
        matches!(self, BuiltinTool::Agent)
    }

    /// Whether this is a harness file tool — one duplicating a capability
    /// IDE-shaped clients already ship natively.
    ///
    /// Advertising these unconditionally cost a real incident: a Claude Code
    /// plan-mode subagent that lacked native `Write` used ahma's instead. They
    /// are withheld from clients with native equivalents.
    pub const fn is_harness_file_tool(self) -> bool {
        match self {
            BuiltinTool::ReadFile
            | BuiltinTool::WriteFile
            | BuiltinTool::ReplaceInFile
            | BuiltinTool::MultiEdit
            | BuiltinTool::ApplyPatch
            | BuiltinTool::ListDir
            | BuiltinTool::FileSearch
            | BuiltinTool::GrepSearch
            | BuiltinTool::TodoWrite => true,
            BuiltinTool::Await
            | BuiltinTool::Status
            | BuiltinTool::RunTerminalCommand
            | BuiltinTool::LogsList
            | BuiltinTool::LogsApprove
            | BuiltinTool::LogsRead
            | BuiltinTool::LogsSearch
            | BuiltinTool::Restart
            | BuiltinTool::Cancel
            | BuiltinTool::SandboxGrant
            | BuiltinTool::FetchWebpage
            | BuiltinTool::Agent
            | BuiltinTool::LogMonitor => false,
        }
    }

    /// Whether this tool is a synchronous/meta tool for the purposes of
    /// protocol-level cancellation bookkeeping (answers "is this a small,
    /// synchronous control-plane call rather than a long-running command?").
    pub const fn is_sync_meta_tool_for_protocol_cancel(self) -> bool {
        match self {
            BuiltinTool::Await
            | BuiltinTool::Status
            | BuiltinTool::Cancel
            | BuiltinTool::LogsList
            | BuiltinTool::LogsApprove
            | BuiltinTool::LogsRead
            | BuiltinTool::LogsSearch
            | BuiltinTool::Restart => true,
            BuiltinTool::RunTerminalCommand
            | BuiltinTool::SandboxGrant
            | BuiltinTool::ReadFile
            | BuiltinTool::ListDir
            | BuiltinTool::FileSearch
            | BuiltinTool::GrepSearch
            | BuiltinTool::FetchWebpage
            | BuiltinTool::WriteFile
            | BuiltinTool::ReplaceInFile
            | BuiltinTool::MultiEdit
            | BuiltinTool::ApplyPatch
            | BuiltinTool::Agent
            | BuiltinTool::TodoWrite
            | BuiltinTool::LogMonitor => false,
        }
    }

    /// Whether this built-in changes the filesystem, a persisted setting, or
    /// external state — the same question [`crate::config::ToolConfig::mutates`]
    /// answers for MTDF-defined tools, asked here for the tools ahma answers
    /// itself so `needs_approval` has one classification covering both.
    ///
    /// `write_file` and `replace_in_file` write the workspace; `logs_approve`
    /// persists a log-symlink exception outside it
    /// (`sandbox::add_log_exception`); `run_terminal_command` runs an
    /// arbitrary command, the least contained of all of them. `sandbox_grant`
    /// is exempt despite writing the permission ledger: its own handler
    /// already refuses to self-persist for the autonomous agent-loop client,
    /// routing `confirm: true` to the human approval surface instead
    /// (`handle_sandbox_grant`) — gating it again here would be redundant,
    /// not safer. Everything else only reads or only steers ahma's own
    /// control plane. Each is exempted explicitly, never by omission — the
    /// same fail-closed shape as the MTDF default.
    pub const fn is_mutating(self) -> bool {
        match self {
            BuiltinTool::WriteFile
            | BuiltinTool::ReplaceInFile
            | BuiltinTool::MultiEdit
            | BuiltinTool::ApplyPatch
            | BuiltinTool::RunTerminalCommand
            | BuiltinTool::LogsApprove => true,
            BuiltinTool::Await
            | BuiltinTool::Status
            | BuiltinTool::LogsList
            | BuiltinTool::LogsRead
            | BuiltinTool::LogsSearch
            | BuiltinTool::Restart
            | BuiltinTool::Cancel
            | BuiltinTool::SandboxGrant
            | BuiltinTool::ReadFile
            | BuiltinTool::ListDir
            | BuiltinTool::FileSearch
            | BuiltinTool::GrepSearch
            | BuiltinTool::FetchWebpage
            | BuiltinTool::Agent
            | BuiltinTool::TodoWrite
            | BuiltinTool::LogMonitor => false,
        }
    }
}

impl BuiltinTool {
    /// Whether this built-in's effect reaches **outside** the workspace sandbox,
    /// so a trusted folder (SPEC R-PERM.1.3) must not auto-approve it.
    ///
    /// `logs_approve` persists a log-symlink exception outside the workspace.
    /// `sandbox_grant` widens the scope itself and already routes to a human.
    /// `fetch_webpage` reaches the network, which has its own egress gate
    /// (R-WEB.6) and is never the folder's to trust. Everything else runs inside
    /// the kernel sandbox, or only steers ahma's own control plane.
    pub const fn crosses_sandbox_boundary(self) -> bool {
        matches!(
            self,
            BuiltinTool::LogsApprove | BuiltinTool::SandboxGrant | BuiltinTool::FetchWebpage
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_boundary_tools_are_the_ones_that_leave_the_workspace() {
        for tool in [
            BuiltinTool::LogsApprove,
            BuiltinTool::SandboxGrant,
            BuiltinTool::FetchWebpage,
        ] {
            assert!(tool.crosses_sandbox_boundary(), "{}", tool.name());
        }
        for tool in [
            BuiltinTool::RunTerminalCommand,
            BuiltinTool::WriteFile,
            BuiltinTool::ReadFile,
        ] {
            assert!(!tool.crosses_sandbox_boundary(), "{}", tool.name());
        }
    }

    #[test]
    fn every_variant_round_trips_through_its_name() {
        for tool in BuiltinTool::ALL {
            assert_eq!(
                BuiltinTool::from_name(tool.name()),
                Some(*tool),
                "`{}` must resolve back to the variant that names it",
                tool.name()
            );
        }
    }

    #[test]
    fn names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for tool in BuiltinTool::ALL {
            assert!(
                seen.insert(tool.name()),
                "`{}` is declared twice",
                tool.name()
            );
        }
    }

    #[test]
    fn an_unknown_name_is_not_a_builtin() {
        assert_eq!(BuiltinTool::from_name("cargo"), None);
        assert_eq!(BuiltinTool::from_name(""), None);
        // Near-misses must not resolve: dispatch depends on exact names.
        assert_eq!(BuiltinTool::from_name("Await"), None);
        assert_eq!(BuiltinTool::from_name("await "), None);
    }

    /// The two file-mutating builtins and `run_terminal_command` (arbitrary
    /// execution) must require approval; `logs_approve` persists a log
    /// exception and must too. This is a regression guard: it's the
    /// classification `needs_approval` reads to decide whether a builtin
    /// prompts when the interactive tool-approval setting is off.
    #[test]
    fn is_mutating_covers_the_known_writers() {
        for tool in [
            BuiltinTool::WriteFile,
            BuiltinTool::ReplaceInFile,
            BuiltinTool::RunTerminalCommand,
            BuiltinTool::LogsApprove,
        ] {
            assert!(tool.is_mutating(), "{} must be mutating", tool.name());
        }
    }

    /// `sandbox_grant` writes the permission ledger but is exempt here: its
    /// own handler refuses to self-persist for the agent-loop client and
    /// routes to the human approval surface instead, so gating it a second
    /// time through `needs_approval` would be redundant.
    #[test]
    fn is_mutating_exempts_sandbox_grant_and_read_only_tools() {
        for tool in [
            BuiltinTool::SandboxGrant,
            BuiltinTool::ReadFile,
            BuiltinTool::Status,
            BuiltinTool::Await,
        ] {
            assert!(!tool.is_mutating(), "{} must not be mutating", tool.name());
        }
    }
}
