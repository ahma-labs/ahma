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

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::ScopeAccess;
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

/// Everything recorded for one workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceAsks {
    pub workspace: PathBuf,
    #[serde(default)]
    pub refusals: Vec<Refusal>,
    #[serde(default)]
    pub asked: Vec<Asked>,
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
    asks.refusals
        .retain(|r| now.saturating_sub(r.at) <= MAX_AGE_SECS);
    asks.asked
        .retain(|a| now.saturating_sub(a.asked_at) <= MAX_AGE_SECS);
    asks
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
/// those under a question already asked in this session: one question per
/// directory per session, whatever the answer was.
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
        .cloned()
        .collect();
    group(&open, home, scopes).into_iter().find(|q| {
        !matches!(
            classify_grant_risk(&q.dir, home, scopes),
            GrantRisk::Refused(_)
        )
    })
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

    #[test]
    fn a_parent_of_the_workspace_is_never_the_question() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        let ws = h.join("work/project");
        std::fs::create_dir_all(&ws).unwrap();
        assert!(!widening_is_safe(&h.join("work"), Some(h), &[ws]));
    }
}
