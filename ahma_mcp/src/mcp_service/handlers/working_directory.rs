//! Where a tool call runs, and whether ahma — not the caller — decided that
//! (SPEC R5.2.8).
//!
//! Substituting a working directory is a **scope** decision, so R5.4 applies to
//! it: no scope decision may be communicated only via an internal log line. The
//! substitution therefore travels back with the result, through the same note
//! channel the ignored-argument disclosure uses (R2.6.4).
//!
//! R5.2.8 binds *every* surface that runs a command, which is why this lives
//! here rather than inside one handler. ahma has two: `run_terminal_command`
//! (`super::shell_tool`) and the MTDF subcommand dispatcher
//! (`crate::mcp_service::AhmaMcpService::dispatch_subcommand_tool`). The rule
//! was first written into the shell handler alone, and the MTDF path went on
//! silently substituting — exactly the regression the "a rule that binds one
//! surface binds all of them" convention exists to prevent.

use super::common;
use crate::sandbox::{Sandbox, ScopeSource};
use rmcp::model::{CallToolResult, ErrorData as McpError};
use serde_json::{Map, Value};

/// The directory a tool call will actually run in, plus — when the caller did
/// not name one — the disclosure that ahma picked it.
#[derive(Debug)]
pub struct WorkingDirectory {
    /// The directory the command runs in.
    pub path: String,
    /// `Some(notice)` when ahma chose [`path`](Self::path) because the call
    /// omitted `working_directory`; `None` when the caller supplied it.
    substitution_notice: Option<String>,
}

impl WorkingDirectory {
    /// The caller named the directory: nothing to disclose.
    fn supplied(path: String) -> Self {
        Self {
            path,
            substitution_notice: None,
        }
    }

    /// ahma chose the directory. `notice` says which one and why, so a model
    /// reading only the tool result can tell the choice was not its own.
    fn substituted(path: String, notice: String) -> Self {
        Self {
            path,
            substitution_notice: Some(notice),
        }
    }

    /// Attach the substitution disclosure to a successful result.
    pub fn disclose(&self, result: CallToolResult) -> CallToolResult {
        match &self.substitution_notice {
            Some(notice) => common::append_note(result, notice),
            None => result,
        }
    }

    /// Attach the substitution disclosure to a *failure*. This is the case the
    /// incident actually turned on: `fatal: not a git repository` is a plausible
    /// project error and a wildly implausible one once you know the command ran
    /// somewhere the caller never named. The error is the only thing the model
    /// sees, so the directory has to be in it.
    pub fn disclose_error(&self, error: McpError) -> McpError {
        let Some(notice) = &self.substitution_notice else {
            return error;
        };
        McpError {
            code: error.code,
            message: format!("{}{}", error.message, notice).into(),
            data: error.data,
        }
    }

    /// Whether ahma picked this directory rather than the caller.
    #[cfg(test)]
    pub(crate) fn was_substituted(&self) -> bool {
        self.substitution_notice.is_some()
    }

    /// The disclosure ahma will attach, if it substituted anything.
    #[cfg(test)]
    pub(crate) fn substitution_notice(&self) -> Option<&str> {
        self.substitution_notice.as_deref()
    }
}

/// Decide where `tool` runs, and whether that decision was ahma's.
///
/// Three outcomes, in order:
/// 1. caller named a directory → use it, disclose nothing;
/// 2. omitted, and the scope came from the container root → refuse (see
///    [`no_working_directory_error`]);
/// 3. omitted otherwise → substitute, and say so in the result.
pub fn resolve(
    sandbox: &Sandbox,
    tool: &str,
    args: &Map<String, Value>,
) -> Result<WorkingDirectory, McpError> {
    if let Some(supplied) = common::opt_str(args, "working_directory") {
        return Ok(WorkingDirectory::supplied(supplied));
    }

    let source = sandbox.scope_source();
    let Some(scope) = substitutable_scope(sandbox) else {
        // No scope to borrow (test mode). `.` is the process CWD — still not
        // the caller's choice, so it is still disclosed.
        return Ok(WorkingDirectory::substituted(
            ".".to_string(),
            substitution_notice(
                ".",
                "the server's current directory (no sandbox scope was available)",
            ),
        ));
    };

    if source == ScopeSource::Container {
        tracing::warn!(
            tool = %tool,
            scope = %scope,
            "tool called without working_directory while the scope is the user's container \
             root; refusing rather than running in a directory that spans every project"
        );
        return Err(no_working_directory_error(sandbox, tool, &scope));
    }

    let why = format!(
        "this session's locked sandbox scope (source: {})",
        source.as_str()
    );
    Ok(WorkingDirectory::substituted(
        scope.clone(),
        substitution_notice(&scope, &why),
    ))
}

/// The first writable scope, which is what ahma substitutes when the caller
/// names no directory. `None` in test mode, where scope is resolved but never
/// enforced and the process CWD is the honest answer.
fn substitutable_scope(sandbox: &Sandbox) -> Option<String> {
    if sandbox.is_test_mode() {
        return None;
    }
    sandbox
        .scopes()
        .first()
        .map(|p: &std::path::PathBuf| p.to_string_lossy().to_string())
}

/// The disclosure appended when ahma chose the working directory.
fn substitution_notice(directory: &str, why: &str) -> String {
    format!(
        "\n\nNote: no `working_directory` was given, so ahma ran this command in `{directory}` \
         — {why}. If that is not where the command was meant to run, re-issue it with an \
         explicit in-scope `working_directory`; do not read the output as if it came from \
         somewhere else."
    )
}

/// Refusal for "no `working_directory`, and the only thing to substitute is the
/// **container root**" (SPEC R5.2.3 / R5.2.8).
///
/// Two reasons this one case refuses instead of running.
///
/// It is a directory nobody chose *for this task*: a container spans every
/// project the user owns. When the scope was `~/sandbox`, commands ran there and
/// answered `fatal: not a git repository` and `bash: ./gradlew: No such file or
/// directory` — ordinary *shell* errors, so the model debugged the project
/// rather than the directory, could not diagnose it, and left ahma for its own
/// unsandboxed terminal. A silent wrong-directory success is worse than a loud
/// failure.
///
/// It is also the missing input to auto-narrowing (R5.2.6): the working
/// directory is the signal that selects which subtree of the container becomes
/// writable, so a call that omits it leaves the server nothing to narrow on.
///
/// The body carries the complete scope through the one canonical renderer
/// (R5.4(d): scope-related errors show the scope and its provenance), plus a
/// machine-readable `data` payload shaped like the `sandbox_denial` one so a
/// client can act on it without parsing prose.
fn no_working_directory_error(sandbox: &Sandbox, tool: &str, scope: &str) -> McpError {
    let source = ScopeSource::Container;
    let scope_text = sandbox.scope_text(source);
    let message = format!(
        "`{tool}` was called without `working_directory`, and this session's sandbox scope is \
         your container root (source: {source}) — the directory that holds *all* your \
         projects, not the one this task is about. Refusing to run in `{scope}`, because \
         commands that land there fail with ordinary-looking shell errors (`not a git \
         repository`, `No such file or directory`) that give no hint the directory was \
         substituted.\n\
         \n{scope_text}\n\
         Fix by passing an explicit in-scope `working_directory` — the project subdirectory \
         this task is about. That also tells ahma which subtree to narrow the writable scope \
         to. Alternatively, give the session a scope of its own with `--sandbox-scope <dir>`, \
         or use a client that reports workspace roots via `roots/list`.",
        source = source.as_str(),
    );
    let data = serde_json::json!({
        "kind": "working_directory_required",
        "reason": "scope_source_is_container",
        "tool": tool,
        "scope_source": source.as_str(),
        "substituted_scope": scope,
        "remediation": "Pass an explicit in-scope `working_directory` naming the project \
                        subdirectory, or configure a session scope (`--sandbox-scope <dir>`).",
    });
    McpError::invalid_params(message, Some(data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxMode;
    use rmcp::model::ContentBlock;

    fn args_with_dir(dir: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("working_directory".into(), Value::String(dir.into()));
        m
    }

    /// An enforcing sandbox scoped to `scope` with no provenance recorded, i.e.
    /// [`ScopeSource::Container`].
    ///
    /// `set_roots_received(false)` is not redundant: a fresh `Sandbox` starts
    /// with the flag set, and a live session only clears it during
    /// `AhmaMcpService::new` so each session renegotiates roots.
    fn default_scope_sandbox(scope: &std::path::Path) -> Sandbox {
        let sb = Sandbox::new(
            vec![scope.to_path_buf()],
            SandboxMode::Strict,
            false,
            false,
            false,
        )
        .expect("tempdir is a valid scope");
        sb.set_roots_received(false);
        sb
    }

    /// A sandbox whose scope came from `roots/list`, i.e. a real client-chosen
    /// directory that may legitimately be substituted.
    fn roots_sandbox(scope: &std::path::Path) -> Sandbox {
        let sb = default_scope_sandbox(scope);
        sb.set_roots_received(true);
        sb
    }

    #[test]
    fn supplied_directory_is_used_verbatim_and_not_disclosed() {
        let dir = tempfile::tempdir().unwrap();
        let sb = roots_sandbox(dir.path());
        let wd = resolve(&sb, "run_terminal_command", &args_with_dir("/somewhere")).unwrap();
        assert_eq!(wd.path, "/somewhere");
        assert!(!wd.was_substituted());
    }

    #[test]
    fn substitution_from_roots_scope_is_disclosed_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let sb = roots_sandbox(dir.path());
        let wd = resolve(&sb, "cargo", &Map::new()).unwrap();
        assert!(wd.was_substituted());

        let disclosed = wd.disclose(common::text_result("ok"));
        let ContentBlock::Text(text) = disclosed.content.last().unwrap() else {
            panic!("expected text content");
        };
        assert!(text.text.contains("no `working_directory` was given"));
        assert!(text.text.contains("roots/list"));
    }

    #[test]
    fn substitution_is_disclosed_on_failure_too() {
        let dir = tempfile::tempdir().unwrap();
        let sb = roots_sandbox(dir.path());
        let wd = resolve(&sb, "cargo", &Map::new()).unwrap();

        let err = wd.disclose_error(common::mcp_internal("fatal: not a git repository"));
        assert!(err.message.contains("fatal: not a git repository"));
        assert!(err.message.contains("no `working_directory` was given"));
    }

    /// The MTDF surface must refuse on a container scope exactly as the shell
    /// surface does — R5.2.8 binds every surface, and this test is what stops
    /// the two from drifting apart again.
    #[test]
    fn container_scope_refuses_and_names_the_tool() {
        let dir = tempfile::tempdir().unwrap();
        let sb = default_scope_sandbox(dir.path());
        let err = resolve(&sb, "cargo", &Map::new()).unwrap_err();

        assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(
            err.message.contains("`cargo` was called without"),
            "refusal must name the tool: {}",
            err.message
        );
        let data = err.data.expect("refusal carries an actionable payload");
        assert_eq!(data["kind"], "working_directory_required");
        assert_eq!(data["reason"], "scope_source_is_container");
        assert_eq!(data["tool"], "cargo");
        assert_eq!(data["scope_source"], "container");
    }

    /// A container scope only refuses when the caller named nothing: an explicit
    /// directory is always honoured (and separately validated by path security).
    #[test]
    fn container_scope_with_explicit_directory_still_runs() {
        let dir = tempfile::tempdir().unwrap();
        let sb = default_scope_sandbox(dir.path());
        let wd = resolve(&sb, "cargo", &args_with_dir("/chosen")).unwrap();
        assert_eq!(wd.path, "/chosen");
    }

    /// Test mode has no enforced scope to borrow, so `.` is the honest answer —
    /// but it is still not the caller's choice, so it is still disclosed.
    #[test]
    fn test_mode_falls_back_to_cwd_and_still_discloses() {
        let dir = tempfile::tempdir().unwrap();
        let sb = Sandbox::new(
            vec![dir.path().to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap();
        let wd = resolve(&sb, "cargo", &Map::new()).unwrap();
        assert_eq!(wd.path, ".");
        assert!(wd.was_substituted());
    }
}
