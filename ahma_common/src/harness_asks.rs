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
    &["Library", "Developer"],
    &["Library", "Developer", "Xcode"],
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
    /// The programs that command ran ([`command_programs`]): a later command
    /// running one of them is asked about too (SPEC R-PERM.10.1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub programs: Vec<String>,
}

/// Words that start a shell command segment without being the program that
/// needs a grant: navigation, printing and inspection. A refusal is never
/// matched to a later command through one of these.
const NOT_THE_PROGRAM: &[&str] = &[
    "cd", "pushd", "popd", "echo", "printf", "true", "false", ":", "export", "set", "unset",
    "test", "[", "pwd", "sleep", "ls", "cat", "head", "tail", "grep", "wc", "which", "type",
];

/// Words that run the program named after them.
const RUNS_THE_NEXT: &[&str] = &["env", "time", "nice", "nohup", "exec", "command", "sudo"];

/// The programs a shell command line runs, by file name, in order: the first
/// word of each `&&`, `||`, `;`, `|` or `&` segment, past `VAR=value`
/// assignments and wrappers such as `env` and `time`, skipping
/// navigation and printing (`cd`, `echo`, `ls`, …). `cd ios && xcodebuild -scheme A build` runs
/// `["xcodebuild"]`. A heuristic over the text, not a shell parse: what it
/// gets wrong costs one extra question or one missed one, never a grant.
pub fn command_programs(command: &str) -> Vec<String> {
    let mut programs = Vec::new();
    for word in command_words(command) {
        // `"$CH"` runs whatever the variable holds: no name to match on.
        if word.contains(['$', '`']) {
            continue;
        }
        let word = word.as_str();
        let name = Path::new(word)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or(word)
            .to_string();
        // `2>&1` splits at its `&`; what follows is a descriptor, not a program.
        let is_name = name.chars().any(char::is_alphabetic);
        if is_name && !NOT_THE_PROGRAM.contains(&name.as_str()) && !programs.contains(&name) {
            programs.push(name);
        }
    }
    programs
}

/// The program word of each command a shell line runs, as written (quotes
/// removed): past `VAR=value` assignments and wrappers that run the next
/// word (`env`, `time`, `nohup`, `sudo`, …); heredoc bodies, strings and comments run nothing and
/// give none. Unlike [`command_programs`] it keeps paths, variables and
/// navigation words, for callers that judge the word itself.
pub fn command_words(command: &str) -> Vec<String> {
    shell_segments(command)
        .into_iter()
        .filter_map(|segment| {
            segment.into_iter().find(|w| {
                !((w.contains('=') && !w.starts_with('-'))
                    || RUNS_THE_NEXT.contains(&w.as_str())
                    || w.starts_with('-'))
            })
        })
        .collect()
}

/// The words of each command a shell line runs, split at unquoted `&`, `|`,
/// `;`, newlines and parentheses. Quotes are removed and keep their contents
/// one word (`"/Applications/Google Chrome.app/…"`); heredoc bodies and
/// comments are dropped, since they run nothing. Still a heuristic, not a
/// shell: it only has to tell what runs from what is merely written down.
fn shell_segments(command: &str) -> Vec<Vec<String>> {
    let mut segments: Vec<Vec<String>> = vec![Vec::new()];
    let mut word = String::new();
    let mut in_word = false;
    // Heredocs announced on the current line: (closing word, strip tabs).
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;
    let end_word = |segments: &mut Vec<Vec<String>>, word: &mut String, in_word: &mut bool| {
        if *in_word {
            segments
                .last_mut()
                .expect("one segment")
                .push(std::mem::take(word));
            *in_word = false;
        }
    };
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' => {
                in_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    word.push(chars[i]);
                    i += 1;
                }
            }
            '"' => {
                in_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                    }
                    word.push(chars[i]);
                    i += 1;
                }
            }
            '\\' if i + 1 < chars.len() => {
                in_word = true;
                i += 1;
                if chars[i] != '\n' {
                    word.push(chars[i]);
                }
            }
            '#' if !in_word => {
                while i + 1 < chars.len() && chars[i + 1] != '\n' {
                    i += 1;
                }
            }
            '<' if chars.get(i + 1) == Some(&'<') && chars.get(i + 2) != Some(&'<') => {
                end_word(&mut segments, &mut word, &mut in_word);
                i += 2;
                let strip_tabs = chars.get(i) == Some(&'-');
                if strip_tabs {
                    i += 1;
                }
                while chars.get(i).is_some_and(|c| *c == ' ' || *c == '\t') {
                    i += 1;
                }
                let mut delimiter = String::new();
                while let Some(&d) = chars.get(i) {
                    if d.is_whitespace() || matches!(d, ';' | '&' | '|' | '(' | ')' | '<' | '>') {
                        break;
                    }
                    if !matches!(d, '\'' | '"' | '\\') {
                        delimiter.push(d);
                    }
                    i += 1;
                }
                if !delimiter.is_empty() {
                    heredocs.push((delimiter, strip_tabs));
                }
                continue;
            }
            '\n' => {
                end_word(&mut segments, &mut word, &mut in_word);
                segments.push(Vec::new());
                // Skip the bodies of the heredocs this line announced.
                for (delimiter, strip_tabs) in std::mem::take(&mut heredocs) {
                    loop {
                        let start = i + 1;
                        if start >= chars.len() {
                            i = chars.len();
                            break;
                        }
                        let end = chars[start..]
                            .iter()
                            .position(|c| *c == '\n')
                            .map_or(chars.len(), |p| start + p);
                        let line: String = chars[start..end].iter().collect();
                        i = end;
                        let line = if strip_tabs {
                            line.trim_start_matches('\t')
                        } else {
                            line.as_str()
                        };
                        if line == delimiter {
                            break;
                        }
                    }
                }
            }
            '&' | '|' | ';' | '(' | ')' => {
                end_word(&mut segments, &mut word, &mut in_word);
                segments.push(Vec::new());
            }
            c if c.is_whitespace() => end_word(&mut segments, &mut word, &mut in_word),
            c => {
                in_word = true;
                word.push(c);
            }
        }
        i += 1;
    }
    end_word(&mut segments, &mut word, &mut in_word);
    segments.retain(|s| !s.is_empty());
    segments
}

/// The folders a shell line names, resolved against `cwd`: every `cd` target (later segments
/// run from there), every `-C` argument (`git -C`, `make -C`), and every word written as an
/// absolute or home path. Words holding `$`, a backtick or a glob are skipped: nothing reliable
/// names a folder there. A heuristic over the text, for the question SPEC R-PERM.11 asks before a
/// command that works in another repository; what it misses is asked about at the first
/// refusal, as before.
pub fn command_paths(command: &str, cwd: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let expand = |word: &str, base: &Path| -> Option<PathBuf> {
        if word.is_empty() || word.contains(['$', '`', '*', '?', '[']) {
            return None;
        }
        let path = if word == "~" {
            home?.to_path_buf()
        } else if let Some(rest) = word.strip_prefix("~/") {
            home?.join(rest)
        } else {
            base.join(word)
        };
        Some(path)
    };
    let mut paths = Vec::new();
    let mut here = cwd.to_path_buf();
    #[cfg(windows)]
    let segments = literal_segments(command);
    #[cfg(not(windows))]
    let segments = shell_segments(command);
    for segment in segments {
        let mut words = segment.iter().map(String::as_str).peekable();
        let mut first = true;
        while let Some(word) = words.next() {
            if first && word == "cd" {
                if let Some(target) = words.next().and_then(|t| expand(t, &here)) {
                    here = target.clone();
                    paths.push(target);
                }
                break;
            }
            first = false;
            if word == "-C" {
                if let Some(target) = words.next().and_then(|t| expand(t, &here)) {
                    paths.push(target);
                }
            } else if (word.starts_with('/')
                || Path::new(word).is_absolute()
                || word == "~"
                || word.starts_with("~/"))
                && let Some(p) = expand(word, &here)
            {
                paths.push(p);
            }
        }
    }
    paths
}

/// The words of each command a line runs, split at unquoted `&`, `|`, `;` and newlines, with
/// quotes removed and a backslash kept as written: a Windows path (`cd C:\Users\me\repo`) is
/// one word, where [`shell_segments`]' POSIX escapes would read `C:Usersmerepo`. Used for the
/// folders a command names on Windows ([`command_paths`]).
#[cfg_attr(not(windows), allow(dead_code))]
fn literal_segments(command: &str) -> Vec<Vec<String>> {
    let mut segments: Vec<Vec<String>> = vec![Vec::new()];
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for c in command.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '&' | '|' | ';' | '\n') => {
                if in_word {
                    segments
                        .last_mut()
                        .expect("one segment")
                        .push(std::mem::take(&mut word));
                    in_word = false;
                }
                segments.push(Vec::new());
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    segments
                        .last_mut()
                        .expect("one segment")
                        .push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            (None, c) => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        segments.last_mut().expect("one segment").push(word);
    }
    segments.retain(|s| !s.is_empty());
    segments
}

/// The digest a command is recorded and matched under: of the command with
/// surrounding whitespace trimmed, so a re-run that differs only by a
/// trailing newline is still "the same command" (SPEC R-PERM.10).
pub fn command_digest(command: &str) -> String {
    crate::digest::sha256_hex(command.trim().as_bytes())
}

/// Whether `command` can use the SSH agent: `ssh`, `scp`, `sftp`, `rsync`, or
/// a `git` subcommand that reaches a remote or signs (a commit, tag or merge
/// signed with an SSH key). Leading `VAR=value` assignments, a path prefix on
/// the program and git's global options are skipped; anything after a shell
/// operator is not looked at.
pub fn may_use_ssh_agent(command: &str) -> bool {
    let mut words = command
        .split_whitespace()
        .skip_while(|w| w.contains('=') && !w.starts_with('-'));
    let Some(program) = words.next() else {
        return false;
    };
    let program = Path::new(program)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or(program);
    match program {
        "ssh" | "scp" | "sftp" | "rsync" => true,
        "git" => {
            // Global options that take their value as the next word.
            const WITH_VALUE: &[&str] = &[
                "-C",
                "-c",
                "--git-dir",
                "--work-tree",
                "--namespace",
                "--config-env",
            ];
            let mut subcommand = None;
            while let Some(w) = words.next() {
                if WITH_VALUE.contains(&w) {
                    words.next();
                } else if !w.starts_with('-') {
                    subcommand = Some(w);
                    break;
                }
            }
            matches!(
                subcommand,
                Some(
                    "fetch"
                        | "pull"
                        | "push"
                        | "clone"
                        | "ls-remote"
                        | "submodule"
                        | "remote"
                        | "archive"
                        | "lfs"
                        | "commit"
                        | "tag"
                        | "merge"
                        | "rebase"
                        | "cherry-pick"
                        | "revert"
                        | "am"
                )
            )
        }
        _ => false,
    }
}

/// Whether `refusal` is one to ask about before `command` runs: the same
/// command run again, or one that runs a program the refused command ran
/// (SPEC R-PERM.10.1). An agent rarely re-runs a command byte for byte — it
/// adds a flag, a pipe, a `cd` — so asking only on an identical re-run meant
/// the question never came. A refusal recorded without a digest (an older
/// ahma) matches any command.
pub fn refused_for(refusal: &Refusal, command: &str) -> bool {
    let Some(digest) = refusal.command_digest.as_deref() else {
        return true;
    };
    digest == command_digest(command)
        || command_programs(command)
            .iter()
            .any(|p| refusal.programs.contains(p))
}

/// The directory to offer for a refusal under `dir`: the widest ancestor that
/// is still one tool's own folder. Climbs while the parent passes
/// [`widening_is_safe`], so it stops below home, a folder many tools share,
/// a volume root, a credential folder and a folder of projects. A refused
/// `/Volumes/T7/caches/swiftpm/manifests/ManifestLoading` offers
/// `/Volumes/T7/caches/swiftpm`: offering the deepest directory meant the next
/// file elsewhere in the same cache was refused again, and asked again.
pub fn tool_root(dir: &Path, home: Option<&Path>, scopes: &[PathBuf]) -> PathBuf {
    if let Some(folder) = home
        .and_then(|home| folder_under_shared_root(dir, home))
        .filter(|folder| widening_is_safe(folder, home, scopes))
    {
        return folder;
    }
    let mut root = dir.to_path_buf();
    while let Some(parent) = root.parent() {
        if !widening_is_safe(parent, home, scopes) {
            break;
        }
        root = parent.to_path_buf();
    }
    root
}

/// The direct child of the deepest folder in [`SHARED_ROOTS`] that holds
/// `dir`: by convention one tool's own folder (`~/Library/Caches/<bundle id>`,
/// `~/.cache/<tool>`). What lies beneath it is the tool's business — Apple's
/// `python3` mirrors a script's whole path there — so its names say nothing
/// about who else shares it. `None` outside those folders, or for one itself.
fn folder_under_shared_root(dir: &Path, home: &Path) -> Option<PathBuf> {
    let shared = SHARED_ROOTS
        .iter()
        .map(|parts| parts.iter().fold(home.to_path_buf(), |p, c| p.join(c)))
        .filter(|root| dir.starts_with(root))
        .max_by_key(|root| root.components().count())?;
    let child = dir.strip_prefix(&shared).ok()?.components().next()?;
    Some(shared.join(child))
}

/// Folder names that hold many tools' data by convention, wherever they are.
const SHARED_FOLDER_NAMES: &[&str] = &[
    "cache",
    "caches",
    ".cache",
    "Caches",
    "tmp",
    "temp",
    "Temp",
    "TemporaryItems",
];

/// Whether `dir` is a system temp directory or an ancestor of one: shared by
/// every program on the machine, like a folder in [`SHARED_ROOTS`].
fn is_system_temp(dir: &Path) -> bool {
    let tmpdir = std::env::temp_dir();
    let tmpdir = dunce::canonicalize(&tmpdir).unwrap_or(tmpdir);
    [
        tmpdir.as_path(),
        Path::new("/tmp"),
        Path::new("/private/tmp"),
        Path::new("/var/tmp"),
        Path::new("/private/var/tmp"),
    ]
    .iter()
    .any(|t| t.starts_with(dir))
}

/// Whether `dir` is the root of a mounted volume: `/Volumes/<name>` (macOS),
/// `/mnt/<name>`, `/media/<user>/<name>` or `/run/media/<user>/<name>`.
pub fn is_volume_root(dir: &Path) -> bool {
    let parts: Vec<&str> = dir
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(p) => p.to_str(),
            _ => None,
        })
        .collect();
    matches!(
        parts.as_slice(),
        ["Volumes", _] | ["mnt", _] | ["media", _, _] | ["run", "media", _, _]
    )
}

/// Whether an SSH signature refused for `recorded` is one to ask about before
/// `command` runs: the same command again, or any command that can use the
/// agent ([`may_use_ssh_agent`]). The second is what makes `git fetch` ask
/// after `git fetch origin` was refused, and what lets a refusal recorded
/// without a command (an MCP `run_terminal_command`) be asked about at all —
/// without ever holding an unrelated command such as `cargo build` or
/// `git status`.
pub fn refused_for_ssh(recorded: Option<&str>, command: &str) -> bool {
    recorded.is_some_and(|d| d == command_digest(command)) || may_use_ssh_agent(command)
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
    /// The programs whose commands were refused, so the dialog can say who
    /// needs it (SPEC R-PERM.10.2). Empty for records of an older ahma.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub programs: Vec<String>,
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

/// A command the kernel refused something no grant or setting allows (SPEC
/// R-ESCAPE.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EscapeRefusal {
    /// Digest of the exact command: only that command is ever offered.
    pub command_digest: String,
    /// What the kernel refused, for the dialog.
    pub why: String,
    pub at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_pid: Option<u32>,
}

/// An escape question put to the human, with its one-use token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskedEscape {
    pub session_id: String,
    pub refusal: EscapeRefusal,
    pub token: String,
    pub asked_at: u64,
    #[serde(default)]
    pub used: bool,
}

/// Remember that `refusal.command_digest` was refused what nothing allows.
pub fn record_escape(dir: &Path, workspace: &Path, refusal: EscapeRefusal) -> Result<()> {
    update(dir, workspace, refusal.at, |asks| {
        asks.escapes
            .retain(|r| r.command_digest != refusal.command_digest);
        asks.escapes.push(refusal);
    })
}

/// The escape to ask about before `command` runs in `session_id`: one
/// recorded for exactly this command, not yet asked in this session.
pub fn next_escape_question(
    asks: &WorkspaceAsks,
    session_id: &str,
    command: &str,
) -> Option<EscapeRefusal> {
    let digest = command_digest(command);
    asks.escapes
        .iter()
        .find(|r| {
            r.command_digest == digest
                && !asks
                    .escapes_asked
                    .iter()
                    .any(|a| a.session_id == session_id && a.refusal.command_digest == digest)
        })
        .cloned()
}

/// Record that the escape was asked, returning the one-use token.
pub fn mark_escape_asked(
    dir: &Path,
    workspace: &Path,
    session_id: &str,
    refusal: &EscapeRefusal,
    now: u64,
) -> Result<String> {
    let token = uuid::Uuid::new_v4().simple().to_string();
    let asked = AskedEscape {
        session_id: session_id.to_string(),
        refusal: refusal.clone(),
        token: token.clone(),
        asked_at: now,
        used: false,
    };
    update(dir, workspace, now, |asks| asks.escapes_asked.push(asked))?;
    Ok(token)
}

/// Spend an escape `token` for exactly `command`: once, within
/// [`TOKEN_TTL_SECS`] of the question. The refusal is forgotten, so the next
/// refusal of the same command is asked about again.
pub fn take_escape_approved(
    dir: &Path,
    workspace: &Path,
    token: &str,
    command: &str,
    now: u64,
) -> Option<AskedEscape> {
    let digest = command_digest(command);
    update(dir, workspace, now, |asks| {
        let asked = asks.escapes_asked.iter_mut().find(|a| {
            a.token == token
                && !a.used
                && a.refusal.command_digest == digest
                && now.saturating_sub(a.asked_at) <= TOKEN_TTL_SECS
        })?;
        asked.used = true;
        let asked = asked.clone();
        asks.escapes.retain(|r| r.command_digest != digest);
        Some(asked)
    })
    .ok()
    .flatten()
}

/// A question put to the human before a command that works in another repository (SPEC
/// R-PERM.11), with the one-use token the approved command carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskedRepo {
    pub session_id: String,
    /// The other repository's root.
    pub repo: PathBuf,
    pub token: String,
    pub command_digest: String,
    pub asked_at: u64,
    #[serde(default)]
    pub used: bool,
}

/// A human's yes to working in another repository from this workspace, for one harness session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoAck {
    pub session_id: String,
    pub repo: PathBuf,
    pub at: u64,
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
    /// Commands the kernel refused something no grant or setting allows
    /// (SPEC R-ESCAPE): asked about, when run again, as one run outside the
    /// sandbox.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub escapes: Vec<EscapeRefusal>,
    /// Those questions, put to the human through the harness dialog.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub escapes_asked: Vec<AskedEscape>,
    /// Questions about working in another repository (SPEC R-PERM.11).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos_asked: Vec<AskedRepo>,
    /// The yeses to them, per harness session.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos_acknowledged: Vec<RepoAck>,
}

/// Whether the human said yes, in `session_id`, to this workspace working in `repo`.
pub fn repo_acknowledged(asks: &WorkspaceAsks, session_id: &str, repo: &Path) -> bool {
    asks.repos_acknowledged
        .iter()
        .any(|a| a.session_id == session_id && a.repo == repo)
}

/// Record that working in `repo` was asked about before `command` in `session_id`, returning the
/// one-use token the approved command carries. Every command in `repo` is asked about until one
/// is approved: a no stops that command, and the next one asks again.
pub fn mark_repo_asked(
    dir: &Path,
    workspace: &Path,
    session_id: &str,
    repo: &Path,
    command: &str,
    now: u64,
) -> Result<String> {
    let token = uuid::Uuid::new_v4().simple().to_string();
    let asked = AskedRepo {
        session_id: session_id.to_string(),
        repo: repo.to_path_buf(),
        token: token.clone(),
        command_digest: command_digest(command),
        asked_at: now,
        used: false,
    };
    update(dir, workspace, now, |asks| asks.repos_asked.push(asked))?;
    Ok(token)
}

/// Spend a repository `token` for exactly `command`, as [`take_approved`] does for a path, and
/// remember the yes for its session.
pub fn take_repo_approved(
    dir: &Path,
    workspace: &Path,
    token: &str,
    command: &str,
    now: u64,
) -> Option<AskedRepo> {
    let digest = command_digest(command);
    update(dir, workspace, now, |asks| {
        let asked = asks.repos_asked.iter_mut().find(|a| {
            a.token == token
                && !a.used
                && a.command_digest == digest
                && now.saturating_sub(a.asked_at) <= TOKEN_TTL_SECS
        })?;
        asked.used = true;
        let asked = asked.clone();
        asks.repos_acknowledged.push(RepoAck {
            session_id: asked.session_id.clone(),
            repo: asked.repo.clone(),
            at: now,
        });
        Some(asked)
    })
    .ok()
    .flatten()
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
    if normal < 2 || is_volume_root(dir) || is_system_temp(dir) {
        return false;
    }
    if dir
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| SHARED_FOLDER_NAMES.contains(&n))
    {
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
            programs: r.programs.clone(),
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
        for program in absorbed.programs {
            if !into.programs.contains(&program) {
                into.programs.push(program);
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
    asks.escapes
        .retain(|r| now.saturating_sub(r.at) <= MAX_AGE_SECS);
    asks.escapes_asked
        .retain(|a| now.saturating_sub(a.asked_at) <= MAX_AGE_SECS);
    asks.repos_asked
        .retain(|a| now.saturating_sub(a.asked_at) <= MAX_AGE_SECS);
    asks.repos_acknowledged
        .retain(|a| now.saturating_sub(a.at) <= MAX_AGE_SECS);
}

/// Remember that the SSH key broker refused a hooked command in `workspace`.
pub fn record_ssh_refusal(dir: &Path, workspace: &Path, refusal: SshRefusal) -> Result<()> {
    update(dir, workspace, refusal.at, |asks| {
        asks.ssh_refusals.retain(|r| !r.same_subject(&refusal));
        asks.ssh_refusals.push(refusal);
    })
}

/// The SSH signature to ask about before `command` runs in harness session
/// `session_id`: one refused when `command` last ran ([`refused_for_ssh`]), that
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
        .filter(|r| refused_for_ssh(r.command_digest.as_deref(), command) && !covered(r))
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
        command_digest: command_digest(command),
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
    let digest = command_digest(command);
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
    // Both sides canonical: a refusal recorded through a symlink (macOS
    // `/tmp` is `/private/tmp`) must meet the grant of its real folder.
    let canonical = crate::scope_grant::canonicalize_best_effort;
    let path = canonical(path);
    let workspace = canonical(workspace);
    granted.iter().any(|g| {
        let applies = g
            .workspace
            .as_deref()
            .is_none_or(|w| workspace.starts_with(canonical(w)));
        let root = canonical(&crate::config::expand_home(&g.path));
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

/// Choose the next question to put to the human before `command` runs in
/// `session_id`, and record it as asked, in **one** locked step; returns it
/// with the one-use token the approved command will carry.
///
/// [`next_question`] then [`mark_asked`] were two steps with the lock taken
/// only for the second, so parallel hooks of one session (Claude Code runs
/// subagents side by side) could all see "not asked yet" and each raise a
/// dialog for the same question (SPEC R-PERM.3.8). Under the lock, the first
/// marks it and every other sees it asked.
///
/// `consider` picks the refusals that apply to this command; the others stay
/// on record for the commands they do apply to. The grant a yes gives must be
/// bounded by a harness process: `harness_pid`, else one a refusal recorded;
/// with neither, nothing is asked.
#[allow(clippy::too_many_arguments)]
pub fn ask_next(
    dir: &Path,
    workspace: &Path,
    session_id: &str,
    consider: &dyn Fn(&Refusal) -> bool,
    covered: &dyn Fn(&Path, ScopeAccess) -> bool,
    home: Option<&Path>,
    scopes: &[PathBuf],
    command: &str,
    harness_pid: Option<u32>,
    now: u64,
) -> Result<Option<(Question, String)>> {
    update(dir, workspace, now, |asks| {
        let mut view = asks.clone();
        view.refusals.retain(|r| consider(r));
        let question = next_question(&view, session_id, covered, home, scopes)?;
        let pid = harness_pid.or_else(|| view.refusals.iter().find_map(|r| r.harness_pid))?;
        let token = uuid::Uuid::new_v4().simple().to_string();
        asks.asked.push(Asked {
            session_id: session_id.to_string(),
            question: question.clone(),
            token: token.clone(),
            command_digest: command_digest(command),
            asked_at: now,
            harness_pid: Some(pid),
            used: false,
        });
        Some((question, token))
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
        command_digest: command_digest(command),
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
    let digest = command_digest(command);
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

    /// SPEC R-PERM.11: the folders a line names, for the question before a command that works in
    /// another repository. The incident's first command was `cd /…/neubit4 && git fetch …`.
    /// PR #259's Windows leg: `cd C:\Users\…\neubit4` named no folder, because the POSIX
    /// splitter read each backslash as an escape.
    #[test]
    fn a_windows_path_stays_one_word() {
        assert_eq!(
            literal_segments(r#"cd C:\Users\me\neubit4 && git -C "C:\a b\x" status"#),
            vec![
                vec!["cd".to_string(), r"C:\Users\me\neubit4".to_string()],
                vec![
                    "git".to_string(),
                    "-C".to_string(),
                    r"C:\a b\x".to_string(),
                    "status".to_string()
                ]
            ]
        );
    }

    #[test]
    fn the_folders_a_command_names_are_found() {
        let cwd = Path::new("/w/ahma");
        let home = Some(Path::new("/Users/me"));
        let p = |c: &str| command_paths(c, cwd, home);
        assert_eq!(
            p("cd /w/neubit4 && git fetch -q origin main && ls rust"),
            [PathBuf::from("/w/neubit4")]
        );
        assert_eq!(
            p("cd ../neubit4; ls"),
            [PathBuf::from("/w/ahma/../neubit4")]
        );
        assert_eq!(p("git -C /w/neubit4 status"), [PathBuf::from("/w/neubit4")]);
        assert_eq!(
            p("cat ~/notes/a.md '/w/other repo/x'"),
            [
                PathBuf::from("/Users/me/notes/a.md"),
                PathBuf::from("/w/other repo/x")
            ]
        );
        assert!(p("cargo build && ls src").is_empty());
        assert!(p("ls $HOME/x /w/*/y `pwd`").is_empty());
        assert!(
            p("cat <<EOF\n/w/neubit4\nEOF").is_empty(),
            "a heredoc body names nothing"
        );
    }

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

    /// SPEC R-ESCAPE: an escape is offered for exactly the refused command,
    /// once per harness session, and its token runs that command once.
    #[test]
    fn an_escape_is_for_one_command_once() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Path::new("/ws/repo");
        let cmd = "open -a Simulator";
        record_escape(
            dir.path(),
            ws,
            EscapeRefusal {
                command_digest: crate::digest::sha256_hex(cmd.as_bytes()),
                why: "lsopen".into(),
                at: 100,
                harness_pid: Some(7),
            },
        )
        .unwrap();
        let asks = load(dir.path(), ws, 101);
        assert!(
            next_escape_question(&asks, "s1", "open -a Xcode").is_none(),
            "another command"
        );
        let q = next_escape_question(&asks, "s1", cmd).expect("this command");
        let token = mark_escape_asked(dir.path(), ws, "s1", &q, 102).unwrap();
        let asks = load(dir.path(), ws, 103);
        assert!(
            next_escape_question(&asks, "s1", cmd).is_none(),
            "once per session"
        );
        assert!(take_escape_approved(dir.path(), ws, &token, "open -a Xcode", 104).is_none());
        assert!(take_escape_approved(dir.path(), ws, &token, cmd, 104).is_some());
        assert!(
            take_escape_approved(dir.path(), ws, &token, cmd, 105).is_none(),
            "once"
        );
        assert!(load(dir.path(), ws, 106).escapes.is_empty());
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
            programs: Vec::new(),
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

    /// A refusal recorded through a symlink (macOS `/tmp` is
    /// `/private/tmp`) is covered by a grant of the same folder by its real
    /// path: both sides are compared canonical, or a covered question keeps
    /// coming back. Unix only: it needs a symlink without privileges.
    #[cfg(unix)]
    #[test]
    fn a_grant_covers_a_refusal_recorded_through_a_symlink() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let ws = root.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let granted = [PersistentScope {
            path: dunce::canonicalize(&real).unwrap(),
            access: ScopeAccess::Rw,
            workspace: Some(link.join("..").join("ws")),
            granted_by: None,
            granted_at: None,
            note: None,
            expires_at: None,
        }];
        assert!(grant_covers(
            &granted,
            &ws,
            &link.join("sub/f.lock"),
            ScopeAccess::Rw
        ));
    }

    /// SPEC R-PERM.3.8, field case: three Plan subagents of one Claude Code
    /// session hit the same refused folder at once. Each hook checked "not
    /// asked yet" and then marked it asked, in two separate steps, so all
    /// three could pass the check and three dialogs appeared for one
    /// question. Choosing and marking is one locked step: exactly one wins.
    #[test]
    fn parallel_hooks_of_one_session_raise_one_dialog() {
        let (root, dir, ws) = store();
        let home = root.path().join("home");
        let cache = home.join(".cache/neubit");
        std::fs::create_dir_all(&cache).unwrap();
        let mut r = refusal(&cache.join("heavy.lock.holder"), ScopeAccess::Rw);
        r.harness_pid = Some(4242);
        record_refusal(&dir, &ws, r).unwrap();

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (dir, ws, home, barrier) =
                    (dir.clone(), ws.clone(), home.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    ask_next(
                        &dir,
                        &ws,
                        "s1",
                        &|_: &Refusal| true,
                        &|_: &Path, _: ScopeAccess| false,
                        Some(&home),
                        std::slice::from_ref(&ws),
                        "make heavy",
                        Some(4242),
                        200,
                    )
                    .unwrap()
                })
            })
            .collect();
        let asked: Vec<_> = handles
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .collect();
        assert_eq!(asked.len(), 1, "one dialog for one question: {asked:?}");
        assert_eq!(asked[0].0.dir, cache);
    }

    /// A refusal the caller filters out is not asked, and is not dropped
    /// from the record either: another command may still need it.
    #[test]
    fn ask_next_filters_without_forgetting() {
        let (root, dir, ws) = store();
        let home = root.path().join("home");
        let cache = home.join(".cache/neubit");
        std::fs::create_dir_all(&cache).unwrap();
        let mut r = refusal(&cache.join("x.lock"), ScopeAccess::Rw);
        r.harness_pid = Some(1);
        record_refusal(&dir, &ws, r).unwrap();
        let none = |_: &Path, _: ScopeAccess| false;
        let skip_all = |_: &Refusal| false;
        let scopes = std::slice::from_ref(&ws);
        let asked = ask_next(
            &dir,
            &ws,
            "s1",
            &skip_all,
            &none,
            Some(&home),
            scopes,
            "c",
            Some(1),
            200,
        );
        assert!(asked.unwrap().is_none());
        assert_eq!(load(&dir, &ws, 201).refusals.len(), 1);
        let take_all = |_: &Refusal| true;
        let asked = ask_next(
            &dir,
            &ws,
            "s1",
            &take_all,
            &none,
            Some(&home),
            scopes,
            "c",
            None,
            200,
        );
        assert!(
            asked.unwrap().is_some(),
            "the refusal's own pid bounds the grant"
        );
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

    #[test]
    fn a_git_ssh_refusal_matches_git_retries_and_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Path::new("/ws/repo");
        let refusal = SshRefusal {
            key: "SHA256:key".into(),
            key_comment: "me@laptop".into(),
            destination: "host:SHA256:host".into(),
            label: "github.com".into(),
            at: 100,
            harness_pid: Some(7),
            command_digest: Some(crate::digest::sha256_hex(b"git fetch origin")),
        };
        record_ssh_refusal(dir.path(), ws, refusal).unwrap();
        let asks = load(dir.path(), ws, 100);

        // An unrelated command does not match, nor does git that stays local.
        assert!(next_ssh_question(&asks, "s1", "cargo build", &|_| false).is_none());
        assert!(next_ssh_question(&asks, "s1", "git status", &|_| false).is_none());
        assert!(next_ssh_question(&asks, "s1", "git log --oneline", &|_| false).is_none());

        // A retry with git fetch (without origin) or with flags/whitespace matches
        assert!(next_ssh_question(&asks, "s1", "git fetch", &|_| false).is_some());
        assert!(next_ssh_question(&asks, "s1", "git fetch origin\n", &|_| false).is_some());
        assert!(next_ssh_question(&asks, "s1", "git pull", &|_| false).is_some());

        // Approving with git fetch spends the token
        let q = next_ssh_question(&asks, "s1", "git fetch", &|_| false).unwrap();
        let token = mark_ssh_asked(dir.path(), ws, "s1", &q, "git fetch", Some(7), 100).unwrap();
        assert!(take_ssh_approved(dir.path(), ws, &token, "git fetch", 100).is_some());
    }

    #[test]
    fn only_commands_that_can_use_the_agent_may_use_ssh_agent() {
        for yes in [
            "git fetch",
            "git -C ../other push origin main",
            "GIT_TRACE=1 git pull",
            "/usr/bin/git -c commit.gpgsign=true commit -m x",
            "git --git-dir .git tag -s v1",
            "ssh git@github.com",
            "scp a host:b",
            "rsync -a x host:y",
        ] {
            assert!(may_use_ssh_agent(yes), "{yes}");
        }
        for no in [
            "",
            "git",
            "git status",
            "git -C fetch status",
            "git diff --stat",
            "cargo build",
            "echo git fetch",
        ] {
            assert!(!may_use_ssh_agent(no), "{no}");
        }
    }

    #[test]
    fn a_command_is_the_same_command_whatever_its_surrounding_whitespace() {
        assert_eq!(
            command_digest("  make build\n"),
            command_digest("make build")
        );
        assert_ne!(command_digest("make build"), command_digest("make  build"));
        let mut refused = refusal(Path::new("/w/x"), ScopeAccess::Rw);
        refused.command_digest = Some(command_digest("make build"));
        assert!(refused_for(&refused, "make build\n"));
    }

    /// SPEC R-PERM.10.1: the programs of a command line, past `cd`, `env`,
    /// assignments and paths.
    #[test]
    fn the_programs_of_a_command_line() {
        assert_eq!(
            command_programs("cd ios/Neubit && xcodebuild -scheme A build 2>&1 | tail -5"),
            ["xcodebuild"]
        );
        assert_eq!(
            command_programs("FOO=1 env BAR=2 ./scripts/audition ask r1; echo done"),
            ["audition"]
        );
        assert_eq!(command_programs("time cargo build && ls"), ["cargo"]);
        assert!(command_programs("cd x && ls && echo hi").is_empty());
    }

    /// SPEC R-PERM.10.1 (field case): a command that only *mentions* a
    /// program — in a heredoc body, a string — does not run it. A script
    /// written with `cat > render.sh <<'EOF'` whose body named Chrome's path
    /// was taken to run the program `Google`, and raised the Chrome question
    /// on an unrelated command.
    #[test]
    fn a_program_named_in_a_heredoc_or_a_string_is_not_run() {
        let incident = "S=/tmp/s && cat > $S/render.sh <<'EOF'\n#!/bin/sh\n\
                        CH=\"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome\"\n\
                        \"$CH\" --headless=new --print-to-pdf=\"$OUT\"\nopen -R \"$OUT\"\nEOF\n\
                        chmod +x $S/render.sh && echo ok";
        assert_eq!(command_programs(incident), ["chmod"]);
        // A quoted path is one word; a variable is no program we can name.
        assert_eq!(
            command_programs(
                "\"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome\" --headless 2>&1 | tail -5"
            ),
            ["Google Chrome"]
        );
        assert!(command_programs("CH=x && \"$CH\" --headless").is_empty());
        assert_eq!(command_programs("echo 'a; rm -rf x' && make"), ["make"]);
        assert_eq!(
            command_programs("git log --format='%h | %s' ; cargo fmt"),
            ["git", "cargo"]
        );
        // `<<-` strips leading tabs from the closing word.
        assert_eq!(
            command_programs("cat <<-END\n\tnot-a-program\n\tEND\nnpm test"),
            ["npm"]
        );
        // A comment runs nothing.
        assert_eq!(
            command_programs("# xcodebuild later\nswift build"),
            ["swift"]
        );
    }

    /// SPEC R-PERM.10.1 (field case): an agent changes the command before it
    /// runs it again — a new flag, a `cd`, a pipe — so the question waited
    /// for a byte-identical re-run that never came. A command running the
    /// same program is asked about; an unrelated one, or one that only
    /// navigates, is not.
    #[test]
    fn a_refusal_is_asked_about_before_the_same_program_runs_again() {
        let refused_command = "cd ios/Neubit && xcodebuild -scheme Neubit build";
        let mut refused = refusal(Path::new("/w/x"), ScopeAccess::Rw);
        refused.command_digest = Some(command_digest(refused_command));
        refused.programs = command_programs(refused_command);
        assert!(refused_for(&refused, refused_command));
        assert!(refused_for(
            &refused,
            "xcodebuild -scheme Neubit -derivedDataPath build/dd build | tail"
        ));
        assert!(!refused_for(&refused, "cargo build"));
        assert!(!refused_for(&refused, "cd ios/Neubit && ls"));
    }

    /// SPEC R-PERM.10.1: the directory offered is the tool's own folder —
    /// below home, a shared folder, a folder named like a cache, a volume
    /// root — not the deepest refused one.
    #[test]
    fn the_directory_offered_is_the_tools_own_folder() {
        let home = Path::new("/Users/me");
        let root = |p: &str| tool_root(Path::new(p), Some(home), &[]);
        assert_eq!(
            root("/Volumes/T7/caches/swiftpm/manifests/ManifestLoading"),
            Path::new("/Volumes/T7/caches/swiftpm")
        );
        assert_eq!(
            root("/Volumes/T7/caches/DerivedData/Neubit-ab/SourcePackages/artifacts"),
            Path::new("/Volumes/T7/caches/DerivedData")
        );
        assert_eq!(
            root("/Users/me/Library/Developer/Xcode/DerivedData/Neubit-ab/Logs"),
            Path::new("/Users/me/Library/Developer/Xcode/DerivedData")
        );
        assert_eq!(
            root("/Users/me/.cache/neubit/blobs/aa"),
            Path::new("/Users/me/.cache/neubit")
        );
        assert_eq!(root("/Volumes/T7/x"), Path::new("/Volumes/T7/x"));
        let tmp = dunce::canonicalize(std::env::temp_dir()).unwrap();
        let in_tmp = tmp.join("tool-cache").join("blobs");
        assert_eq!(
            tool_root(&in_tmp, Some(home), &[]),
            tmp.join("tool-cache"),
            "never the system temp directory itself"
        );
        assert!(is_volume_root(Path::new("/Volumes/T7")));
        assert!(!is_volume_root(Path::new("/Volumes/T7/caches")));
    }

    /// SPEC R-PERM.10.1: under a per-user shared folder the tool's folder is
    /// the shared folder's direct child, whatever lies beneath it. Apple's
    /// `python3` mirrors a script's whole path into its bytecode cache, so a
    /// script in `/private/tmp` put a `tmp` inside the tool's folder and the
    /// question named `…/com.apple.python/private/tmp/claude-501`.
    #[test]
    fn a_mirrored_path_inside_a_tool_cache_offers_the_tool_folder() {
        let home = Path::new("/Users/me");
        let root = |p: &str| tool_root(Path::new(p), Some(home), &[]);
        assert_eq!(
            root("/Users/me/Library/Caches/com.apple.python/private/tmp/claude-501/x/__pycache__"),
            Path::new("/Users/me/Library/Caches/com.apple.python")
        );
        assert_eq!(
            root("/Users/me/.cache/tool/tmp/cache/a"),
            Path::new("/Users/me/.cache/tool")
        );
        assert_eq!(
            root("/Users/me/Library/Caches"),
            Path::new("/Users/me/Library/Caches"),
            "a shared folder itself is left to the climb, which never widens it"
        );
    }
}
