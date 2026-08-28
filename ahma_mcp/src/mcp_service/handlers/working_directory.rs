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
use crate::sandbox::{ContainerNarrowing, Sandbox, ScopeSource};
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
    /// `Some` when this call was the one that narrowed a container-root scope
    /// (SPEC R5.2.6). Also a scope decision, so it is disclosed the same way.
    narrowing: Option<ContainerNarrowing>,
}

impl WorkingDirectory {
    /// The caller named the directory: nothing to disclose.
    fn supplied(path: String) -> Self {
        Self {
            path,
            substitution_notice: None,
            narrowing: None,
        }
    }

    /// ahma chose the directory. `notice` says which one and why, so a model
    /// reading only the tool result can tell the choice was not its own.
    fn substituted(path: String, notice: String) -> Self {
        Self {
            path,
            substitution_notice: Some(notice),
            narrowing: None,
        }
    }

    /// Record that resolving this directory also narrowed the session's scope.
    fn with_narrowing(mut self, narrowing: Option<ContainerNarrowing>) -> Self {
        self.narrowing = narrowing;
        self
    }

    /// Every scope decision this resolution made, in the order the reader needs
    /// them: where the command ran, then what that did to the sandbox.
    fn notices(&self) -> Vec<String> {
        self.substitution_notice
            .iter()
            .cloned()
            .chain(self.narrowing.iter().map(ContainerNarrowing::notice))
            .collect()
    }

    /// Attach the scope disclosures to a successful result.
    pub fn disclose(&self, result: CallToolResult) -> CallToolResult {
        self.notices()
            .iter()
            .fold(result, |acc, notice| common::append_note(acc, notice))
    }

    /// Attach the substitution disclosure to a *failure*. This is the case the
    /// incident actually turned on: `fatal: not a git repository` is a plausible
    /// project error and a wildly implausible one once you know the command ran
    /// somewhere the caller never named. The error is the only thing the model
    /// sees, so the directory has to be in it.
    pub fn disclose_error(&self, error: McpError) -> McpError {
        let notices = self.notices();
        if notices.is_empty() {
            return error;
        }
        McpError {
            code: error.code,
            message: format!("{}{}", error.message, notices.concat()).into(),
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

    /// The narrowing this resolution performed, if any.
    #[cfg(test)]
    pub(crate) fn narrowing(&self) -> Option<&ContainerNarrowing> {
        self.narrowing.as_ref()
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
        // R5.2.6: the working directory is the signal that selects which subtree
        // of a container root becomes writable. Narrowing before the command runs
        // is what makes the choice binding on this very call, not the next one.
        let narrowing = sandbox.narrow_container_to(std::path::Path::new(&supplied));
        return Ok(WorkingDirectory::supplied(supplied).with_narrowing(narrowing));
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

    if matches!(source, ScopeSource::Container | ScopeSource::Unestablished) {
        // Container: the scope spans every project the user owns — a directory
        // nobody chose *for this task* (R5.2.8). Unestablished: the scope has no
        // provenance at all, so substituting it would be running in a directory
        // whose origin ahma cannot even attribute. Both refuse.
        //
        // `PendingTui` is deliberately absent: that scope *was* chosen, by a
        // human at the TUI (R5.3.6), so it is as substitutable as `Explicit`.
        // Both meanings used to share one `Pending` variant, which would have
        // made R5.3.6 inherit this refusal the moment it was wired up.
        tracing::warn!(
            tool = %tool,
            scope = %scope,
            source = source.as_str(),
            "tool called without working_directory while the scope is not a chosen project \
             (source: container root or pending); refusing rather than substituting"
        );
        return Err(no_working_directory_error(sandbox, tool, &scope, source));
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
fn no_working_directory_error(
    sandbox: &Sandbox,
    tool: &str,
    scope: &str,
    source: ScopeSource,
) -> McpError {
    let scope_text = sandbox.scope_text(source);
    let why = match source {
        ScopeSource::Container => {
            "your container root — the directory that holds *all* your projects, not the one \
             this task is about"
        }
        _ => {
            "a directory with no established provenance — nothing explicit, no client-reported \
             workspace root"
        }
    };
    let message = format!(
        "`{tool}` was called without `working_directory`, and this session's sandbox scope is \
         {why} (source: {source}). Refusing to run in `{scope}`, because \
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
        "reason": match source {
            ScopeSource::Container => "scope_source_is_container",
            _ => "scope_source_is_pending",
        },
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
    /// [`ScopeSource::Pending`] (a fresh `Sandbox` has received nothing from any
    /// client and carries no container root).
    fn default_scope_sandbox(scope: &std::path::Path) -> Sandbox {
        Sandbox::new(
            vec![scope.to_path_buf()],
            SandboxMode::Strict,
            false,
            false,
            false,
        )
        .expect("tempdir is a valid scope")
    }

    /// A sandbox whose scope came from `roots/list`, i.e. a real client-chosen
    /// directory that may legitimately be substituted.
    fn roots_sandbox(scope: &std::path::Path) -> Sandbox {
        let sb = default_scope_sandbox(scope);
        sb.set_roots_received(true);
        sb
    }

    /// A sandbox scoped to a container root holding two sibling projects, armed
    /// for auto-narrowing (SPEC R5.2.6).
    fn container_sandbox(container: &std::path::Path) -> Sandbox {
        std::fs::create_dir_all(container.join("proj-a")).unwrap();
        std::fs::create_dir_all(container.join("proj-b")).unwrap();
        let canonical = dunce::canonicalize(container).unwrap();
        default_scope_sandbox(container).with_container_root(Some(canonical))
    }

    /// The narrowing signal comes from the supplied `working_directory`, and the
    /// call that provides it is the one that gets told (R5.4).
    #[test]
    fn supplied_directory_narrows_the_container_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let sb = container_sandbox(dir.path());
        let project = dunce::canonicalize(dir.path()).unwrap().join("proj-a");

        let wd = resolve(
            &sb,
            "cargo",
            &args_with_dir(&project.join("src").to_string_lossy()),
        )
        .unwrap();

        let narrowing = wd.narrowing().expect("first path-bearing call narrows");
        assert_eq!(narrowing.child, project, "narrows to the immediate child");
        assert_eq!(sb.narrowed_to(), Some(project.clone()));
        let writable = sb.scopes().to_vec();
        assert!(
            writable.contains(&project),
            "the chosen project must be writable: {writable:?}"
        );
        // Every spelling the sandbox keeps (macOS keeps the `/var` alias beside
        // the canonical `/private/var`) must point at the child, not the container.
        assert!(
            writable
                .iter()
                .all(|s| s.file_name() == project.file_name()),
            "the container itself must no longer be writable: {writable:?}"
        );
        assert!(
            sb.read_scopes()
                .contains(&dunce::canonicalize(dir.path()).unwrap()),
            "the rest of the container stays readable: {:?}",
            sb.read_scopes()
        );

        let text = wd.disclose(common::text_result("ok"));
        let ContentBlock::Text(block) = text.content.last().unwrap() else {
            panic!("expected text content");
        };
        assert!(block.text.contains("narrowed"), "{}", block.text);
        assert!(block.text.contains("proj-a"), "{}", block.text);
    }

    /// Narrowing happens once. A later call naming a sibling project must not
    /// re-point the writable scope — R5.1.1 allows exactly one commit, and a
    /// second narrowing driven by tool input would be a scope change mid-session.
    #[test]
    fn second_call_naming_a_sibling_does_not_re_narrow() {
        let dir = tempfile::tempdir().unwrap();
        let sb = container_sandbox(dir.path());
        let root = dunce::canonicalize(dir.path()).unwrap();

        let first = resolve(
            &sb,
            "cargo",
            &args_with_dir(&root.join("proj-a").to_string_lossy()),
        )
        .unwrap();
        assert!(first.narrowing().is_some());

        let second = resolve(
            &sb,
            "cargo",
            &args_with_dir(&root.join("proj-b").to_string_lossy()),
        )
        .unwrap();
        assert!(
            second.narrowing().is_none(),
            "a session narrows at most once"
        );
        let writable = sb.scopes().to_vec();
        assert!(writable.contains(&root.join("proj-a")));
        assert!(
            !writable.iter().any(|s| s.ends_with("proj-b")),
            "the sibling must never become writable: {writable:?}"
        );
    }

    /// The container itself is not a project. Naming it selects nothing, so
    /// nothing narrows — and R5.2.8's refusal covers the omitted-directory case.
    #[test]
    fn naming_the_container_itself_narrows_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let sb = container_sandbox(dir.path());
        let root = dunce::canonicalize(dir.path()).unwrap();

        let wd = resolve(&sb, "cargo", &args_with_dir(&root.to_string_lossy())).unwrap();
        assert!(wd.narrowing().is_none());
        assert_eq!(sb.narrowed_to(), None);
    }

    /// A path outside the container is not this code's to reject: it narrows
    /// nothing and leaves the scope alone, so `validate_path` still produces the
    /// out-of-scope error with the message it deserves.
    #[test]
    fn a_path_outside_the_container_narrows_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let sb = container_sandbox(dir.path());
        let before = sb.scopes().to_vec();

        let wd = resolve(
            &sb,
            "cargo",
            &args_with_dir(&elsewhere.path().to_string_lossy()),
        )
        .unwrap();

        assert!(wd.narrowing().is_none());
        assert_eq!(sb.scopes().to_vec(), before, "scope must be untouched");
    }

    /// Narrowing is armed only for a container-root scope. A client-reported or
    /// operator-chosen scope is already the project, and shrinking it would take
    /// away something the user actually asked for.
    #[test]
    fn a_roots_scope_is_never_narrowed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        let sb = roots_sandbox(dir.path());
        let before = sb.scopes().to_vec();

        let wd = resolve(
            &sb,
            "cargo",
            &args_with_dir(&dir.path().join("sub").to_string_lossy()),
        )
        .unwrap();

        assert!(wd.narrowing().is_none());
        assert_eq!(sb.scopes().to_vec(), before);
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
        let sb = container_sandbox(dir.path());
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

    /// A scope with no provenance at all (nothing explicit, no roots, no
    /// container) is equally not a directory anyone chose for this task, so an
    /// omitted `working_directory` refuses there too.
    #[test]
    fn pending_scope_refuses_too() {
        let dir = tempfile::tempdir().unwrap();
        let sb = default_scope_sandbox(dir.path());
        let err = resolve(&sb, "cargo", &Map::new()).unwrap_err();

        assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        let data = err.data.expect("refusal carries an actionable payload");
        assert_eq!(data["kind"], "working_directory_required");
        assert_eq!(data["reason"], "scope_source_is_pending");
        assert_eq!(data["scope_source"], "pending");
    }

    /// Once the client has reported usable roots, the container is no longer the
    /// scope source and narrowing must not fire — rewriting a roots-derived
    /// scope entry with a joined child name would corrupt it.
    #[test]
    fn roots_scope_with_stale_container_root_is_never_narrowed() {
        let container = tempfile::tempdir().unwrap();
        let sb = container_sandbox(container.path());
        let root = dunce::canonicalize(container.path()).unwrap();
        // The client later reports a usable root: provenance moves to roots/list.
        sb.set_roots_received(true);
        let before = sb.scopes().to_vec();

        let wd = resolve(
            &sb,
            "cargo",
            &args_with_dir(&root.join("proj-a").join("src").to_string_lossy()),
        )
        .unwrap();

        assert!(
            wd.narrowing().is_none(),
            "roots provenance disarms narrowing"
        );
        assert_eq!(sb.scopes().to_vec(), before, "scope must be untouched");
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
