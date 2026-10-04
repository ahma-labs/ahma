//! Questions ahma asks through a harness's own permission dialog (SPEC R-PERM.10).
//!
//! A hooked command (Claude Code's Bash tool, rewritten through `ahma hooks
//! run-shell`) can be refused a path mid-command, where no dialog is possible:
//! the hook already answered before the command started, and the kernel policy
//! is fixed at spawn. So the refusal is recorded here, per workspace, and the
//! *next* hooked command in that workspace is answered `ask` instead of
//! `allow`. The harness shows its own yes/no dialog; a yes runs the rewritten
//! command with a one-use token that applies a session grant first.
//!
//! Two rules keep this from becoming a stream of questions:
//! - **One question per directory per harness session.** A question already
//!   asked in this session, answered or not, is never asked again; a "no" is
//!   therefore remembered without the harness telling anyone.
//! - **Related paths become one question.** Refusals that share a parent are
//!   asked as that parent when widening to it is safe ([`widening_is_safe`]):
//!   never home or above, never a folder many tools share (`~/.cache`,
//!   `~/Library`, …), never a folder holding credentials, never a folder of
//!   projects. Otherwise each path is asked on its own.
//!
//! Stored as one owner-only JSON file per workspace under
//! `runtime_dir()/harness-asks/`, beside the session grants. A sandboxed
//! command cannot write there (the runtime directory is outside every scope),
//! which is what makes the token's word trustworthy.
//!
//! The TUI reads the same records ([`load_all`], [`pending_questions`]) and
//! may answer a question first (SPEC R-PERM.10(e)); it records the answer
//! ([`mark_answered`]) so the harness dialog does not ask it again.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{PersistentScope, ScopeAccess};
use crate::scope_grant::{GrantRisk, classify_grant_risk, sensitive_dirs};

/// Refusals and questions older than this are forgotten: the session tier's
/// own bound ([`crate::session_grants::MAX_AGE_SECS`]).
pub const MAX_AGE_SECS: u64 = crate::session_grants::MAX_AGE_SECS;

/// How long a yes in the harness dialog may take to arrive: the token in the
/// rewritten command is honoured only this long after the question was asked.
pub const TOKEN_TTL_SECS: u64 = 60 * 60;

/// Folders under home that many tools share. A grant may name a folder
/// *inside* one (`~/.cache/neubit`), never the shared folder itself, which
/// would hand every other tool's data to the sandbox.
const SHARED_ROOTS: &[&[&str]] = &[
    &[".cache"],
    &[".config"],
    &[".local"],
    &[".local", "share"],
    &[".local", "state"],
    &["Library"],
    &["Library", "Caches"],
    &["Library", "Application Support"],
    &["Library", "Preferences"],
    &["Library", "Containers"],
    &["Library", "Group Containers"],
    &["Documents"],
    &["Downloads"],
    &["Desktop"],
];

/// One path a hooked command was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    /// The refused path, as the command reported it.
    pub path: PathBuf,
    /// The directory a grant for it would name (a file's parent, when safe).
    pub grant_dir: PathBuf,
    pub access: ScopeAccess,
    /// Unix seconds of the latest refusal of this path.
    pub at: u64,
    /// The harness process whose life bounds a session grant for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_pid: Option<u32>,
    /// Digest of the command that was refused: the dialog asks when that
    /// command is run again, never before an unrelated one (SPEC R-PERM.10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_digest: Option<String>,
}

/// Whether a refusal recorded for `recorded` (a command digest) is one to
/// ask about before `command` runs: the same command run again. A refusal
/// recorded without a digest (an older ahma) matches any command.
pub fn refused_for(recorded: Option<&str>, command: &str) -> bool {
    recorded.is_none_or(|d| d == crate::digest::sha256_hex(command.as_bytes()))
}

/// A question put to the human through a harness dialog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asked {
    /// The harness session it was asked in.
    pub session_id: String,
    pub question: Question,
    /// The one-use token the rewritten command carries.
    pub token: String,
    /// Digest of the command the dialog approved: a token is honoured only
    /// for that command.
    pub command_digest: String,
    pub asked_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_pid: Option<u32>,
    /// Whether the token has been spent.
    #[serde(default)]
    pub used: bool,
}

/// What one question asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    /// The directory the grant names.
    pub dir: PathBuf,
    /// Read-write when any covered refusal was a write.
    pub access: ScopeAccess,
    /// The refused paths this one grant would cover, for the dialog text.
    pub covers: Vec<PathBuf>,
}

/// A question a human answered at another surface (the TUI) before the
/// harness dialog asked it (SPEC R-PERM.10(e)). Whatever the answer, the
/// dialog does not ask about these refusals again: a yes became a grant, and
/// a no is remembered like a no in the dialog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answered {
    /// The directory the question named.
    pub dir: PathBuf,
    /// The access it asked for; a read-write question settles reads too.
    pub access: ScopeAccess,
    /// Unix seconds of the answer.
    pub at: u64,
    /// The harness process whose refusals it answered. Refusals a later
    /// harness session records are its own question, as in the dialog
    /// (one question per directory per harness session). `None` answers all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_pid: Option<u32>,
}

impl Answered {
    /// Whether this answer settles `refusal`.
    fn settles(&self, refusal: &Refusal) -> bool {
        refusal.path.starts_with(&self.dir)
            && (self.access.is_write() || !refusal.access.is_write())
            && (self.harness_pid.is_none() || self.harness_pid == refusal.harness_pid)
    }
}

/// Everything recorded for one workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceAsks {
    pub workspace: PathBuf,
    #[serde(default)]
    pub refusals: Vec<Refusal>,
    #[serde(default)]
    pub asked: Vec<Asked>,
    /// Questions answered at another surface before the dialog asked them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answered: Vec<Answered>,
    /// Signatures the SSH key broker refused a hooked command (SPEC R-CRED.3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ssh_refusals: Vec<SshRefusal>,
    /// Questions about them put to the human through the harness dialog.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ssh_asked: Vec<AskedSsh>,
}

/// One signature the SSH key broker refused a hooked command because no grant
/// allowed it (SPEC R-CRED.3): asked about when that command runs again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshRefusal {
    /// The key, `SHA256:…`.
    pub key: String,
    /// The key's comment, for the dialog text.
    #[serde(default)]
    pub key_comment: String,
    /// What it would have signed for, as [`crate::ssh_sign::SshSignGrant`]
    /// names it (`host:SHA256:…`, `sshsig:<namespace>`).
    pub destination: String,
    /// What the human reads for the destination: the server's names.
    #[serde(default)]
    pub label: String,
    /// Unix seconds of the latest refusal.
    pub at: u64,
    /// The harness process whose life bounds a session grant for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_pid: Option<u32>,
    /// Digest of the command that was refused: the dialog asks when that
    /// command is run again, never before an unrelated one (SPEC R-PERM.10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_digest: Option<String>,
}

impl SshRefusal {
    fn same_subject(&self, other: &SshRefusal) -> bool {
        self.key == other.key && self.destination == other.destination
    }
}

/// A question about an SSH signature put to the human through a harness
/// dialog, with the one-use token the approved command carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskedSsh {
    pub session_id: String,
    pub refusal: SshRefusal,
    pub token: String,
    pub command_digest: String,
    pub asked_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_pid: Option<u32>,
    #[serde(default)]
    pub used: bool,
}

/// The directory these records live in: `runtime_dir()/harness-asks`.
pub fn default_dir() -> Option<PathBuf> {
    Some(crate::hub::runtime_dir()?.join("harness-asks"))
}

/// Whether a question may name `dir` in place of the narrower paths beneath it.
///
/// The hard denylist ([`classify_grant_risk`]) applies first. On top of it a
/// *widened* question also refuses: anything with fewer than two path
/// components, home or an ancestor of it, a folder many tools share or one
/// containing such a folder, a folder containing credentials, and a folder of
/// projects (two or more child directories that are git repositories).
pub fn widening_is_safe(dir: &Path, home: Option<&Path>, scopes: &[PathBuf]) -> bool {
    let normal = dir
        .components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .count();
    if normal < 2 {
        return false;
    }
    if matches!(
        classify_grant_risk(dir, home, scopes),
        GrantRisk::Refused(_)
    ) {
        return false;
    }
    if let Some(home) = home {
        if home.starts_with(dir) {
            return false;
        }
        let shared = SHARED_ROOTS
            .iter()
            .map(|parts| parts.iter().fold(home.to_path_buf(), |p, c| p.join(c)));
        if shared
            .chain(sensitive_dirs(home))
            .any(|guarded| guarded.starts_with(dir))
        {
            return false;
        }
    }
    let repos = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().join(".git").exists())
                .count()
        })
        .unwrap_or(0);
    repos < 2
}

/// Group `refusals` into as few questions as is safe.
///
/// Each refusal starts as a question for its own grant directory. Two
/// questions merge when one directory contains the other, or when their
/// nearest common ancestor passes [`widening_is_safe`]; this repeats until
/// nothing more merges. A question is read-write when any refusal it covers
/// was a write.
pub fn group(refusals: &[Refusal], home: Option<&Path>, scopes: &[PathBuf]) -> Vec<Question> {
    let mut questions: Vec<Question> = refusals
        .iter()
        .map(|r| Question {
            dir: r.grant_dir.clone(),
            access: r.access,
            covers: vec![r.path.clone()],
        })
        .collect();
    loop {
        let mut merged = None;
        'pairs: for i in 0..questions.len() {
            for j in (i + 1)..questions.len() {
                let (a, b) = (&questions[i].dir, &questions[j].dir);
                // Identical directories are one question. Anything wider, even
                // a directory one of them already names, is a widening for the
                // other and must pass the same test.
                let target = if a == b {
                    Some(a.clone())
                } else {
                    common_ancestor(a, b).filter(|c| widening_is_safe(c, home, scopes))
                };
                if let Some(dir) = target {
                    merged = Some((i, j, dir));
                    break 'pairs;
                }
            }
        }
        let Some((i, j, dir)) = merged else {
            return questions;
        };
        let absorbed = questions.remove(j);
        let into = &mut questions[i];
        into.dir = dir;
        if absorbed.access == ScopeAccess::Rw {
            into.access = ScopeAccess::Rw;
        }
        for path in absorbed.covers {
            if !into.covers.contains(&path) {
                into.covers.push(path);
            }
        }
    }
}

/// The file one workspace's records live in.
fn file_for(dir: &Path, workspace: &Path) -> PathBuf {
    let key = crate::digest::sha256_hex(workspace.as_os_str().as_encoded_bytes());
    dir.join(format!("{}.json", &key[..16]))
}

/// Run `f` on `workspace`'s records under an exclusive lock, saving what it
/// leaves. Expired refusals and questions are dropped first.
fn update<T>(
    dir: &Path,
    workspace: &Path,
    now: u64,
    f: impl FnOnce(&mut WorkspaceAsks) -> T,
) -> Result<T> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let file = file_for(dir, workspace);
    let _lock = crate::fs_lock::FsLock::acquire(&file.with_extension("lock"))
        .with_context(|| format!("lock {}", file.display()))?;
    let mut asks = load(dir, workspace, now);
    let out = f(&mut asks);
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&asks)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &file).with_context(|| format!("rename to {}", file.display()))?;
    Ok(out)
}

/// `workspace`'s records, without what has expired. Empty when there are none
/// or they cannot be read: an unreadable record asks nothing.
pub fn load(dir: &Path, workspace: &Path, now: u64) -> WorkspaceAsks {
    let mut asks = std::fs::read(file_for(dir, workspace))
        .ok()
        .and_then(|raw| serde_json::from_slice::<WorkspaceAsks>(&raw).ok())
        .filter(|a| a.workspace == workspace)
        .unwrap_or_else(|| WorkspaceAsks {
            workspace: workspace.to_path_buf(),
            ..WorkspaceAsks::default()
        });
    forget_expired(&mut asks, now);
    asks
}

/// Drop the refusals, questions and answers older than [`MAX_AGE_SECS`].
fn forget_expired(asks: &mut WorkspaceAsks, now: u64) {
    asks.refusals
        .retain(|r| now.saturating_sub(r.at) <= MAX_AGE_SECS);
    asks.asked
        .retain(|a| now.saturating_sub(a.asked_at) <= MAX_AGE_SECS);
    asks.answered
        .retain(|a| now.saturating_sub(a.at) <= MAX_AGE_SECS);
    asks.ssh_refusals
        .retain(|r| now.saturating_sub(r.at) <= MAX_AGE_SECS);
    asks.ssh_asked
        .retain(|a| now.saturating_sub(a.asked_at) <= MAX_AGE_SECS);
}

/// Remember that the SSH key broker refused a hooked command in `workspace`.
pub fn record_ssh_refusal(dir: &Path, workspace: &Path, refusal: SshRefusal) -> Result<()> {
    update(dir, workspace, refusal.at, |asks| {
        asks.ssh_refusals.retain(|r| !r.same_subject(&refusal));
        asks.ssh_refusals.push(refusal);
    })
}

/// The SSH signature to ask about before `command` runs in harness session
/// `session_id`: one refused when `command` last ran ([`refused_for`]), that
/// no grant covers (`covered`), and not yet asked in this session, whatever
/// the answer was.
pub fn next_ssh_question(
    asks: &WorkspaceAsks,
    session_id: &str,
    command: &str,
    covered: &dyn Fn(&SshRefusal) -> bool,
) -> Option<SshRefusal> {
    asks.ssh_refusals
        .iter()
        .filter(|r| refused_for(r.command_digest.as_deref(), command) && !covered(r))
        .find(|r| {
            !asks
                .ssh_asked
                .iter()
                .any(|a| a.session_id == session_id && a.refusal.same_subject(r))
        })
        .cloned()
}

/// Record that `refusal` was put to the human in `session_id` for `command`,
/// returning the one-use token the approved command will carry.
pub fn mark_ssh_asked(
    dir: &Path,
    workspace: &Path,
    session_id: &str,
    refusal: &SshRefusal,
    command: &str,
    harness_pid: Option<u32>,
    now: u64,
) -> Result<String> {
    let token = uuid::Uuid::new_v4().simple().to_string();
    let asked = AskedSsh {
        session_id: session_id.to_string(),
        refusal: refusal.clone(),
        token: token.clone(),
        command_digest: crate::digest::sha256_hex(command.as_bytes()),
        asked_at: now,
        harness_pid,
        used: false,
    };
    update(dir, workspace, now, |asks| asks.ssh_asked.push(asked))?;
    Ok(token)
}

/// Spend an SSH-signature `token`, as [`take_approved`] does for a path.
pub fn take_ssh_approved(
    dir: &Path,
    workspace: &Path,
    token: &str,
    command: &str,
    now: u64,
) -> Option<AskedSsh> {
    let digest = crate::digest::sha256_hex(command.as_bytes());
    update(dir, workspace, now, |asks| {
        let asked = asks.ssh_asked.iter_mut().find(|a| {
            a.token == token
                && !a.used
                && a.command_digest == digest
                && now.saturating_sub(a.asked_at) <= TOKEN_TTL_SECS
        })?;
        asked.used = true;
        let asked = asked.clone();
        asks.ssh_refusals
            .retain(|r| !r.same_subject(&asked.refusal));
        Some(asked)
    })
    .ok()
    .flatten()
}

/// Every workspace's records in `dir`, without what has expired, ordered by
/// workspace: what a surface that lists every waiting question reads (the
/// TUI, SPEC R-PERM.10(e)). A file that cannot be read, or whose name is not
/// the one its workspace would have, is skipped.
pub fn load_all(dir: &Path, now: u64) -> Vec<WorkspaceAsks> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut all: Vec<WorkspaceAsks> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| {
            let asks = serde_json::from_slice::<WorkspaceAsks>(&std::fs::read(&p).ok()?).ok()?;
            (file_for(dir, &asks.workspace) == p).then_some(asks)
        })
        .map(|mut asks| {
            forget_expired(&mut asks, now);
            asks
        })
        .collect();
    all.sort_by(|a, b| a.workspace.cmp(&b.workspace));
    all
}

/// Whether a grant in `granted` already lets a command in `workspace` reach
/// `path` with `access` (SPEC R-PERM.10(c)): the test the hook and the TUI
/// both apply before asking.
pub fn grant_covers(
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
        let root = crate::config::expand_home(&g.path);
        let root = dunce::canonicalize(&root).unwrap_or(root);
        applies && (g.access.is_write() || !access.is_write()) && path.starts_with(&root)
    })
}

/// Remember that a hooked command in `workspace` was refused `refusal.path`.
pub fn record_refusal(dir: &Path, workspace: &Path, refusal: Refusal) -> Result<()> {
    update(dir, workspace, refusal.at, |asks| {
        asks.refusals
            .retain(|r| !(r.path == refusal.path && r.access == refusal.access));
        asks.refusals.push(refusal);
    })
}

/// The question to ask next in harness session `session_id`, if any.
///
/// Refusals that `covered` says a grant already covers are skipped, as are
/// those under a question already asked in this session (one question per
/// directory per session, whatever the answer was) and those a human answered
/// at another surface ([`mark_answered`]).
pub fn next_question(
    asks: &WorkspaceAsks,
    session_id: &str,
    covered: &dyn Fn(&Path, ScopeAccess) -> bool,
    home: Option<&Path>,
    scopes: &[PathBuf],
) -> Option<Question> {
    let asked_here: Vec<&Path> = asks
        .asked
        .iter()
        .filter(|a| a.session_id == session_id)
        .map(|a| a.question.dir.as_path())
        .collect();
    let open: Vec<Refusal> = asks
        .refusals
        .iter()
        .filter(|r| !covered(&r.path, r.access))
        .filter(|r| !asked_here.iter().any(|d| r.path.starts_with(d)))
        .filter(|r| !asks.answered.iter().any(|a| a.settles(r)))
        .cloned()
        .collect();
    group(&open, home, scopes)
        .into_iter()
        .find(|q| !denylisted(q, home, scopes))
}

/// Every question still waiting in `asks`, for a surface that may answer
/// ahead of the harness dialog (the TUI, SPEC R-PERM.10(e)).
///
/// A refusal waits while no grant covers it (`covered`), no harness dialog
/// has asked about it for the harness process that was refused, and no
/// surface has answered it. Grouped as [`next_question`] groups them, without
/// what the denylist refuses.
pub fn pending_questions(
    asks: &WorkspaceAsks,
    covered: &dyn Fn(&Path, ScopeAccess) -> bool,
    home: Option<&Path>,
    scopes: &[PathBuf],
) -> Vec<Question> {
    let open: Vec<Refusal> = asks
        .refusals
        .iter()
        .filter(|r| !covered(&r.path, r.access))
        .filter(|r| {
            !asks.asked.iter().any(|a| {
                r.path.starts_with(&a.question.dir)
                    && (a.harness_pid.is_none() || a.harness_pid == r.harness_pid)
            })
        })
        .filter(|r| !asks.answered.iter().any(|a| a.settles(r)))
        .cloned()
        .collect();
    group(&open, home, scopes)
        .into_iter()
        .filter(|q| !denylisted(q, home, scopes))
        .collect()
}

/// Whether the hard denylist refuses `question`'s directory: never asked.
fn denylisted(question: &Question, home: Option<&Path>, scopes: &[PathBuf]) -> bool {
    matches!(
        classify_grant_risk(&question.dir, home, scopes),
        GrantRisk::Refused(_)
    )
}

/// Record that a human answered `question` at another surface for the
/// refusals of `harness_pid`, so the harness dialog does not ask it too.
pub fn mark_answered(
    dir: &Path,
    workspace: &Path,
    question: &Question,
    harness_pid: Option<u32>,
    now: u64,
) -> Result<()> {
    let answered = Answered {
        dir: question.dir.clone(),
        access: question.access,
        at: now,
        harness_pid,
    };
    update(dir, workspace, now, |asks| asks.answered.push(answered))
}

/// Record that `question` was put to the human in `session_id` for `command`,
/// returning the one-use token the approved command will carry.
pub fn mark_asked(
    dir: &Path,
    workspace: &Path,
    session_id: &str,
    question: &Question,
    command: &str,
    harness_pid: Option<u32>,
    now: u64,
) -> Result<String> {
    let token = uuid::Uuid::new_v4().simple().to_string();
    let asked = Asked {
        session_id: session_id.to_string(),
        question: question.clone(),
        token: token.clone(),
        command_digest: crate::digest::sha256_hex(command.as_bytes()),
        asked_at: now,
        harness_pid,
        used: false,
    };
    update(dir, workspace, now, |asks| asks.asked.push(asked))?;
    Ok(token)
}

/// Spend `token`: the question it answers, when it was issued for exactly
/// `command` in `workspace`, is unspent, and is younger than
/// [`TOKEN_TTL_SECS`]. A token is good once.
pub fn take_approved(
    dir: &Path,
    workspace: &Path,
    token: &str,
    command: &str,
    now: u64,
) -> Option<Asked> {
    let digest = crate::digest::sha256_hex(command.as_bytes());
    update(dir, workspace, now, |asks| {
        let asked = asks.asked.iter_mut().find(|a| {
            a.token == token
                && !a.used
                && a.command_digest == digest
                && now.saturating_sub(a.asked_at) <= TOKEN_TTL_SECS
        })?;
        asked.used = true;
        let asked = asked.clone();
        let dir = &asked.question.dir;
        asks.refusals.retain(|r| !r.path.starts_with(dir));
        Some(asked)
    })
    .ok()
    .flatten()
}

/// The deepest directory containing both `a` and `b`, if they share any.
fn common_ancestor(a: &Path, b: &Path) -> Option<PathBuf> {
    a.ancestors()
        .find(|anc| b.starts_with(anc))
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC R-CRED.3 through R-PERM.10: a refused signature is asked about
    /// once per harness session, and its token approves only the command it
    /// was issued for, once.
    #[test]
    fn a_refused_signature_is_asked_once_and_its_token_spent_once() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Path::new("/ws/repo");
        let refusal = SshRefusal {
            key: "SHA256:key".into(),
            key_comment: "me@laptop".into(),
            destination: "host:SHA256:host".into(),
            label: "github.com".into(),
            at: 100,
            harness_pid: Some(7),
            command_digest: Some(crate::digest::sha256_hex(b"git push")),
        };
        record_ssh_refusal(dir.path(), ws, refusal.clone()).unwrap();
        record_ssh_refusal(
            dir.path(),
            ws,
            SshRefusal {
                at: 101,
                ..refusal.clone()
            },
        )
        .unwrap();
        let asks = load(dir.path(), ws, 102);
        assert_eq!(asks.ssh_refusals.len(), 1, "one subject, one refusal");
        let never = |_: &SshRefusal| false;
        assert!(
            next_ssh_question(&asks, "s1", "cargo build", &never).is_none(),
            "an unrelated command is not held for it"
        );
        let q = next_ssh_question(&asks, "s1", "git push", &never).expect("asked");
        assert_eq!(q.label, "github.com");
        let token = mark_ssh_asked(dir.path(), ws, "s1", &q, "git push", Some(7), 103).unwrap();
        let asks = load(dir.path(), ws, 104);
        assert!(
            next_ssh_question(&asks, "s1", "git push", &never).is_none(),
            "once per session"
        );
        assert!(
            next_ssh_question(&asks, "s2", "git push", &never).is_some(),
            "another session asks"
        );
        assert!(
            next_ssh_question(&asks, "s2", "git push", &|_| true).is_none(),
            "a grant that covers it asks nothing"
        );
        assert!(take_ssh_approved(dir.path(), ws, &token, "git pull", 105).is_none());
        let asked = take_ssh_approved(dir.path(), ws, &token, "git push", 105).expect("approved");
        assert_eq!(asked.refusal.key, "SHA256:key");
        assert!(
            take_ssh_approved(dir.path(), ws, &token, "git push", 106).is_none(),
            "once"
        );
        assert!(load(dir.path(), ws, 107).ssh_refusals.is_empty());
    }

    fn refusal(path: &Path, access: ScopeAccess) -> Refusal {
        let grant_dir = if path.extension().is_some() {
            path.parent().unwrap().to_path_buf()
        } else {
            path.to_path_buf()
        };
        Refusal {
            path: path.to_path_buf(),
            grant_dir,
            access,
            at: 100,
            harness_pid: None,
            command_digest: None,
        }
    }

    #[test]
    fn related_refusals_become_one_question_one_level_below_a_shared_folder() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        let rs = [
            refusal(&h.join(".cache/neubit/heavy.lock.holder"), ScopeAccess::Rw),
            refusal(&h.join(".cache/neubit/db/index.db"), ScopeAccess::Rw),
            refusal(&h.join(".cache/neubit/tmp"), ScopeAccess::Ro),
        ];
        let qs = group(&rs, Some(h), &[]);
        assert_eq!(qs.len(), 1, "{qs:?}");
        assert_eq!(qs[0].dir, h.join(".cache/neubit"));
        assert_eq!(qs[0].access, ScopeAccess::Rw, "a write among them");
        assert_eq!(qs[0].covers.len(), 3);
    }

    #[test]
    fn a_shared_folder_is_never_the_question() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        let rs = [
            refusal(&h.join(".cache/neubit/x.lock"), ScopeAccess::Rw),
            refusal(&h.join(".cache/other-tool/y.lock"), ScopeAccess::Rw),
        ];
        let qs = group(&rs, Some(h), &[]);
        assert_eq!(qs.len(), 2, "two tools' caches stay two questions: {qs:?}");
        assert!(!widening_is_safe(&h.join(".cache"), Some(h), &[]));
        assert!(!widening_is_safe(&h.join("Library/Caches"), Some(h), &[]));
        assert!(widening_is_safe(&h.join(".cache/neubit"), Some(h), &[]));
    }

    #[test]
    fn home_credentials_and_roots_are_never_the_question() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        assert!(!widening_is_safe(h, Some(h), &[]), "home itself");
        assert!(
            !widening_is_safe(h.parent().unwrap(), Some(h), &[]),
            "above home"
        );
        assert!(
            !widening_is_safe(&h.join(".config"), Some(h), &[]),
            "holds gh tokens"
        );
        assert!(
            !widening_is_safe(&h.join(".ssh"), Some(h), &[]),
            "a credential folder itself"
        );
        assert!(!widening_is_safe(Path::new("/"), Some(h), &[]));
        assert!(!widening_is_safe(Path::new("/usr"), Some(h), &[]));
        let rs = [
            refusal(&h.join("notes.txt"), ScopeAccess::Rw),
            refusal(&h.join(".config/tool/a.toml"), ScopeAccess::Rw),
        ];
        assert_eq!(group(&rs, Some(h), &[]).len(), 2, "never widened to home");
    }

    #[test]
    fn a_folder_of_projects_is_never_the_question() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        for repo in ["alpha", "beta"] {
            std::fs::create_dir_all(h.join("src").join(repo).join(".git")).unwrap();
        }
        assert!(!widening_is_safe(&h.join("src"), Some(h), &[]));
        let rs = [
            refusal(&h.join("src/alpha/build.log"), ScopeAccess::Rw),
            refusal(&h.join("src/beta/build.log"), ScopeAccess::Rw),
        ];
        assert_eq!(group(&rs, Some(h), &[]).len(), 2);
    }

    fn store() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("asks");
        let ws = root.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        (root, dir, ws)
    }

    #[test]
    fn a_refusal_is_asked_once_per_session_and_a_token_is_good_once() {
        let (root, dir, ws) = store();
        let home = root.path().join("home");
        let cache = home.join(".cache/neubit");
        std::fs::create_dir_all(&cache).unwrap();
        let mut r = refusal(&cache.join("heavy.lock.holder"), ScopeAccess::Rw);
        r.harness_pid = Some(4242);
        record_refusal(&dir, &ws, r).unwrap();

        let none = |_: &Path, _: ScopeAccess| false;
        let asks = load(&dir, &ws, 200);
        let q = next_question(&asks, "s1", &none, Some(&home), std::slice::from_ref(&ws))
            .expect("a question");
        assert_eq!(q.dir, cache);
        let token = mark_asked(&dir, &ws, "s1", &q, "make heavy", Some(4242), 200).unwrap();

        let asks = load(&dir, &ws, 201);
        assert!(
            next_question(&asks, "s1", &none, Some(&home), std::slice::from_ref(&ws)).is_none(),
            "never asked twice in one session, whatever the answer"
        );
        assert!(
            next_question(&asks, "s2", &none, Some(&home), std::slice::from_ref(&ws)).is_some(),
            "another session may be asked"
        );

        assert!(
            take_approved(&dir, &ws, &token, "make other", 202).is_none(),
            "a token answers only the command it was issued for"
        );
        assert!(
            take_approved(&dir, &ws, "not-a-token", "make heavy", 202).is_none(),
            "an unknown token is refused"
        );
        let got = take_approved(&dir, &ws, &token, "make heavy", 202).expect("approved");
        assert_eq!(got.question.dir, cache);
        assert_eq!(got.harness_pid, Some(4242));
        assert!(
            take_approved(&dir, &ws, &token, "make heavy", 203).is_none(),
            "a token is good once"
        );
    }

    #[test]
    fn a_token_expires_and_a_covered_refusal_asks_nothing() {
        let (root, dir, ws) = store();
        let home = root.path().join("home");
        let cache = home.join(".cache/tool");
        std::fs::create_dir_all(&cache).unwrap();
        record_refusal(&dir, &ws, refusal(&cache.join("a.lock"), ScopeAccess::Rw)).unwrap();
        let asks = load(&dir, &ws, 200);

        let granted = |p: &Path, _: ScopeAccess| p.starts_with(&cache);
        assert!(
            next_question(&asks, "s1", &granted, Some(&home), &[]).is_none(),
            "already granted: nothing to ask"
        );

        let none = |_: &Path, _: ScopeAccess| false;
        let q = next_question(&asks, "s1", &none, Some(&home), &[]).unwrap();
        let token = mark_asked(&dir, &ws, "s1", &q, "c", None, 200).unwrap();
        assert!(take_approved(&dir, &ws, &token, "c", 200 + TOKEN_TTL_SECS + 1).is_none());
    }

    #[test]
    fn old_refusals_are_forgotten() {
        let (_root, dir, ws) = store();
        let mut r = refusal(Path::new("/opt/tool/cache/x.db"), ScopeAccess::Rw);
        r.at = 100;
        record_refusal(&dir, &ws, r).unwrap();
        assert_eq!(load(&dir, &ws, 100 + MAX_AGE_SECS + 1).refusals.len(), 0);
        assert_eq!(load(&dir, &ws, 101).refusals.len(), 1);
    }

    /// SPEC R-PERM.10(e): the TUI lists what no surface has answered, and an
    /// answer there means the dialog does not ask it — for that harness
    /// session. A later session's refusal of the same path is its own question.
    #[test]
    fn an_answer_at_the_tui_means_the_dialog_does_not_ask() {
        let (root, dir, ws) = store();
        let home = root.path().join("home");
        let cache = home.join(".cache/neubit");
        std::fs::create_dir_all(&cache).unwrap();
        let mut r = refusal(&cache.join("heavy.lock.holder"), ScopeAccess::Rw);
        r.harness_pid = Some(4242);
        record_refusal(&dir, &ws, r.clone()).unwrap();
        let none = |_: &Path, _: ScopeAccess| false;
        let scopes = std::slice::from_ref(&ws);

        let asks = load(&dir, &ws, 200);
        let waiting = pending_questions(&asks, &none, Some(&home), scopes);
        assert_eq!(waiting.len(), 1, "{waiting:?}");
        assert_eq!(waiting[0].dir, cache);
        let granted = |p: &Path, _: ScopeAccess| p.starts_with(&cache);
        assert!(
            pending_questions(&asks, &granted, Some(&home), scopes).is_empty(),
            "a covered refusal is not waiting"
        );

        mark_answered(&dir, &ws, &waiting[0], Some(4242), 201).unwrap();
        let asks = load(&dir, &ws, 202);
        assert!(
            pending_questions(&asks, &none, Some(&home), scopes).is_empty(),
            "answered: no longer waiting"
        );
        assert!(
            next_question(&asks, "s1", &none, Some(&home), scopes).is_none(),
            "the dialog does not ask what the TUI answered"
        );

        let mut later = r;
        later.harness_pid = Some(5151);
        later.at = 203;
        record_refusal(&dir, &ws, later).unwrap();
        let asks = load(&dir, &ws, 204);
        assert_eq!(
            pending_questions(&asks, &none, Some(&home), scopes).len(),
            1,
            "another harness session's refusal waits again"
        );
        assert!(next_question(&asks, "s2", &none, Some(&home), scopes).is_some());
    }

    /// A question the dialog put to the human for a harness process leaves
    /// the TUI's list; another process's refusal under it stays.
    #[test]
    fn a_question_the_dialog_asked_is_not_listed_for_that_harness() {
        let (root, dir, ws) = store();
        let home = root.path().join("home");
        let cache = home.join(".cache/neubit");
        std::fs::create_dir_all(&cache).unwrap();
        let mut r = refusal(&cache.join("heavy.lock.holder"), ScopeAccess::Rw);
        r.harness_pid = Some(4242);
        record_refusal(&dir, &ws, r).unwrap();
        let none = |_: &Path, _: ScopeAccess| false;
        let scopes = std::slice::from_ref(&ws);
        let asks = load(&dir, &ws, 200);
        let q = next_question(&asks, "s1", &none, Some(&home), scopes).unwrap();
        mark_asked(&dir, &ws, "s1", &q, "make heavy", Some(4242), 200).unwrap();

        let asks = load(&dir, &ws, 201);
        assert!(pending_questions(&asks, &none, Some(&home), scopes).is_empty());

        let mut other = refusal(&cache.join("other.lock"), ScopeAccess::Rw);
        other.harness_pid = Some(5151);
        record_refusal(&dir, &ws, other).unwrap();
        let asks = load(&dir, &ws, 202);
        let waiting = pending_questions(&asks, &none, Some(&home), scopes);
        assert_eq!(waiting.len(), 1, "{waiting:?}");
        assert_eq!(waiting[0].covers, vec![cache.join("other.lock")]);
    }

    #[test]
    fn load_all_reads_every_workspace_and_trusts_no_misnamed_file() {
        let (root, dir, ws) = store();
        let other = root.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        record_refusal(
            &dir,
            &ws,
            refusal(Path::new("/opt/a/x.db"), ScopeAccess::Rw),
        )
        .unwrap();
        record_refusal(
            &dir,
            &other,
            refusal(Path::new("/opt/b/y.db"), ScopeAccess::Rw),
        )
        .unwrap();
        let stray = WorkspaceAsks {
            workspace: root.path().join("elsewhere"),
            refusals: vec![refusal(Path::new("/opt/c/z.db"), ScopeAccess::Rw)],
            ..WorkspaceAsks::default()
        };
        std::fs::write(
            dir.join("0000000000000000.json"),
            serde_json::to_vec(&stray).unwrap(),
        )
        .unwrap();

        let all = load_all(&dir, 200);
        let got: Vec<PathBuf> = all.iter().map(|a| a.workspace.clone()).collect();
        let mut want = vec![ws.clone(), other.clone()];
        want.sort();
        assert_eq!(got, want, "both workspaces, never the misnamed file");
        assert!(load_all(&root.path().join("missing"), 200).is_empty());
        assert!(
            load_all(&dir, 100 + MAX_AGE_SECS + 1)
                .iter()
                .all(|a| a.refusals.is_empty()),
            "expired refusals are dropped"
        );
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
        assert!(grant_covers(
            &[g("/c/neubit", ScopeAccess::Rw, None)],
            &ws,
            p,
            ScopeAccess::Rw
        ));
        assert!(!grant_covers(
            &[g("/c/neubit", ScopeAccess::Ro, None)],
            &ws,
            p,
            ScopeAccess::Rw
        ));
        assert!(grant_covers(
            &[g("/c/neubit", ScopeAccess::Ro, None)],
            &ws,
            p,
            ScopeAccess::Ro
        ));
        assert!(!grant_covers(
            &[g("/c/neubit", ScopeAccess::Rw, Some("/w/other"))],
            &ws,
            p,
            ScopeAccess::Rw
        ));
    }

    #[test]
    fn a_parent_of_the_workspace_is_never_the_question() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        let ws = h.join("work/project");
        std::fs::create_dir_all(&ws).unwrap();
        assert!(!widening_is_safe(&h.join("work"), Some(h), &[ws]));
    }
}
