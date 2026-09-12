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
            pub fn name(self) -> &'static str {
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
    pub fn is_sandbox_exempt(self) -> bool {
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
            | BuiltinTool::Agent
            | BuiltinTool::LogMonitor => false,
        }
    }

    /// Whether ahma's own agent loop is denied this tool.
    ///
    /// `agent` is withheld from the agent loop so a run cannot recurse into
    /// itself; every other built-in is fair game.
    pub fn is_denied_in_agent_loop(self) -> bool {
        matches!(self, BuiltinTool::Agent)
    }

    /// Whether this is a harness file tool — one duplicating a capability
    /// IDE-shaped clients already ship natively.
    ///
    /// Advertising these unconditionally cost a real incident: a Claude Code
    /// plan-mode subagent that lacked native `Write` used ahma's instead. They
    /// are withheld from clients with native equivalents.
    pub fn is_harness_file_tool(self) -> bool {
        match self {
            BuiltinTool::ReadFile
            | BuiltinTool::WriteFile
            | BuiltinTool::ReplaceInFile
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
    pub fn is_sync_meta_tool_for_protocol_cancel(self) -> bool {
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
            | BuiltinTool::Agent
            | BuiltinTool::TodoWrite
            | BuiltinTool::LogMonitor => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
