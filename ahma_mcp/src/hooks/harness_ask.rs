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
pub(super) fn record_refusal(cwd: &Path, path: &Path, access: ScopeAccess, command: &str) {
    let Some(dir) = harness_asks::default_dir() else {
        return;
    };
    let refusal = Refusal {
        path: path.to_path_buf(),
        grant_dir: crate::sandbox::grant_channel::grant_dir_for(path),
        access,
        at: ahma_common::session_grants::now_secs(),
        harness_pid: harness_pid(),
        command_digest: Some(ahma_common::digest::sha256_hex(command.as_bytes())),
    };
    if let Err(e) = harness_asks::record_refusal(&dir, &workspace_for(cwd), refusal) {
        tracing::debug!("harness question not recorded: {e:#}");
    }
}

/// The question to put to the human before `command` runs again in `cwd`, in
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
    // Asked when the refused command is run again: a "no" then stops only
    // the retry it was about, never an unrelated command (SPEC R-PERM.10).
    asks.refusals.retain(|r| {
        (may_write || !r.access.is_write())
            && harness_asks::refused_for(r.command_digest.as_deref(), command)
    });
    if asks.refusals.is_empty() {
        return None;
    }
    let scopes = super::resolve_hook_sandbox_scopes(cwd);
    let mut granted = persistent.to_vec();
    granted.extend(crate::sandbox::session_scopes_for(&scopes));
    let home = ahma_common::config::ahma_home_dir().map(|h| dunce::canonicalize(&h).unwrap_or(h));
    let is_covered =
        |p: &Path, a: ScopeAccess| harness_asks::grant_covers(&granted, &workspace, p, a);
    let question =
        harness_asks::next_question(&asks, session_id, &is_covered, home.as_deref(), &scopes)?;
    // A grant must outlive this hook: without the harness's pid nothing can
    // bound it, so nothing is asked and the one-line note stays the answer.
    // The harness asking now is the one whose dialog shows the question, so
    // its life bounds the grant. A stored pid is only a fallback: it may be a
    // harness that has exited, or another window on the same workspace.
    let pid = harness_pid().or_else(|| asks.refusals.iter().find_map(|r| r.harness_pid))?;
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

/// The SSH signature to ask about before `command` runs in `cwd`, with the
/// token that approves it (SPEC R-CRED.3, R-PERM.10): a signature the broker
/// refused an earlier command here, that no grant covers now, not yet asked
/// in this harness session.
pub(super) fn next_ssh_ask(
    cwd: &Path,
    session_id: &str,
    command: &str,
    settings_file: Option<&Path>,
) -> Option<(harness_asks::SshRefusal, String)> {
    let dir = harness_asks::default_dir()?;
    let workspace = workspace_for(cwd);
    let now = ahma_common::session_grants::now_secs();
    let asks = harness_asks::load(&dir, &workspace, now);
    if asks.ssh_refusals.is_empty() {
        return None;
    }
    let mut grants = settings_file
        .and_then(|f| ahma_common::config::AhmaSettings::load_from_result(f).ok())
        .map(|s| s.sandbox.ssh_sign)
        .unwrap_or_default();
    if let Some(sessions) = ahma_common::ssh_sign::session_dir() {
        grants.extend(ahma_common::ssh_sign::active_sessions(
            &sessions,
            now,
            &crate::sandbox::pid_alive,
        ));
    }
    let covered = |r: &harness_asks::SshRefusal| {
        ahma_common::ssh_sign::allowed(&grants, &r.key, &r.destination, &workspace, now)
    };
    let refusal = harness_asks::next_ssh_question(&asks, session_id, command, &covered)?;
    let pid = harness_pid().or(refusal.harness_pid)?;
    let token = harness_asks::mark_ssh_asked(
        &dir,
        &workspace,
        session_id,
        &refusal,
        command,
        Some(pid),
        now,
    )
    .ok()?;
    Some((refusal, token))
}

/// The dialog text for a refused SSH signature: which key, for which server,
/// what a yes allows and for how long, and how to make it permanent.
pub(super) fn ssh_ask_reason(refusal: &harness_asks::SshRefusal) -> String {
    let key = if refusal.key_comment.is_empty() {
        refusal.key.clone()
    } else {
        format!("{} ({})", refusal.key, refusal.key_comment)
    };
    format!(
        "ahma: when this command last ran it asked to use your SSH key {key} for {} ({}); the \
         key itself never enters the sandbox. Approve to let commands in this workspace sign \
         with it for {} for this session, then run this command. Deny and ahma will not ask \
         about it again this session. To allow it always: `ahma permissions grant ssh-sign \
         \"{} for {}\"`.",
        refusal.label, refusal.destination, refusal.label, refusal.key, refusal.destination,
    )
}

/// Remember that `command` in `cwd` was refused something no grant or setting
/// allows (SPEC R-ESCAPE.1), so the harness dialog can offer to run exactly
/// it once outside the sandbox when it is run again.
pub(super) fn record_escape(cwd: &Path, command: &str, why: &str) {
    let Some(dir) = harness_asks::default_dir() else {
        return;
    };
    let refusal = harness_asks::EscapeRefusal {
        command_digest: ahma_common::digest::sha256_hex(command.as_bytes()),
        why: why.to_string(),
        at: ahma_common::session_grants::now_secs(),
        harness_pid: harness_pid(),
    };
    if let Err(e) = harness_asks::record_escape(&dir, &workspace_for(cwd), refusal) {
        tracing::debug!("escape question not recorded: {e:#}");
    }
}

/// The escape question before `command` runs again, with its token.
pub(super) fn next_escape_ask(
    cwd: &Path,
    session_id: &str,
    command: &str,
) -> Option<(harness_asks::EscapeRefusal, String)> {
    let dir = harness_asks::default_dir()?;
    let workspace = workspace_for(cwd);
    let now = ahma_common::session_grants::now_secs();
    let asks = harness_asks::load(&dir, &workspace, now);
    let refusal = harness_asks::next_escape_question(&asks, session_id, command)?;
    let token =
        harness_asks::mark_escape_asked(&dir, &workspace, session_id, &refusal, now).ok()?;
    Some((refusal, token))
}

/// The dialog text for an escape (SPEC R-ESCAPE.2): what was refused, that a
/// yes runs this exact command once **without** ahma's sandbox, and that it
/// is recorded.
pub(super) fn escape_ask_reason(refusal: &harness_asks::EscapeRefusal) -> String {
    format!(
        "ahma: this exact command was refused something no grant or setting can allow ({}). \
         Approve to run it ONCE OUTSIDE ahma's sandbox: its writes are not confined to the \
         workspace, and the run is recorded in the audit log. Deny to keep it sandboxed; ahma \
         will not ask again this session.",
        refusal.why
    )
}

/// Spend an escape `token` for exactly `command` in `cwd`: `Some` (the
/// refusal it answers) when this command may now run once outside the
/// sandbox. A token the agent wrote itself is refused before this
/// ([`carries_approval`]), so only a human's yes reaches here.
pub(super) fn take_escape(
    token: &str,
    cwd: &Path,
    command: &str,
) -> Option<harness_asks::EscapeRefusal> {
    let dir = harness_asks::default_dir()?;
    let now = ahma_common::session_grants::now_secs();
    harness_asks::take_escape_approved(&dir, &workspace_for(cwd), token, command, now)
        .map(|asked| asked.refusal)
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
        if let Some(asked) = harness_asks::take_ssh_approved(&dir, &workspace, token, command, now)
        {
            return Some(apply_ssh_approval(&asked, &workspace, now));
        }
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

/// Record the session grant a yes to an SSH-signature question gives.
fn apply_ssh_approval(asked: &harness_asks::AskedSsh, workspace: &Path, now: u64) -> String {
    let r = &asked.refusal;
    let recorded = asked.harness_pid.and_then(|owner| {
        let dir = ahma_common::ssh_sign::session_dir()?;
        let grant = ahma_common::ssh_sign::SshSignGrant {
            key: r.key.clone(),
            destination: r.destination.clone(),
            label: r.label.clone(),
            workspace: workspace.to_path_buf(),
            granted_at: now,
            expires_at: None,
            granted_by: Some("harness dialog".into()),
            owner_pid: Some(owner),
        };
        ahma_common::ssh_sign::record_session(&dir, &grant).ok()
    });
    match recorded {
        Some(_) => {
            ahma_common::permissions::append_audit(&ahma_common::permissions::audit_entry(
                ahma_common::config::fmt_utc_datetime(now),
                ahma_common::permissions::AuditAction::Grant,
                ahma_common::permissions::GrantKind::SshSign,
                format!("{} for {}", r.key, r.destination),
                None,
                ahma_common::permissions::GrantTier::Session,
                Some("harness dialog".into()),
            ));
            format!(
                "ahma: approved — commands in this workspace may sign with {} for {} for this \
                 session.",
                r.key, r.label
            )
        }
        None => {
            "ahma: the approval could not be recorded; the command runs without it.".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC R-PERM.10: the dialog asks when the refused command is run again,
    /// not before whatever command comes next. A "no" used to block an
    /// unrelated command, and an unrelated command was held for a question
    /// about a path it never touches.
    #[test]
    fn a_refusal_is_asked_about_when_its_command_runs_again() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        let ws = dunce::canonicalize(&ws).unwrap();
        let cache = tmp.path().join("tool-cache");
        std::fs::create_dir_all(&cache).unwrap();
        let cache = dunce::canonicalize(&cache).unwrap();
        record_refusal(&ws, &cache.join("x.lock"), ScopeAccess::Rw, "make heavy");
        if harness_pid().is_none() {
            // No harness above this test process to bound a grant: nothing
            // is asked anywhere, which `next_ask` already pins.
            return;
        }
        assert!(
            next_ask(&ws, "s1", "cargo build", &[]).is_none(),
            "another command is not held for it"
        );
        assert!(next_ask(&ws, "s1", "make heavy", &[]).is_some());
    }

    /// SPEC R-CRED.3 through R-PERM.10, the whole loop at the hook: the
    /// broker refuses a signature no grant allows and remembers it; before
    /// the next command the dialog asks; the approved command's token records
    /// a session grant; the same signature is then allowed.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_refused_signature_is_asked_about_and_a_yes_lets_it_sign() {
        use crate::credentials::ssh_agent::broker::{
            Decision, Destination, SignConsent, SignRequest,
        };
        use crate::credentials::ssh_agent::consent::RecordedConsent;
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        let ws = dunce::canonicalize(&ws).unwrap();
        let workspace = workspace_for(&ws);
        // SAFETY: getpid has no preconditions; this process is the owner.
        let owner = std::process::id();
        let consent =
            RecordedConsent::new(workspace.clone(), None, Some(owner)).for_command("git push");
        let request = SignRequest {
            key_fingerprint: "SHA256:key".into(),
            key_comment: "me@laptop".into(),
            destination: Destination::Host {
                host_key_fingerprint: "SHA256:host".into(),
                names: vec!["github.com".into()],
            },
        };
        assert!(matches!(consent.decide(&request).await, Decision::Deny(_)));

        assert!(
            next_ssh_ask(&ws, "session-1", "cargo build", None).is_none(),
            "an unrelated command is not held for it (R-PERM.10)"
        );
        let (refusal, token) =
            next_ssh_ask(&ws, "session-1", "git push", None).expect("the dialog asks");
        let reason = ssh_ask_reason(&refusal);
        assert!(
            reason.contains("github.com") && reason.contains("never enters the sandbox"),
            "{reason}"
        );
        assert!(
            next_ssh_ask(&ws, "session-1", "git push", None).is_none(),
            "once per harness session"
        );
        let line = apply_approval(&token, &ws, "git push").expect("a line");
        assert!(line.contains("approved"), "{line}");
        assert_eq!(consent.decide(&request).await, Decision::Allow);
    }

    /// SPEC R-ESCAPE: what nothing allows is offered, when this exact command
    /// is run again, as one run outside the sandbox; the dialog says so, and
    /// the token runs it once.
    #[test]
    fn an_unfixable_refusal_is_offered_once_as_one_unsandboxed_run() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        let ws = dunce::canonicalize(&ws).unwrap();
        let cmd = "open -a Simulator";
        record_escape(&ws, cmd, "Blocked: ahma's kernel sandbox refused `lsopen`");
        assert!(next_escape_ask(&ws, "s1", "open -a Xcode").is_none());
        let (refusal, token) = next_escape_ask(&ws, "s1", cmd).expect("asked");
        let reason = escape_ask_reason(&refusal);
        assert!(
            reason.contains("ONCE OUTSIDE")
                && reason.contains("audit log")
                && reason.contains("lsopen"),
            "{reason}"
        );
        assert!(
            next_escape_ask(&ws, "s1", cmd).is_none(),
            "once per session"
        );
        assert!(
            take_escape(&token, &ws, "open -a Xcode").is_none(),
            "only this command"
        );
        assert!(take_escape(&token, &ws, cmd).is_some());
        assert!(take_escape(&token, &ws, cmd).is_none(), "once");
    }

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
        record_refusal(
            &ws,
            &cache.join("heavy.lock.holder"),
            ScopeAccess::Rw,
            "make heavy",
        );

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

    /// The question's grant is bound to the harness asking now, never to a
    /// pid stored with an older refusal (a harness that has exited, or another
    /// window on the same workspace).
    #[test]
    fn the_grant_is_bound_to_the_harness_asking_now() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        let ws = dunce::canonicalize(&ws).unwrap();
        let cache = tmp.path().join("tool-cache");
        std::fs::create_dir_all(&cache).unwrap();
        let cache = dunce::canonicalize(&cache).unwrap();
        let dir = harness_asks::default_dir().unwrap();
        let stale = u32::MAX - 7;
        harness_asks::record_refusal(
            &dir,
            &workspace_for(&ws),
            Refusal {
                path: cache.join("lock"),
                grant_dir: cache.clone(),
                access: ScopeAccess::Rw,
                at: ahma_common::session_grants::now_secs(),
                harness_pid: Some(stale),
                command_digest: None,
            },
        )
        .unwrap();
        let live = harness_pid().expect("the test runner has a parent");
        next_ask(&ws, "s-live", "make", &[]).expect("a question");
        let asks = harness_asks::load(
            &dir,
            &workspace_for(&ws),
            ahma_common::session_grants::now_secs(),
        );
        let asked = asks
            .asked
            .iter()
            .find(|a| a.session_id == "s-live")
            .unwrap();
        assert_eq!(asked.harness_pid, Some(live), "not the stale {stale}");
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
