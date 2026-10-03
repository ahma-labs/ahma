//! Ask the human through the harness's own dialog before a command that the
//! sandbox will refuse again (SPEC R-PERM.10).
//!
//! A hooked command refused a path records it ([`record_refusal`]). The next
//! Bash call in that workspace is answered `ask` with a rewritten command that
//! carries a one-use token ([`APPROVE_FLAG`]); Claude Code shows its native
//! yes/no dialog, and only a yes runs that command, whose `run-shell` spends
//! the token to apply a session grant before the sandbox is built
//! ([`apply_approval`]). The storage and grouping rules live in
//! [`ahma_common::harness_asks`].

use std::path::{Path, PathBuf};

use ahma_common::config::{PersistentScope, ScopeAccess};
use ahma_common::harness_asks::{self, Question, Refusal};

/// The `run-shell` flag that carries an approved question's token. Only the
/// hook writes it: a command the agent itself wrote with this flag is refused
/// ([`carries_approval`]), so a token can never approve its own question.
pub(super) const APPROVE_FLAG: &str = "--approve-grant";

/// Whether an agent-written command tries to pass an approval token itself:
/// a `run-shell` invocation with [`APPROVE_FLAG`] anywhere in it.
pub(super) fn carries_approval(command: &str) -> bool {
    command.contains("run-shell") && command.contains(APPROVE_FLAG)
}

/// The workspace questions are kept under: the hook's first scope, which is
/// the enclosing repository when there is one (SPEC R5.2.1).
pub(super) fn workspace_for(cwd: &Path) -> PathBuf {
    super::resolve_hook_sandbox_scopes(cwd)
        .into_iter()
        .next()
        .unwrap_or_else(|| dunce::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()))
}

/// The harness process: the nearest ancestor that is neither a shell nor ahma.
/// Its life bounds a session grant approved in its dialog. `None` when it
/// cannot be found, and then nothing is asked, because nothing could bound it.
pub(super) fn harness_pid() -> Option<u32> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    const PASS_THROUGH: &[&str] = &[
        "sh",
        "bash",
        "zsh",
        "dash",
        "fish",
        "ksh",
        "env",
        "ahma",
        "timeout",
        "cmd",
        "pwsh",
        "powershell",
    ];
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    let mut pid = sys.process(Pid::from_u32(std::process::id()))?.parent()?;
    for _ in 0..8 {
        let proc = sys.process(pid)?;
        let name = proc.name().to_string_lossy().to_ascii_lowercase();
        let name = name.trim_start_matches('-').trim_end_matches(".exe");
        if !PASS_THROUGH.contains(&name) {
            return (pid.as_u32() > 1).then_some(pid.as_u32());
        }
        pid = proc.parent()?;
    }
    None
}

/// Remember that a hooked command in `cwd`'s workspace was refused `path`.
/// Best-effort: a store that cannot be written only means no question later.
pub(super) fn record_refusal(cwd: &Path, path: &Path, access: ScopeAccess) {
    let Some(dir) = harness_asks::default_dir() else {
        return;
    };
    let refusal = Refusal {
        path: path.to_path_buf(),
        grant_dir: crate::sandbox::grant_channel::grant_dir_for(path),
        access,
        at: ahma_common::session_grants::now_secs(),
        harness_pid: harness_pid(),
    };
    if let Err(e) = harness_asks::record_refusal(&dir, &workspace_for(cwd), refusal) {
        tracing::debug!("harness question not recorded: {e:#}");
    }
}

/// Whether a grant in `granted` already lets a command in `workspace` reach
/// `path` with `access`.
fn covered(
    granted: &[PersistentScope],
    workspace: &Path,
    path: &Path,
    access: ScopeAccess,
) -> bool {
    granted.iter().any(|g| {
        let applies = g
            .workspace
            .as_deref()
            .is_none_or(|w| workspace.starts_with(w));
        let root = ahma_common::config::expand_home(&g.path);
        let root = dunce::canonicalize(&root).unwrap_or(root);
        applies && (g.access.is_write() || !access.is_write()) && path.starts_with(&root)
    })
}

/// The question to put to the human before the next command in `cwd`, in
/// harness session `session_id`, with the token that approves it. Recording
/// the question is what makes it the only time it is asked this session.
pub(super) fn next_ask(
    cwd: &Path,
    session_id: &str,
    command: &str,
    persistent: &[PersistentScope],
) -> Option<(Question, String)> {
    let dir = harness_asks::default_dir()?;
    let workspace = workspace_for(cwd);
    let now = ahma_common::session_grants::now_secs();
    let mut asks = harness_asks::load(&dir, &workspace, now);
    // A command that cannot write (the write queue's read-only lane, R2.7.4)
    // could not use a write grant: asking before a `git status` would be noise.
    let may_write = !matches!(
        crate::adapter::lane::classify_shell_command(command),
        crate::adapter::workspace_queue::Lane::ReadOnly
    );
    asks.refusals.retain(|r| may_write || !r.access.is_write());
    if asks.refusals.is_empty() {
        return None;
    }
    let scopes = super::resolve_hook_sandbox_scopes(cwd);
    let mut granted = persistent.to_vec();
    granted.extend(crate::sandbox::session_scopes_for(&scopes));
    let home = ahma_common::config::ahma_home_dir().map(|h| dunce::canonicalize(&h).unwrap_or(h));
    let is_covered = |p: &Path, a: ScopeAccess| covered(&granted, &workspace, p, a);
    let question =
        harness_asks::next_question(&asks, session_id, &is_covered, home.as_deref(), &scopes)?;
    // A grant must outlive this hook: without the harness's pid nothing can
    // bound it, so nothing is asked and the one-line note stays the answer.
    let pid = asks
        .refusals
        .iter()
        .find_map(|r| r.harness_pid)
        .or_else(harness_pid)?;
    let token = harness_asks::mark_asked(
        &dir,
        &workspace,
        session_id,
        &question,
        command,
        Some(pid),
        now,
    )
    .ok()?;
    Some((question, token))
}

/// The dialog text for `question`: what was refused, what a yes grants and
/// for how long, what a no means, and how to make it permanent.
pub(super) fn ask_reason(question: &Question) -> String {
    let (verb, ro) = if question.access.is_write() {
        ("write", "")
    } else {
        ("read", " --read-only")
    };
    let mut covers: Vec<String> = question
        .covers
        .iter()
        .take(4)
        .map(|p| p.display().to_string())
        .collect();
    if question.covers.len() > 4 {
        covers.push(format!("and {} more", question.covers.len() - 4));
    }
    let home = ahma_common::config::ahma_home_dir();
    let risk =
        match ahma_common::scope_grant::classify_grant_risk(&question.dir, home.as_deref(), &[]) {
            ahma_common::scope_grant::GrantRisk::High(why) => format!(" Note: {}.", why.join("; ")),
            _ => String::new(),
        };
    format!(
        "ahma: an earlier command here was refused {verb} access to {} ({}). Approve to let \
         commands in this workspace {verb} it for this session, then run this command. Deny and \
         ahma will not ask about it again this session.{risk} To allow it always: `ahma sandbox \
         grant {}{ro}`.",
        question.dir.display(),
        covers.join(", "),
        question.dir.display(),
    )
}

/// Spend an approval `token` for `command` in `cwd`: apply the session grant
/// the human approved in the harness dialog, and say so in one line. `None`
/// when the token is unknown, spent, expired, for another command, or the
/// grant is now refused; then the command runs without it.
pub(super) fn apply_approval(token: &str, cwd: &Path, command: &str) -> Option<String> {
    let dir = harness_asks::default_dir()?;
    let workspace = workspace_for(cwd);
    let now = ahma_common::session_grants::now_secs();
    let Some(asked) = harness_asks::take_approved(&dir, &workspace, token, command, now) else {
        return Some(
            "ahma: the approval this command carries is unknown, already used or expired; it \
             runs without a new grant."
                .to_string(),
        );
    };
    let q = &asked.question;
    if let Some(why) = ahma_common::scope_grant::refusal_reason(&q.dir) {
        return Some(format!(
            "ahma: {} cannot be granted ({why}); the command runs without it.",
            q.dir.display()
        ));
    }
    let owner = asked.harness_pid?;
    crate::sandbox::record_session_grant(
        &q.dir,
        q.access,
        Some(&workspace),
        owner,
        "harness dialog",
    );
    let verb = if q.access.is_write() { "write" } else { "read" };
    Some(format!(
        "ahma: approved — commands in this workspace may {verb} {} for this session.",
        q.dir.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole R-PERM.10 loop at the hook: a refusal is recorded; the next
    /// Claude Code Bash call is answered `ask` with a token-carrying command;
    /// the same session is not asked again; a yes (the command running with
    /// the token) applies a session grant once.
    #[test]
    fn a_refused_path_is_asked_once_and_a_yes_applies_a_session_grant() {
        use super::super::{
            HookEnvironment, HookPlatform, HookScope, HooksDecision, HooksExecArgs,
            ask_first_if_refused_before, build_exec_output, compute_exec_decision_internal,
        };
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        let ws = dunce::canonicalize(&ws).unwrap();
        let cache = tmp.path().join("tool-cache");
        std::fs::create_dir_all(&cache).unwrap();
        let cache = dunce::canonicalize(&cache).unwrap();
        record_refusal(&ws, &cache.join("heavy.lock.holder"), ScopeAccess::Rw);

        let env = HookEnvironment {
            home_dir: tmp.path().join("home"),
            project_root: ws.clone(),
            current_exe: PathBuf::from("/usr/local/bin/ahma"),
        };
        let args = HooksExecArgs {
            platform: HookPlatform::Claude,
            scope: HookScope::Project,
            managed_id: super::super::MANAGED_ID_DEFAULT_SHELL_V1.to_string(),
        };
        let cfg = crate::shell::cli::AppConfig::default();
        let input = serde_json::json!({
            "session_id": "session-1",
            "cwd": ws.display().to_string(),
            "tool_input": { "command": "make heavy" }
        });
        let decide = || {
            let d = compute_exec_decision_internal(&input, HookScope::Project, &env, true, false);
            ask_first_if_refused_before(d, &input, &args, &env, &cfg)
        };

        // A read-only command could not use a write grant: no dialog for it,
        // and it does not use up the session's one question.
        let reader = serde_json::json!({
            "session_id": "session-1",
            "cwd": ws.display().to_string(),
            "tool_input": { "command": "git status" }
        });
        let d = compute_exec_decision_internal(&reader, HookScope::Project, &env, true, false);
        assert!(
            matches!(
                ask_first_if_refused_before(d, &reader, &args, &env, &cfg),
                HooksDecision::AllowRewrite { .. }
            ),
            "no question before a reader"
        );

        let first = decide();
        assert!(matches!(first, HooksDecision::AskGrant { .. }), "{first:?}");
        let out = build_exec_output(first, HookPlatform::Claude);
        let hs = &out["hookSpecificOutput"];
        assert_eq!(hs["permissionDecision"], "ask", "{out}");
        assert!(
            hs["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .contains(&cache.display().to_string()),
            "{out}"
        );
        let command = hs["updatedInput"]["command"].as_str().unwrap().to_string();
        let token = command
            .split_whitespace()
            .skip_while(|t| t.trim_matches('\'') != APPROVE_FLAG)
            .nth(1)
            .map(|t| t.trim_matches('\'').to_string())
            .expect("the rewritten command carries a token");

        assert!(
            matches!(decide(), HooksDecision::AllowRewrite { .. }),
            "asked once per session, whatever the answer"
        );

        let line = apply_approval(&token, &ws, "make heavy").expect("a line");
        assert!(line.contains("approved"), "{line}");
        let granted =
            crate::sandbox::session_scopes_for(&super::super::resolve_hook_sandbox_scopes(&ws));
        assert!(
            granted
                .iter()
                .any(|g| g.path == cache && g.access.is_write()),
            "the yes became a session grant: {granted:?}"
        );
        let again = apply_approval(&token, &ws, "make heavy").unwrap();
        assert!(again.contains("already used"), "{again}");
    }

    #[test]
    fn a_command_carrying_a_token_of_its_own_is_refused() {
        use super::super::{HookScope, HooksDecision, compute_exec_decision_internal};
        let tmp = tempfile::tempdir().unwrap();
        let env = super::super::HookEnvironment {
            home_dir: tmp.path().join("home"),
            project_root: tmp.path().to_path_buf(),
            current_exe: PathBuf::from("/usr/local/bin/ahma"),
        };
        for command in [
            "ahma hooks run-shell --approve-grant t --wrapped-by ahma-hooks-wrapper-v1 --command x",
            "ahma hooks run-shell --approve-grant t --command x",
        ] {
            let input = serde_json::json!({
                "cwd": tmp.path().display().to_string(),
                "tool_input": { "command": command }
            });
            let d = compute_exec_decision_internal(&input, HookScope::Project, &env, true, false);
            assert!(
                matches!(d, HooksDecision::Refuse { .. }),
                "{command}: {d:?}"
            );
        }
    }

    #[test]
    fn an_agent_written_command_cannot_carry_a_token() {
        assert!(carries_approval(
            "'/x/ahma' 'hooks' 'run-shell' '--approve-grant' 'abc' '--command' 'make'"
        ));
        assert!(!carries_approval(
            "'/x/ahma' 'hooks' 'run-shell' '--command' 'make'"
        ));
    }

    #[test]
    fn a_grant_covers_its_workspace_and_access() {
        let ws = PathBuf::from("/w/proj");
        let g = |path: &str, access, workspace: Option<&str>| PersistentScope {
            path: PathBuf::from(path),
            access,
            workspace: workspace.map(PathBuf::from),
            granted_by: None,
            granted_at: None,
            note: None,
            expires_at: None,
        };
        let p = Path::new("/c/neubit/lock");
        assert!(covered(
            &[g("/c/neubit", ScopeAccess::Rw, None)],
            &ws,
            p,
            ScopeAccess::Rw
        ));
        assert!(!covered(
            &[g("/c/neubit", ScopeAccess::Ro, None)],
            &ws,
            p,
            ScopeAccess::Rw
        ));
        assert!(covered(
            &[g("/c/neubit", ScopeAccess::Ro, None)],
            &ws,
            p,
            ScopeAccess::Ro
        ));
        assert!(!covered(
            &[g("/c/neubit", ScopeAccess::Rw, Some("/w/other"))],
            &ws,
            p,
            ScopeAccess::Rw
        ));
    }

    #[test]
    fn the_dialog_says_what_yes_and_no_mean() {
        let q = Question {
            dir: PathBuf::from("/opt/neubit-cache"),
            access: ScopeAccess::Rw,
            covers: vec![PathBuf::from("/opt/neubit-cache/heavy.lock.holder")],
        };
        let r = ask_reason(&q);
        assert!(
            r.contains("refused write access to /opt/neubit-cache"),
            "{r}"
        );
        assert!(r.contains("heavy.lock.holder"), "{r}");
        assert!(r.contains("for this session"), "{r}");
        assert!(
            r.contains("will not ask about it again this session"),
            "{r}"
        );
        assert!(r.contains("ahma sandbox grant /opt/neubit-cache"), "{r}");
    }
}
