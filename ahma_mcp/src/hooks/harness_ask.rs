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
    // Offer the tool's own folder, not the deepest refused one (R-PERM.10.1).
    let grant_dir = crate::sandbox::grant_channel::offered_dir_for(
        path,
        &super::resolve_hook_sandbox_scopes(cwd),
    );
    let refusal = Refusal {
        path: path.to_path_buf(),
        grant_dir,
        access,
        at: ahma_common::session_grants::now_secs(),
        harness_pid: harness_pid(),
        command_digest: Some(ahma_common::harness_asks::command_digest(command)),
        programs: ahma_common::harness_asks::command_programs(command),
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
    // A command that cannot write (the write queue's read-only lane, R2.7.4)
    // could not use a write grant: asking before a `git status` would be noise.
    let may_write = !matches!(
        crate::adapter::lane::classify_shell_command(command),
        crate::adapter::workspace_queue::Lane::ReadOnly
    );
    // Asked before the refused command, or one running the same program,
    // runs again: a "no" then stops only the retry it was about, never an
    // unrelated command (SPEC R-PERM.10, R-PERM.10.1).
    let consider = |r: &harness_asks::Refusal| {
        (may_write || !r.access.is_write()) && harness_asks::refused_for(r, command)
    };
    let scopes = super::resolve_hook_sandbox_scopes(cwd);
    let mut granted = persistent.to_vec();
    granted.extend(crate::sandbox::session_scopes_for(&scopes));
    let home = ahma_common::config::ahma_home_dir().map(|h| dunce::canonicalize(&h).unwrap_or(h));
    let is_covered =
        |p: &Path, a: ScopeAccess| harness_asks::grant_covers(&granted, &workspace, p, a);
    // A grant must outlive this hook: the harness asking now is the one whose
    // dialog shows the question, so its life bounds the grant; a pid a
    // refusal recorded is only the fallback (it may be a harness that has
    // exited, or another window on the same workspace). With neither, nothing
    // is asked and the one-line note stays the answer. Choosing the question
    // and recording it asked is one locked step, so parallel hooks of one
    // session raise one dialog (SPEC R-PERM.3.8).
    harness_asks::ask_next(
        &dir,
        &workspace,
        session_id,
        &consider,
        &is_covered,
        home.as_deref(),
        &scopes,
        command,
        harness_pid(),
        now,
    )
    .ok()
    .flatten()
}

/// The workspace a yes to `question` asked in `cwd` is approved in, and the
/// binding the grant will be saved with: the same [`binding_for`] answer
/// [`persist_approval`] reaches, so the dialog says what a yes really does
/// (SPEC R5.4.11.1, R7).
///
/// [`binding_for`]: ahma_common::scope_grant::binding_for
pub(super) fn approval_binding(
    question: &Question,
    cwd: &Path,
    persistent: &[PersistentScope],
) -> (PathBuf, PathBuf) {
    let workspace = workspace_for(cwd);
    let home = ahma_common::config::ahma_home_dir();
    let (binding, _) = ahma_common::scope_grant::binding_for(
        &question.dir,
        &workspace,
        persistent,
        home.as_deref(),
    );
    (workspace, binding)
}

/// The dialog text for `question` (SPEC R-PERM.10.2): who was refused what,
/// what a yes allows for whom and how to undo it, what a no means — a few
/// plain sentences. The evidence and the tiers are in the agent's result and
/// `ahma explain`, not here: a paragraph under ahma's own wrapper was what a
/// human once approved without being able to read it.
pub(super) fn ask_reason(question: &Question, workspace: &Path, binding: &Path) -> String {
    let (refused, verb) = if question.access.is_write() {
        ("writing", "write")
    } else {
        ("reading", "read")
    };
    let home = ahma_common::config::ahma_home_dir();
    let dir = display_path(&question.dir, home.as_deref());
    let who = match question.programs.as_slice() {
        [] => "a command here".to_string(),
        [one] => one.clone(),
        [first, ..] => format!("{first} (and {} more)", question.programs.len() - 1),
    };
    let whom = if binding == workspace {
        format!("commands in {}", short_name(workspace))
    } else {
        format!(
            "commands in every workspace under {} (another there has it already)",
            display_path(binding, home.as_deref())
        )
    };
    let risk =
        match ahma_common::scope_grant::classify_grant_risk(&question.dir, home.as_deref(), &[]) {
            ahma_common::scope_grant::GrantRisk::High(why) => {
                format!(" Caution: {}.", why.join("; "))
            }
            _ => String::new(),
        };
    format!(
        "ahma: {who} was refused {refused} {dir}. Yes: let {whom} {verb} it, kept from now on \
         only if it fixes this run (undo: `ahma sandbox revoke {}`). No: don't ask again this \
         session.{risk}",
        question.dir.display(),
    )
}

/// `path` as a human reads it: under home, with `~`.
fn display_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// A workspace by its folder name, the way its owner calls it.
fn short_name(workspace: &Path) -> String {
    workspace
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| workspace.display().to_string())
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
        command_digest: ahma_common::harness_asks::command_digest(command),
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

/// The dialog text for an escape (SPEC R-ESCAPE.2, R-PERM.10.2): what could
/// not run, in its first sentence, and that a yes runs this one command with
/// the human's own full access.
pub(super) fn escape_ask_reason(refusal: &harness_asks::EscapeRefusal) -> String {
    let why = refusal.why.trim();
    let why = why.strip_prefix("Blocked: ").unwrap_or(why);
    let first = why
        .find(". ")
        .map_or(why, |end| &why[..end])
        .trim_end_matches('.');
    format!(
        "ahma: {first}. Yes: run this one command outside the sandbox, with your full access \
         (recorded in the audit log). No: keep it sandboxed; ahma won't ask again this session."
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

/// What spending an approval token did: the line to show before the run,
/// and, for a folder grant, the trial its run decides (SPEC R-PERM.10.3).
pub(super) struct Approval {
    pub line: String,
    pub trial: Option<Trial>,
}

/// A folder grant on trial for one run: in force as a session grant now,
/// remembered only if the run shows it helped.
pub(super) struct Trial {
    question: Question,
    workspace: PathBuf,
    session_file: Option<PathBuf>,
}

/// How the approved run ended, as far as a grant is concerned.
#[derive(Debug)]
pub(super) enum TrialOutcome {
    /// It exited 0.
    Succeeded,
    /// It failed, refused nothing, and could run in the sandbox.
    Failed,
    /// It failed on a refused path.
    Refused(PathBuf),
    /// It failed on something no grant opens (R6.2.13, R-ESCAPE).
    Unrunnable,
    /// It was stopped (a time limit) before it showed anything.
    Stopped,
}

/// What a trial's run says about the grant.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum TrialVerdict {
    Keep,
    Drop(&'static str),
    Undecided,
}

/// Keep a grant on `dir` when the run succeeded, or failed past it: not on
/// `dir` again, not unrunnable, and `dir` exists (it was there, or the run
/// made it). Drop it when the same folder was refused again, the command
/// cannot run in any sandbox, or nothing exists there. A stopped run decides
/// nothing.
pub(super) fn trial_verdict(dir: &Path, outcome: TrialOutcome) -> TrialVerdict {
    let canonical = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    match outcome {
        TrialOutcome::Succeeded => TrialVerdict::Keep,
        TrialOutcome::Stopped => TrialVerdict::Undecided,
        TrialOutcome::Unrunnable => {
            TrialVerdict::Drop("the command cannot run inside the sandbox at all")
        }
        TrialOutcome::Refused(path) if canonical(&path).starts_with(canonical(dir)) => {
            TrialVerdict::Drop("the same folder was refused again")
        }
        TrialOutcome::Refused(_) | TrialOutcome::Failed if dir.exists() => TrialVerdict::Keep,
        TrialOutcome::Refused(_) | TrialOutcome::Failed => {
            TrialVerdict::Drop("nothing exists there, and the run did not create it")
        }
    }
}

/// Spend an approval `token` for `command` in `cwd`: apply the session grant
/// the human approved in the harness dialog, for this run, and say so in one
/// line. `None`
/// when the token is unknown, spent, expired, for another command, or the
/// grant is now refused; then the command runs without it.
pub(super) fn apply_approval(token: &str, cwd: &Path, command: &str) -> Option<Approval> {
    let dir = harness_asks::default_dir()?;
    let workspace = workspace_for(cwd);
    let now = ahma_common::session_grants::now_secs();
    let line_only = |line: String| Approval { line, trial: None };
    let Some(asked) = harness_asks::take_approved(&dir, &workspace, token, command, now) else {
        if let Some(asked) = harness_asks::take_ssh_approved(&dir, &workspace, token, command, now)
        {
            return Some(line_only(apply_ssh_approval(&asked, &workspace, now)));
        }
        return Some(line_only(
            "ahma: the approval this command carries is unknown, already used or expired; it \
             runs without a new grant."
                .to_string(),
        ));
    };
    let q = asked.question;
    if let Some(why) = ahma_common::scope_grant::refusal_reason(&q.dir) {
        return Some(line_only(format!(
            "ahma: {} cannot be granted ({why}); the command runs without it.",
            q.dir.display()
        )));
    }
    let owner = asked.harness_pid?;
    // The session grant reaches this very command: its settings were read
    // after the token was spent. It is remembered only if the run shows it
    // helped (SPEC R-PERM.10.3).
    let session_file = crate::sandbox::record_session_grant(
        &q.dir,
        q.access,
        Some(&workspace),
        owner,
        "harness dialog",
    );
    let verb = if q.access.is_write() { "write" } else { "read" };
    Some(Approval {
        line: format!(
            "ahma: approved — this command may {verb} {}; it is remembered only if this run \
             shows it helped.",
            q.dir.display()
        ),
        trial: Some(Trial {
            question: q,
            workspace,
            session_file,
        }),
    })
}

/// Decide a trial from its run (SPEC R-PERM.10.3), and say what became of
/// the grant in one line: remembered, withdrawn, or this session only.
pub(super) fn finish_trial(trial: Trial, outcome: TrialOutcome) -> String {
    let q = &trial.question;
    let verb = if q.access.is_write() { "write" } else { "read" };
    match trial_verdict(&q.dir, outcome) {
        TrialVerdict::Keep => match persist_approval(q, &trial.workspace) {
            Ok(binding) if binding == trial.workspace => format!(
                "ahma: remembered — the run got past the refusal, so commands in this workspace \
                 may {verb} {} from now on (undo: `ahma sandbox revoke {}`).",
                q.dir.display(),
                q.dir.display()
            ),
            Ok(binding) => format!(
                "ahma: remembered — the run got past the refusal, so commands in every \
                 workspace under {} may {verb} {} from now on (undo: `ahma sandbox revoke {}`).",
                binding.display(),
                q.dir.display(),
                q.dir.display()
            ),
            Err(e) => format!(
                "ahma: the grant helped, but could not be remembered ({e:#}); it applies for \
                 this session. To keep it, run `ahma sandbox grant {}`.",
                q.dir.display()
            ),
        },
        TrialVerdict::Drop(why) => {
            if let Some(file) = &trial.session_file
                && let Err(e) = ahma_common::session_grants::remove(file)
            {
                tracing::warn!("could not withdraw the session grant: {e:#}");
            }
            format!(
                "ahma: the grant on {} did not help ({why}), so it was withdrawn and nothing \
                 was saved.",
                q.dir.display()
            )
        }
        TrialVerdict::Undecided => format!(
            "ahma: the run ended before it showed whether the grant on {} helped; it applies \
             for this session only. To keep it, run `ahma sandbox grant {}`.",
            q.dir.display(),
            q.dir.display()
        ),
    }
}

/// Remember a grant the human approved in the harness dialog (SPEC
/// R-PERM.10.1): persisted, bound by [`binding_for`] so a sibling clone that
/// approved the same directory shares one grant. Returns the binding.
///
/// [`binding_for`]: ahma_common::scope_grant::binding_for
fn persist_approval(q: &harness_asks::Question, workspace: &Path) -> anyhow::Result<PathBuf> {
    use ahma_common::config::{AhmaSettings, settings_path};
    use ahma_common::scope_grant::{NewGrant, binding_for, persist_grant};
    let file = settings_path().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    let existing = AhmaSettings::load_from_checked(&file)
        .map_err(|e| anyhow::anyhow!("{}", e.summary()))?
        .sandbox
        .persistent_scopes;
    let home = ahma_common::config::ahma_home_dir();
    let (binding, replaced) = binding_for(&q.dir, workspace, &existing, home.as_deref());
    persist_grant(
        &file,
        NewGrant {
            path: &q.dir,
            access: q.access,
            granted_by: Some("harness dialog".to_string()),
            granted_at: Some(chrono::Local::now().format("%Y-%m-%d").to_string()),
            note: None,
            surface: "harness",
            // Never above the workspace it was approved in (R-PERM.4.3).
            live_scopes: &[workspace.to_path_buf()],
            workspace: Some(&binding),
            expires_at: None,
        },
    )?;
    // After the wider grant is written, so a failure here leaves more
    // granted than asked for in one more place, never less.
    if !replaced.is_empty() {
        let _settings_lock = ahma_common::config::lock_settings_for_write(&file)?;
        let mut settings = AhmaSettings::load_from_checked(&file)
            .map_err(|e| anyhow::anyhow!("{}", e.summary()))?;
        for old in &replaced {
            settings.sandbox.revoke_scope(&q.dir, Some(old));
        }
        settings.save_to(&file)?;
    }
    Ok(binding)
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
        let approval = apply_approval(&token, &ws, "git push").expect("a line");
        assert!(approval.line.contains("approved"), "{}", approval.line);
        assert!(approval.trial.is_none(), "a signature is no folder grant");
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
            reason.contains("run this one command outside the sandbox")
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
    /// the token) applies a session grant once, for this command, and is
    /// remembered for later ones (SPEC R-PERM.10.1).
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
        // SPEC R-PERM.10.2: the dialog shows the command that will run, as the
        // agent wrote it, under one line of ahma's — not re-quoted into argv.
        #[cfg(unix)]
        {
            let (first, body) = command.split_once('\n').expect("a heredoc");
            assert!(first.contains("--command-stdin"), "{command}");
            assert!(!first.contains("make heavy"), "{command}");
            assert!(body.starts_with("make heavy\n"), "{command}");
        }
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

        let approval = apply_approval(&token, &ws, "make heavy").expect("a line");
        assert!(approval.line.contains("approved"), "{}", approval.line);
        let granted =
            crate::sandbox::session_scopes_for(&super::super::resolve_hook_sandbox_scopes(&ws));
        assert!(
            granted
                .iter()
                .any(|g| g.path == cache && g.access.is_write()),
            "the yes became a session grant: {granted:?}"
        );
        let file = ahma_common::config::settings_path().unwrap();
        let remembered = || {
            ahma_common::config::AhmaSettings::load_from_checked(&file)
                .map(|s| {
                    s.sandbox.persistent_scopes.iter().any(|s| {
                        s.path == cache
                            && s.access.is_write()
                            && s.workspace.as_deref() == Some(ws.as_path())
                    })
                })
                .unwrap_or(false)
        };
        // SPEC R-PERM.10.3: a yes is a trial; nothing is saved before the run.
        assert!(!remembered(), "saved before the run showed it helped");
        let trial = approval.trial.expect("a folder grant is a trial");
        let line = finish_trial(trial, TrialOutcome::Succeeded);
        assert!(line.contains("remembered"), "{line}");
        assert!(
            remembered(),
            "kept once the run succeeded, bound to this workspace"
        );
        let again = apply_approval(&token, &ws, "make heavy").unwrap();
        assert!(again.line.contains("already used"), "{}", again.line);
    }

    /// SPEC R-PERM.10.3: a grant is kept only when the run shows it helped.
    /// Field case: a yes saved a grant on `/Applications/Google`, a folder
    /// that does not exist, for a program that could not run in any sandbox;
    /// it stayed in settings.toml for good.
    #[test]
    fn a_grant_is_kept_only_when_the_run_shows_it_helped() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = dunce::canonicalize(tmp.path()).unwrap().join("cache");
        let missing = tmp.path().join("Google");
        std::fs::create_dir_all(&dir).unwrap();
        let elsewhere = tmp.path().join("other");
        let cases: [(&Path, TrialOutcome, &str); 8] = [
            (&dir, TrialOutcome::Succeeded, "keep"),
            (&missing, TrialOutcome::Succeeded, "keep"),
            (&dir, TrialOutcome::Failed, "keep"),
            (&dir, TrialOutcome::Refused(elsewhere.clone()), "keep"),
            (&dir, TrialOutcome::Refused(dir.join("x.lock")), "drop"),
            (&dir, TrialOutcome::Unrunnable, "drop"),
            (&missing, TrialOutcome::Failed, "drop"),
            (&dir, TrialOutcome::Stopped, "undecided"),
        ];
        for (dir, outcome, want) in cases {
            let label = format!("{outcome:?}");
            let got = match trial_verdict(dir, outcome) {
                TrialVerdict::Keep => "keep",
                TrialVerdict::Drop(_) => "drop",
                TrialVerdict::Undecided => "undecided",
            };
            assert_eq!(got, want, "{} {label}", dir.display());
        }
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
                programs: Vec::new(),
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

    /// SPEC R5.4.11.1, R7: a yes that shares the grant with every workspace
    /// under a parent folder must say so before it is given. The field case
    /// asked about "this workspace" and then reported "every workspace under
    /// /Volumes/T7/github".
    #[test]
    fn the_dialog_names_a_shared_binding_before_the_yes() {
        let q = Question {
            dir: PathBuf::from("/Volumes/T7/caches/swiftpm"),
            access: ScopeAccess::Rw,
            covers: vec![PathBuf::from("/Volumes/T7/caches/swiftpm/manifests")],
            programs: vec!["swift".into()],
        };
        let ws = PathBuf::from("/Volumes/T7/github/ahma");
        let shared = PathBuf::from("/Volumes/T7/github");
        let r = ask_reason(&q, &ws, &shared);
        assert!(
            r.contains("every workspace under /Volumes/T7/github"),
            "{r}"
        );
        assert!(!r.contains("commands in ahma"), "{r}");
    }

    /// SPEC R-PERM.10.2: who needs what, what a yes and a no mean, how to
    /// undo it — and nothing more. The field case's dialog was a paragraph
    /// under ahma's own wrapper argv; the human said yes to a folder that did
    /// not exist.
    #[test]
    fn the_dialog_says_who_needs_what_in_a_few_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = dunce::canonicalize(tmp.path())
            .unwrap()
            .join("neubit-cache");
        std::fs::create_dir_all(&cache).unwrap();
        let q = Question {
            dir: cache.clone(),
            access: ScopeAccess::Rw,
            covers: vec![cache.join("heavy.lock.holder")],
            programs: vec!["gradle".into()],
        };
        let ws = PathBuf::from("/Volumes/T7/github/neubit4");
        let r = ask_reason(&q, &ws, &ws);
        assert!(
            r.starts_with(&format!(
                "ahma: gradle was refused writing {}.",
                cache.display()
            )),
            "{r}"
        );
        assert!(
            r.contains(
                "Yes: let commands in neubit4 write it, kept from now on only if it fixes this run"
            ),
            "{r}"
        );
        assert!(
            r.contains(&format!("ahma sandbox revoke {}", cache.display())),
            "{r}"
        );
        assert!(r.contains("No: don't ask again this session."), "{r}");
        assert!(!r.contains('\n'), "{r}");
        let plumbing = 2 * cache.display().to_string().chars().count();
        let len = r.chars().count() - plumbing;
        assert!(len <= 200, "{len} chars besides the paths: {r}");
    }

    /// Without a program name (a record from an older ahma) it says "a
    /// command"; a path under home is written with `~`.
    #[test]
    fn the_dialog_without_a_program_and_under_home() {
        let Some(home) = ahma_common::config::ahma_home_dir() else {
            return;
        };
        let dir = home.join("Library/Application Support/Google/Chrome");
        let q = Question {
            dir: dir.clone(),
            access: ScopeAccess::Ro,
            covers: vec![dir.join("Crashpad")],
            programs: Vec::new(),
        };
        let ws = PathBuf::from("/w/neubit4");
        let r = ask_reason(&q, &ws, &ws);
        assert!(
            r.starts_with(
                "ahma: a command here was refused reading ~/Library/Application Support/Google/Chrome."
            ),
            "{r}"
        );
        assert!(r.contains("read it, kept from now on"), "{r}");
    }

    /// SPEC R-PERM.10.2, R-ESCAPE.2: the escape says in plain words what
    /// failed and what a yes gives away.
    #[test]
    fn the_escape_dialog_is_plain() {
        let refusal = harness_asks::EscapeRefusal {
            command_digest: "d".into(),
            why: crate::sandbox::mach::blocked_line("com.google.Chrome.MachPortRendezvousServer"),
            at: 0,
            harness_pid: None,
        };
        let r = escape_ask_reason(&refusal);
        assert!(
            r.starts_with("ahma: Google Chrome can't run inside ahma's sandbox"),
            "{r}"
        );
        assert!(
            r.contains("Yes: run this one command outside the sandbox, with your full access"),
            "{r}"
        );
        assert!(r.contains("No: keep it sandboxed"), "{r}");
        assert!(!r.contains("Blocked:"), "{r}");
        assert!(r.chars().count() <= 360, "{} chars: {r}", r.chars().count());
        // A kernel record's line is cut to its first sentence.
        let kernel = harness_asks::EscapeRefusal {
            why: "Blocked: ahma's kernel sandbox refused `lsopen` on /x/App.app by open (12). No \
                  grant or setting changes this: opening an app hands it to launchd. If the \
                  command must do it, run the same command again."
                .into(),
            ..refusal
        };
        let r = escape_ask_reason(&kernel);
        assert!(
            r.starts_with(
                "ahma: ahma's kernel sandbox refused `lsopen` on /x/App.app by open (12)."
            ),
            "{r}"
        );
        assert!(!r.contains("No grant or setting"), "{r}");
    }

    /// SPEC R-PERM.10: a refusal the command shrugged off is said in one
    /// line and never becomes a dialog. Apple's `python3` was refused its
    /// bytecode cache, ran fine without it, and the next run asked the human
    /// to approve a folder the command never needed.
    #[test]
    fn a_refusal_in_a_command_that_succeeded_is_noted_never_asked() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        let ws = dunce::canonicalize(&ws).unwrap();
        let cache = tmp.path().join("pycache");
        std::fs::create_dir_all(&cache).unwrap();
        let cache = dunce::canonicalize(&cache).unwrap();
        let who = crate::sandbox::grant_channel::HookRequester {
            harness: Some("claude".into()),
            session_id: Some("s1".into()),
            scopes: vec![ws.clone()],
            command: Some("python3 apply.py".into()),
        };
        let output = format!(
            "applied\nerror: could not write {}: Operation not permitted",
            cache.join("apply.pyc").display()
        );
        let shown = super::super::succeeded_output(&output, &who);
        assert!(shown.starts_with(&output), "{shown}");
        assert!(shown.contains("went on"), "the one-line note: {shown}");
        let dir = harness_asks::default_dir().unwrap();
        let now = ahma_common::session_grants::now_secs();
        assert!(
            harness_asks::load(&dir, &workspace_for(&ws), now)
                .refusals
                .is_empty(),
            "nothing remembered, so no dialog for a refusal that cost nothing"
        );
    }

    #[test]
    fn pre_exec_path_outside_sandbox_is_recorded_and_asked_on_rerun() {
        use super::super::{
            HookEnvironment, HookPlatform, HookScope, HooksDecision, HooksExecArgs,
            ask_first_if_refused_before, compute_exec_decision_internal,
            report_shell_execution_error,
        };
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        let ws = dunce::canonicalize(&ws).unwrap();
        let cache = tmp.path().join("neubit-cache");
        std::fs::create_dir_all(&cache).unwrap();
        let cache = dunce::canonicalize(&cache).unwrap();

        let who = crate::sandbox::grant_channel::HookRequester {
            harness: Some("claude".into()),
            session_id: Some("s1".into()),
            scopes: vec![ws.clone()],
            command: Some("scripts/heavy build".into()),
        };
        let err = crate::sandbox::SandboxError::PathOutsideSandbox {
            path: cache.join("lock"),
            scopes: vec![ws.clone()],
        };
        let _ = report_shell_execution_error(err.into(), &who, &ws);

        if harness_pid().is_none() {
            return;
        }

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

        // Testing sessionId casing (camelCase)
        let input = serde_json::json!({
            "sessionId": "s1",
            "cwd": ws.display().to_string(),
            "tool_input": { "command": "scripts/heavy build" }
        });
        let d = compute_exec_decision_internal(&input, HookScope::Project, &env, true, false);
        let decision = ask_first_if_refused_before(d, &input, &args, &env, &cfg);
        assert!(
            matches!(decision, HooksDecision::AskGrant { .. }),
            "PathOutsideSandbox was recorded and asked even with camelCase sessionId: {decision:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ssh_refusal_without_command_still_asks_git_retry() {
        use crate::credentials::ssh_agent::broker::{
            Decision, Destination, SignConsent, SignRequest,
        };
        use crate::credentials::ssh_agent::consent::RecordedConsent;
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        let ws = dunce::canonicalize(&ws).unwrap();
        let workspace = workspace_for(&ws);
        let owner = std::process::id();

        // Consent created without for_command (e.g. MCP run_terminal_command)
        let consent = RecordedConsent::new(workspace.clone(), None, Some(owner));
        let request = SignRequest {
            key_fingerprint: "SHA256:mcpkey".into(),
            key_comment: "me@laptop".into(),
            destination: Destination::Host {
                host_key_fingerprint: "SHA256:github".into(),
                names: vec!["github.com".into()],
            },
        };
        assert!(matches!(consent.decide(&request).await, Decision::Deny(_)));

        // Unrelated command is not held
        assert!(next_ssh_ask(&ws, "s-mcp", "cargo build", None).is_none());

        // Git command IS asked
        let (refusal, _token) =
            next_ssh_ask(&ws, "s-mcp", "git fetch origin", None).expect("git command asks");
        assert_eq!(refusal.key, "SHA256:mcpkey");
    }

    #[test]
    fn nested_sandbox_records_escape_and_asks_on_rerun() {
        use super::super::{
            HookEnvironment, HookPlatform, HookScope, HooksDecision, HooksExecArgs,
            ask_first_if_refused_before, compute_exec_decision_internal,
            report_shell_execution_error,
        };
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        let ws = dunce::canonicalize(&ws).unwrap();

        let cmd = "xcodebuild -workspace Neubit.xcworkspace -scheme Neubit build";
        let who = crate::sandbox::grant_channel::HookRequester {
            harness: Some("claude".into()),
            session_id: Some("s1".into()),
            scopes: vec![ws.clone()],
            command: Some(cmd.into()),
        };
        let err = crate::sandbox::SandboxError::Unfixable {
            details: "Blocked: ahma's kernel sandbox refused nested `sandbox-exec` (macOS does not allow nested sandboxes).\n\nA tool in this command applied its own sandbox (`sandbox-exec`) and macOS refused to nest it inside ahma's: `sandbox_apply: Operation not permitted`.".into(),
        };
        let reported = report_shell_execution_error(err.into(), &who, &ws);
        assert!(reported.is_err());
        let err_msg = reported.unwrap_err().to_string();
        assert!(
            err_msg.contains("-IDEPackageSupportDisableManifestSandbox=YES")
                || err_msg.contains("ONCE OUTSIDE")
                || err_msg.contains("run the same command again")
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
            "session_id": "s1",
            "cwd": ws.display().to_string(),
            "tool_input": { "command": cmd }
        });
        let d = compute_exec_decision_internal(&input, HookScope::Project, &env, true, false);
        let decision = ask_first_if_refused_before(d, &input, &args, &env, &cfg);
        assert!(
            matches!(decision, HooksDecision::AskGrant { .. }),
            "Nested sandbox refusal offers escape ask on re-run: {decision:?}"
        );
        if let HooksDecision::AskGrant { reason, .. } = decision {
            assert!(reason.contains("outside the sandbox"));
        }
    }
}
