//! # Workspace write queue (SPEC R2.7)
//!
//! Ahma's answer to "async is fast, but two mutating commands in one workspace
//! must never overlap". Every operation that may write the workspace (the
//! *exclusive* lane, see [`Lane`]) takes a **lease** on its workspace before it
//! spawns, and holds it until the process tree exits. Leases are granted in the
//! order the calls arrived — a FIFO, not a free-for-all — so the side effects of
//! ahma commands land in exactly the order the model issued them, while the
//! model itself is free to think, read and plan during a long `cargo nextest`.
//!
//! ## Two layers, one rule
//!
//! 1. **In-process FIFO.** A ticket is issued synchronously when the call
//!    arrives ([`WorkspaceQueue::enqueue`]), so arrival order — not tokio's task
//!    scheduling — decides who runs next. The lanes are process-global: every
//!    `Adapter` in a process (tests build many; a hook builds its own) shares them.
//! 2. **Cross-process kernel lock.** The head of the in-process queue then takes
//!    an advisory [`FsLock`] (`flock` / `LockFileEx`) on a rendezvous file in the
//!    per-user runtime directory. That is what serialises two Claude Code
//!    sessions, the TUI and a terminal hook on the same workspace.
//!
//! ## No stale locks, by construction
//!
//! The lock state lives in the kernel, attached to an open file description.
//! It is released when the holder exits for *any* reason — panic, `SIGKILL`,
//! crash, power loss. There is no lock file whose mere existence means "held"
//! (the `index.lock` failure mode) and nothing ever needs manual cleanup. The
//! rendezvous file lives **outside every sandbox scope**, so a command the agent
//! runs cannot delete it while it is held and let a second holder in.
//!
//! ## Keyed by workspace, not by machine
//!
//! The key is the repository root that contains the working directory (the
//! nearest ancestor holding `.git`, file or directory — so each git worktree is
//! its own workspace and worktrees are the unit of parallelism), falling back to
//! the sandbox scope root, then the directory itself ([`workspace_key`]).
//! Unrelated repositories never wait for each other.
//!
//! ## Re-entrancy
//!
//! A command running under a lease may itself start ahma (dogfooding: `cargo
//! nextest` spawns `ahma` binaries). A child that waited for the lease its own
//! ancestor holds would wait forever, so the holder stamps its children with
//! [`HELD_LEASE_ENV`]; a process that inherits the stamp for a key is already
//! inside that lease and skips the cross-process lock for it.

use ahma_common::fs_lock::FsLock;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Environment variable naming the workspace lease(s) the current process tree
/// already runs under. Internal supervision marker, like `AHMA_SERVER_CHILD` —
/// not configuration (SPEC R-CFG): it only ever *skips a wait* for a lease an
/// ancestor provably holds. Never inherited by the detached hub.
pub const HELD_LEASE_ENV: &str = ahma_common::process_guard::HELD_WORKSPACE_LEASE_ENV;

/// How an operation participates in the workspace queue.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    /// May write the workspace: takes the lease, runs in arrival order.
    #[default]
    Exclusive,
    /// Cannot write the workspace — the kernel sandbox is told so at spawn
    /// (SPEC R2.7.4). Needs no lease and never waits.
    ReadOnly,
    /// Long-lived by design (a dev server, a log follower): holding the lease
    /// would block every later command for its whole life, so it takes none.
    Service,
}

impl Lane {
    pub fn as_str(self) -> &'static str {
        match self {
            Lane::Exclusive => "exclusive",
            Lane::ReadOnly => "read_only",
            Lane::Service => "service",
        }
    }
}

/// What a command does to the workspace's source files, for the edit guard
/// (SPEC R2.7.8). Published with the holder so a hook in another process can
/// decide without re-parsing the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SourceEffect {
    /// Not known: treated as a writer, exactly as every holder was before
    /// this field existed. A record from an older ahma decodes as this.
    #[default]
    Unknown,
    /// Builds, tests, linters: they read sources and write only their own
    /// outputs. An edit made while one runs is allowed; the drift report
    /// (R2.7.6) says the run may have seen it.
    ReadsSources,
    /// Formatters, codemods, checkouts, package managers: an edit racing one
    /// is overwritten or lost, so it is refused.
    RewritesSources,
}

/// Who holds (or is waiting for) a workspace lease — enough to name it in a
/// message a model can act on ("cancel op X").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderInfo {
    pub op_id: String,
    /// One-line human title (the command).
    pub title: String,
    /// Process id of the ahma process holding the lease.
    pub pid: u32,
    /// Seconds since the Unix epoch when the lease was taken (or the call queued).
    pub since_unix: u64,
    /// What the command does to source files (add-only, R24.5).
    #[serde(default)]
    pub effect: SourceEffect,
    /// The subtree the command runs in, when narrower than the workspace: an
    /// edit outside it cannot race it. `None` means the whole workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footprint: Option<PathBuf>,
    /// How long the same command usually runs, for "usually takes …".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typical_secs: Option<u64>,
}

impl HolderInfo {
    pub fn new(op_id: impl Into<String>, title: impl Into<String>) -> Self {
        let title = title.into();
        Self {
            op_id: op_id.into(),
            effect: super::lane::classify_source_effect(&title, &[]),
            title,
            pid: std::process::id(),
            since_unix: unix_now(),
            footprint: None,
            typical_secs: None,
        }
    }

    /// Bound the holder to the subtree it runs in.
    #[must_use]
    pub fn with_footprint(mut self, footprint: PathBuf) -> Self {
        self.footprint = Some(footprint);
        self
    }

    /// Say how long the same command usually runs.
    #[must_use]
    pub fn with_typical_secs(mut self, secs: Option<u64>) -> Self {
        self.typical_secs = secs;
        self
    }

    /// Override the classified effect (a project-declared source reader).
    #[must_use]
    pub fn with_effect(mut self, effect: SourceEffect) -> Self {
        self.effect = effect;
        self
    }

    /// Whether an edit of `path` must wait for this holder (SPEC R2.7.8): a
    /// source reader never blocks; anything else blocks inside its footprint.
    pub fn blocks_edit_of(&self, path: &Path) -> bool {
        match self.effect {
            SourceEffect::ReadsSources => false,
            SourceEffect::Unknown | SourceEffect::RewritesSources => self
                .footprint
                .as_deref()
                .is_none_or(|f| path.starts_with(f)),
        }
    }

    /// How long ago `since_unix` was, rendered for a model (`3m12s`).
    pub fn age(&self) -> String {
        let secs = unix_now().saturating_sub(self.since_unix);
        format_duration_secs(secs)
    }

    /// `op X (`title`, 3m12s)` — the canonical way every message names a holder.
    pub fn describe(&self) -> String {
        format!("op `{}` (`{}`, {})", self.op_id, self.title, self.age())
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `125` → `2m05s`; `7` → `7s`; `3725` → `1h02m05s`.
pub fn format_duration_secs(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// The workspace a working directory belongs to (SPEC R2.7.2).
///
/// The nearest ancestor of `working_dir` (itself included) that contains a
/// `.git` entry — a directory for a plain clone, a file for a worktree, so two
/// worktrees of one repository are two workspaces. Without one, the longest
/// sandbox scope containing the directory; without that, the directory itself.
/// Both sides are canonicalized so `/tmp` vs `/private/tmp` (macOS) or case
/// differences (Windows) cannot split one workspace into two keys.
pub fn workspace_key(working_dir: &Path, scopes: &[PathBuf]) -> PathBuf {
    let wd = dunce::canonicalize(working_dir).unwrap_or_else(|_| working_dir.to_path_buf());
    for ancestor in wd.ancestors() {
        if ancestor.join(".git").exists() {
            return ancestor.to_path_buf();
        }
    }
    scopes
        .iter()
        .map(|s| dunce::canonicalize(s).unwrap_or_else(|_| s.clone()))
        .filter(|s| wd.starts_with(s))
        .max_by_key(|s| s.components().count())
        .unwrap_or(wd)
}

/// Where the drift probe (SPEC R2.7.6) may walk for an operation keyed on
/// `key`: the key itself when a sandbox scope contains it, otherwise the
/// longest scope containing `working_dir`, otherwise `working_dir`. The lease
/// key may lie above every scope (a `.git` in `$HOME`); the walk runs in the
/// server process, so it must not list files the sandbox hides from the model.
pub fn drift_root(key: &Path, scopes: &[PathBuf], working_dir: &Path) -> PathBuf {
    let scopes: Vec<PathBuf> = scopes
        .iter()
        .map(|s| dunce::canonicalize(s).unwrap_or_else(|_| s.clone()))
        .collect();
    if scopes.iter().any(|s| key.starts_with(s)) {
        return key.to_path_buf();
    }
    let wd = dunce::canonicalize(working_dir).unwrap_or_else(|_| working_dir.to_path_buf());
    scopes
        .into_iter()
        .filter(|s| wd.starts_with(s))
        .max_by_key(|s| s.components().count())
        .unwrap_or(wd)
}

/// Stable 64-bit FNV-1a. Stable across Rust versions and builds, which
/// `DefaultHasher` is not — two ahma binaries of different versions must agree
/// on the rendezvous file name, or they would not serialise at all.
pub fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The hex id of a workspace key, used in rendezvous file names and in
/// [`HELD_LEASE_ENV`].
pub fn key_id(key: &Path) -> String {
    format!("{:016x}", stable_hash(key.to_string_lossy().as_bytes()))
}

/// The per-user directory holding every lease rendezvous file: `<runtime
/// dir>/locks`. Outside every sandbox scope by construction (R5.4.8 keeps
/// `~/.ahma` out; `$XDG_RUNTIME_DIR` is never a workspace).
pub fn default_lock_dir() -> Option<PathBuf> {
    ahma_common::hub::runtime_dir().map(|d| d.join("locks"))
}

/// Whether this process already runs inside an ancestor's lease on `key`.
fn inherited_lease(key: &Path) -> bool {
    let id = key_id(key);
    std::env::var(HELD_LEASE_ENV)
        .map(|v| v.split(',').any(|held| held == id))
        .unwrap_or(false)
}

/// The value to stamp on a child spawned under a lease on `key`: every lease
/// the current process already inherits, plus this one.
pub fn child_lease_env(key: &Path) -> String {
    let id = key_id(key);
    match std::env::var(HELD_LEASE_ENV) {
        Ok(existing) if !existing.is_empty() => {
            if existing.split(',').any(|h| h == id) {
                existing
            } else {
                format!("{existing},{id}")
            }
        }
        _ => id,
    }
}

/// Why an edit to `path` is refused while `holder` runs in `workspace` (SPEC
/// R2.7.8). One wording for ahma's own file tools and the harness edit hook, so
/// a model learns one rule.
pub fn edit_refusal(path: &Path, workspace: &Path, holder: Option<&HolderInfo>) -> String {
    let who = holder
        .map(HolderInfo::describe)
        .unwrap_or_else(|| "a command in another ahma session".to_string());
    let usually = holder
        .and_then(|h| h.typical_secs)
        .map(|s| format!(" It usually takes {}.", format_duration_secs(s)))
        .unwrap_or_default();
    format!(
        "Not edited: {} is in workspace {}, where {who} is running and may rewrite source \
         files. An edit now could be overwritten, or change what that command sees partway \
         through.{usually} `await` it (or `cancel` it), then make the edit. Builds and tests do \
         not block edits; if this command only reads sources (a test or build wrapper), a human \
         can declare it in `[tools] source_readers` and edits will proceed while it runs.",
        path.display(),
        workspace.display()
    )
}

/// The one-line form of [`edit_refusal`], for every refusal after the first
/// while the same command runs: the agent already has the full reason.
fn edit_refusal_again(path: &Path, holder: &HolderInfo) -> String {
    let usually = holder
        .typical_secs
        .map(|s| format!(", usually {}", format_duration_secs(s)))
        .unwrap_or_default();
    format!(
        "Not edited: {} — op `{}` is still running ({}{usually}); `await` or `cancel` it.",
        path.display(),
        holder.op_id,
        holder.age()
    )
}

/// The workspace a path that is about to be edited belongs to. The file may
/// not exist yet, so the key is taken from its nearest existing ancestor.
pub fn workspace_key_for_path(path: &Path, scopes: &[PathBuf]) -> PathBuf {
    let start = path.ancestors().find(|a| a.is_dir()).unwrap_or(path);
    workspace_key(start, scopes)
}

// ---------------------------------------------------------------------------
// In-process lanes
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Entry {
    seq: u64,
    holder: HolderInfo,
    /// The lease is held (both layers), i.e. the operation is running.
    granted: bool,
}

#[derive(Debug, Default)]
struct LaneQueue {
    state: Mutex<LaneState>,
    notify: Notify,
}

#[derive(Debug, Default)]
struct LaneState {
    next_seq: u64,
    /// Front = the holder (or the next to take the cross-process lock).
    entries: VecDeque<Entry>,
}

impl LaneQueue {
    fn remove(&self, seq: u64) {
        let mut state = self.state.lock();
        state.entries.retain(|e| e.seq != seq);
        drop(state);
        self.notify.notify_waiters();
    }

    fn is_head(&self, seq: u64) -> bool {
        self.state.lock().entries.front().map(|e| e.seq) == Some(seq)
    }

    fn ahead_of(&self, seq: u64) -> Vec<HolderInfo> {
        self.state
            .lock()
            .entries
            .iter()
            .take_while(|e| e.seq != seq)
            .map(|e| e.holder.clone())
            .collect()
    }

    /// The in-process holder that is actually running, if any.
    fn granted_holder(&self) -> Option<HolderInfo> {
        self.state
            .lock()
            .entries
            .iter()
            .find(|e| e.granted)
            .map(|e| e.holder.clone())
    }

    fn mark_granted(&self, seq: u64) {
        if let Some(e) = self.state.lock().entries.iter_mut().find(|e| e.seq == seq) {
            e.granted = true;
        }
    }
}

type LaneKey = (Option<PathBuf>, PathBuf);

/// Process-global lanes, keyed by `(lock dir, workspace)` so tests with private
/// lock directories never share a lane with each other or with production.
static LANES: LazyLock<Mutex<HashMap<LaneKey, Arc<LaneQueue>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lane_for(lock_dir: &Option<PathBuf>, key: &Path) -> Arc<LaneQueue> {
    LANES
        .lock()
        .entry((lock_dir.clone(), key.to_path_buf()))
        .or_default()
        .clone()
}

// ---------------------------------------------------------------------------
// Queue
// ---------------------------------------------------------------------------

/// What a workspace looks like right now, for a caller deciding whether an edit
/// may proceed (SPEC R2.7.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseProbe {
    /// No exclusive operation is running in the workspace.
    Free,
    /// An exclusive operation holds the workspace. `holder` is `None` only when
    /// another process holds it and has not (yet) published who it is.
    Held { holder: Option<HolderInfo> },
}

/// Errors from waiting for a lease.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("cancelled while queued for the workspace")]
    Cancelled,
}

/// The workspace write queue. Cheap to clone; all state is process-global.
#[derive(Debug, Clone)]
pub struct WorkspaceQueue {
    enabled: bool,
    lock_dir: Option<PathBuf>,
    /// `[tools] source_readers`: commands a project declares read-only for
    /// its sources (SPEC R2.7.8).
    source_readers: Vec<String>,
}

impl WorkspaceQueue {
    /// A queue using the per-user runtime lock directory.
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            lock_dir: default_lock_dir(),
            source_readers: Vec::new(),
        }
    }

    /// A queue with an explicit rendezvous directory (tests; `None` =
    /// in-process ordering only).
    pub fn with_lock_dir(enabled: bool, lock_dir: Option<PathBuf>) -> Self {
        Self {
            enabled,
            lock_dir,
            source_readers: Vec::new(),
        }
    }

    /// A project's own commands that only read sources (`[tools]
    /// source_readers`, SPEC R2.7.8), published on every holder this queue
    /// creates so the edit guard in another process can honour them.
    #[must_use]
    pub fn with_source_readers(mut self, readers: Vec<String>) -> Self {
        self.source_readers = readers;
        self
    }

    /// The declared source readers.
    pub fn source_readers(&self) -> &[String] {
        &self.source_readers
    }

    /// A queue that never orders anything — the pre-R2.7 behaviour, kept for
    /// `tools.workspace_queue = false` and embedders that opt out.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            lock_dir: None,
            source_readers: Vec::new(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn lock_path(&self, key: &Path) -> Option<PathBuf> {
        self.lock_dir
            .as_ref()
            .map(|d| d.join(format!("ws-{}.lock", key_id(key))))
    }

    fn holder_path(&self, key: &Path) -> Option<PathBuf> {
        self.lock_dir
            .as_ref()
            .map(|d| d.join(format!("ws-{}.holder.json", key_id(key))))
    }

    /// Take a place in line for `key`, **synchronously** — call this where the
    /// request arrives, before spawning anything, so arrival order is the
    /// execution order. `None` when the queue is disabled.
    pub fn enqueue(&self, key: &Path, holder: HolderInfo) -> Option<Ticket> {
        if !self.enabled {
            return None;
        }
        let lane = lane_for(&self.lock_dir, key);
        let seq = {
            let mut state = lane.state.lock();
            let seq = state.next_seq;
            state.next_seq += 1;
            state.entries.push_back(Entry {
                seq,
                holder: holder.clone(),
                granted: false,
            });
            seq
        };
        Some(Ticket {
            lane,
            seq,
            key: key.to_path_buf(),
            lock_path: self.lock_path(key),
            holder_path: self.holder_path(key),
            holder,
            done: false,
        })
    }

    /// Is an exclusive operation running in `key` right now — in this process
    /// or any other ahma process? Never waits and never takes the lease.
    pub fn probe(&self, key: &Path) -> LeaseProbe {
        if !self.enabled {
            return LeaseProbe::Free;
        }
        if let Some(holder) = lane_for(&self.lock_dir, key).granted_holder() {
            return LeaseProbe::Held {
                holder: Some(holder),
            };
        }
        if inherited_lease(key) {
            // We *are* running inside that lease (a command's own child).
            return LeaseProbe::Free;
        }
        let Some(lock_path) = self.lock_path(key) else {
            return LeaseProbe::Free;
        };
        match FsLock::try_acquire(&lock_path) {
            Ok(Some(_free)) => LeaseProbe::Free,
            Ok(None) => LeaseProbe::Held {
                holder: self.read_holder(key),
            },
            Err(e) => {
                tracing::debug!(
                    "workspace lease probe could not open {}: {e}",
                    lock_path.display()
                );
                LeaseProbe::Free
            }
        }
    }

    /// [`Self::probe`] for a path about to be edited, from a caller that may
    /// not know the sandbox scopes (the harness edit hook, SPEC R2.7.8).
    /// Returns the busy workspace and its holder, or `None` when it is free.
    ///
    /// Inside a git repository the key is the repository, the same as the
    /// server's. Outside one the server keys by its sandbox scope, which the
    /// hook cannot see, so every ancestor of the path whose rendezvous file
    /// exists is probed too — never creating one for a directory that was
    /// never a workspace.
    pub fn probe_path(
        &self,
        path: &Path,
        scopes: &[PathBuf],
    ) -> Option<(PathBuf, Option<HolderInfo>)> {
        let key = workspace_key_for_path(path, scopes);
        if let LeaseProbe::Held { holder } = self.probe(&key) {
            return Some((key, holder));
        }
        if key.join(".git").exists() {
            return None;
        }
        let start = path.ancestors().find(|a| a.is_dir()).unwrap_or(path);
        let start = dunce::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
        start
            .ancestors()
            .filter(|a| *a != key)
            .filter(|a| self.lock_path(a).is_some_and(|p| p.exists()))
            .find_map(|a| match self.probe(a) {
                LeaseProbe::Held { holder } => Some((a.to_path_buf(), holder)),
                LeaseProbe::Free => None,
            })
    }

    /// Whether an edit of `path` must wait, and the message saying so (SPEC
    /// R2.7.8). The one decision behind ahma's own file tools and the
    /// harness edit guard: a source reader (build, test, linter) never blocks;
    /// anything else blocks inside its footprint; an unreadable holder record
    /// blocks, as before. The full reason is given once per running command;
    /// every later refusal for it is one line.
    pub fn edit_conflict(&self, path: &Path, scopes: &[PathBuf]) -> Option<String> {
        let (workspace, holder) = self.probe_path(path, scopes)?;
        let canonical = {
            let start = path.ancestors().find(|a| a.exists()).unwrap_or(path);
            let rest = path.strip_prefix(start).unwrap_or(Path::new(""));
            dunce::canonicalize(start)
                .map(|c| c.join(rest))
                .unwrap_or_else(|_| path.to_path_buf())
        };
        let Some(holder) = holder else {
            return Some(edit_refusal(path, &workspace, None));
        };
        if !holder.blocks_edit_of(&canonical) {
            return None;
        }
        if self.already_refused(&workspace, &holder.op_id) {
            Some(edit_refusal_again(path, &holder))
        } else {
            Some(edit_refusal(path, &workspace, Some(&holder)))
        }
    }

    /// Record that `op_id` was named in a full refusal for `workspace`, and
    /// say whether it already had been. One small file beside the lock,
    /// overwritten when the holder changes; no lock directory means every
    /// refusal is a first one.
    fn already_refused(&self, workspace: &Path, op_id: &str) -> bool {
        let Some(lock) = self.lock_path(workspace) else {
            return false;
        };
        let marker = lock.with_extension("refused");
        if std::fs::read_to_string(&marker).is_ok_and(|s| s == op_id) {
            return true;
        }
        let _ = std::fs::write(&marker, op_id);
        false
    }

    /// Why `op_id` has not started: the operations ahead of it, if it is still
    /// waiting in a line (SPEC R2.7.3). `None` when it is not queued at all or
    /// already runs. An empty list means it is next, and waiting only for
    /// another process that has not published who it is.
    pub fn waiting_behind(&self, op_id: &str) -> Option<Vec<HolderInfo>> {
        let lanes: Vec<(PathBuf, Arc<LaneQueue>)> = LANES
            .lock()
            .iter()
            .filter(|((dir, _), _)| dir == &self.lock_dir)
            .map(|((_, key), lane)| (key.clone(), lane.clone()))
            .collect();
        for (key, lane) in lanes {
            let state = lane.state.lock();
            let Some(pos) = state.entries.iter().position(|e| e.holder.op_id == op_id) else {
                continue;
            };
            if state.entries[pos].granted {
                return None;
            }
            if pos > 0 {
                return Some(
                    state
                        .entries
                        .iter()
                        .take(pos)
                        .map(|e| e.holder.clone())
                        .collect(),
                );
            }
            drop(state);
            return Some(self.read_holder(&key).into_iter().collect());
        }
        None
    }

    /// The published holder of `key`'s lease in another process, if any.
    pub fn read_holder(&self, key: &Path) -> Option<HolderInfo> {
        let path = self.holder_path(key)?;
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }
}

/// Write the holder record beside the lock, atomically — to a private temp
/// file, then renamed over the record — so a reader never sees half a JSON
/// document and mistakes a held workspace for an anonymous holder.
async fn publish_holder(path: &Path, json: String) {
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    if tokio::fs::write(&tmp, json).await.is_ok() && tokio::fs::rename(&tmp, path).await.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
}

/// A place in a workspace's line. Dropping it (cancelled before it ran) gives
/// the place up; [`Ticket::acquire`] turns it into a [`Lease`].
#[derive(Debug)]
pub struct Ticket {
    lane: Arc<LaneQueue>,
    seq: u64,
    key: PathBuf,
    lock_path: Option<PathBuf>,
    holder_path: Option<PathBuf>,
    holder: HolderInfo,
    done: bool,
}

/// Progress callback while a ticket waits: called with who is ahead, at most
/// every few seconds, and doubles as the proof-of-life that keeps an
/// idle-output watchdog from killing a queued (not stalled) operation.
pub type WaitObserver<'a> = &'a (dyn Fn(&[HolderInfo]) + Send + Sync);

impl Ticket {
    pub fn key(&self) -> &Path {
        &self.key
    }

    /// Operations ahead of this ticket in this process, oldest first.
    pub fn ahead(&self) -> Vec<HolderInfo> {
        self.lane.ahead_of(self.seq)
    }

    /// Wait for the turn, then the cross-process lock. Returns immediately when
    /// nothing is ahead. Cancelling `cancel` gives the place up.
    pub async fn acquire(
        mut self,
        cancel: &CancellationToken,
        observe: WaitObserver<'_>,
    ) -> Result<Lease, QueueError> {
        const OBSERVE_EVERY: Duration = Duration::from_secs(5);
        let mut last_observed = Instant::now() - OBSERVE_EVERY;

        // ── Layer 1: in-process FIFO ────────────────────────────────────────
        loop {
            let notified = self.lane.notify.notified();
            tokio::pin!(notified);
            // Register before checking, so a release between the check and the
            // await cannot be missed.
            notified.as_mut().enable();
            if self.lane.is_head(self.seq) {
                break;
            }
            if last_observed.elapsed() >= OBSERVE_EVERY {
                observe(&self.ahead());
                last_observed = Instant::now();
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = cancel.cancelled() => return Err(QueueError::Cancelled),
                // Periodic wake-up purely to re-observe (liveness), never to poll state.
                _ = tokio::time::sleep(OBSERVE_EVERY) => {}
            }
        }

        // ── Layer 2: cross-process kernel lock ──────────────────────────────
        let fs_lock = match (&self.lock_path, inherited_lease(&self.key)) {
            (Some(path), false) => {
                let mut backoff = Duration::from_millis(20);
                loop {
                    // open + flock are blocking syscalls: keep them off the
                    // async worker threads.
                    let attempt = {
                        let path = path.clone();
                        tokio::task::spawn_blocking(move || FsLock::try_acquire(&path))
                            .await
                            .unwrap_or_else(|e| Err(std::io::Error::other(e)))
                    };
                    match attempt {
                        Ok(Some(lock)) => break Some(lock),
                        Ok(None) => {}
                        Err(e) => {
                            // Degrade loudly, never wedge: in-process order
                            // still holds; cross-process order does not.
                            tracing::warn!(
                                "workspace queue: cannot open lease file {}: {e} — \
                                 ordering within this process only",
                                path.display()
                            );
                            break None;
                        }
                    }
                    if last_observed.elapsed() >= OBSERVE_EVERY {
                        let elsewhere = match &self.holder_path {
                            Some(p) => tokio::fs::read_to_string(p).await.ok(),
                            None => None,
                        }
                        .and_then(|t| serde_json::from_str::<HolderInfo>(&t).ok());
                        observe(&elsewhere.into_iter().collect::<Vec<_>>());
                        last_observed = Instant::now();
                    }
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(QueueError::Cancelled),
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(Duration::from_millis(250));
                }
            }
            _ => None,
        };

        // Publish who holds it, for the next waiter's message and the edit guard.
        // Advisory only: the kernel lock is the authority.
        let mut holder = self.holder.clone();
        holder.since_unix = unix_now();
        if fs_lock.is_some()
            && let Some(path) = &self.holder_path
            && let Ok(json) = serde_json::to_string(&holder)
        {
            publish_holder(path, json).await;
        }

        self.lane.mark_granted(self.seq);
        self.done = true;
        Ok(Lease {
            lane: self.lane.clone(),
            seq: self.seq,
            key: self.key.clone(),
            holder_path: if fs_lock.is_some() {
                self.holder_path.clone()
            } else {
                None
            },
            _fs_lock: fs_lock,
        })
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if !self.done {
            self.lane.remove(self.seq);
        }
    }
}

/// The right to run an exclusive operation in a workspace. Held for the life of
/// the process tree; dropping it hands the workspace to the next in line.
#[derive(Debug)]
pub struct Lease {
    lane: Arc<LaneQueue>,
    seq: u64,
    key: PathBuf,
    holder_path: Option<PathBuf>,
    _fs_lock: Option<FsLock>,
}

impl Lease {
    pub fn key(&self) -> &Path {
        &self.key
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        // Order matters: withdraw the published holder *before* the kernel lock
        // is released (field drop, after this body), so it can never erase the
        // next holder's record.
        if let Some(path) = &self.holder_path {
            let _ = std::fs::remove_file(path);
        }
        self.lane.remove(self.seq);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    fn no_observe(_: &[HolderInfo]) {}

    fn queue(dir: &Path) -> WorkspaceQueue {
        WorkspaceQueue::with_lock_dir(true, Some(dir.join("locks")))
    }

    #[test]
    fn workspace_key_is_the_repository_root() {
        let td = tempdir().unwrap();
        let repo = td.path().join("repo");
        let sub = repo.join("crate_a").join("src");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let key = workspace_key(&sub, &[]);
        assert_eq!(key, dunce::canonicalize(&repo).unwrap());
    }

    #[test]
    fn a_worktree_is_its_own_workspace() {
        let td = tempdir().unwrap();
        let wt = td.path().join("wt");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        // A worktree's `.git` is a file.
        std::fs::write(wt.join(".git"), "gitdir: /elsewhere").unwrap();
        assert_eq!(
            workspace_key(&wt.join("src"), &[]),
            dunce::canonicalize(&wt).unwrap()
        );
    }

    #[test]
    fn without_git_the_longest_containing_scope_is_the_key() {
        let td = tempdir().unwrap();
        let outer = td.path().join("outer");
        let inner = outer.join("inner");
        let wd = inner.join("deep");
        std::fs::create_dir_all(&wd).unwrap();
        let key = workspace_key(&wd, &[outer.clone(), inner.clone()]);
        assert_eq!(key, dunce::canonicalize(&inner).unwrap());
    }

    /// SPEC R2.7.6: a `.git` above the sandbox scope (a dotfiles repository in
    /// `$HOME`) may key the lease, but the drift walk never leaves the scope —
    /// it would list files the sandbox forbids the model to see.
    #[test]
    fn the_drift_root_never_leaves_the_sandbox_scope() {
        let td = tempdir().unwrap();
        let home = td.path().join("home");
        let project = home.join("project");
        let wd = project.join("src");
        std::fs::create_dir_all(&wd).unwrap();
        std::fs::create_dir_all(home.join(".git")).unwrap();
        let scopes = vec![project.clone()];
        let key = workspace_key(&wd, &scopes);
        assert_eq!(
            key,
            dunce::canonicalize(&home).unwrap(),
            "the lease key is the repo"
        );
        assert_eq!(
            drift_root(&key, &scopes, &wd),
            dunce::canonicalize(&project).unwrap(),
            "the walk is clipped to the scope"
        );

        // Inside the scope, the repository root is the walk root.
        std::fs::create_dir_all(project.join(".git")).unwrap();
        let key = workspace_key(&wd, &scopes);
        assert_eq!(drift_root(&key, &scopes, &wd), key);
    }

    #[test]
    fn stable_hash_is_fnv1a() {
        // Pinned values: the file name must not change between releases.
        assert_eq!(stable_hash(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(stable_hash(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(format_duration_secs(7), "7s");
        assert_eq!(format_duration_secs(125), "2m05s");
        assert_eq!(format_duration_secs(3725), "1h02m05s");
    }

    #[tokio::test]
    async fn an_uncontended_ticket_is_granted_immediately() {
        let td = tempdir().unwrap();
        let q = queue(td.path());
        let t = q
            .enqueue(td.path(), HolderInfo::new("op1", "echo"))
            .unwrap();
        assert!(t.ahead().is_empty());
        let lease = t
            .acquire(&CancellationToken::new(), &no_observe)
            .await
            .unwrap();
        assert_eq!(lease.key(), td.path());
    }

    #[tokio::test]
    async fn leases_are_granted_in_arrival_order() {
        let td = tempdir().unwrap();
        let q = queue(td.path());
        let key = td.path().to_path_buf();
        let first = q.enqueue(&key, HolderInfo::new("op1", "a")).unwrap();
        let second = q.enqueue(&key, HolderInfo::new("op2", "b")).unwrap();
        let third = q.enqueue(&key, HolderInfo::new("op3", "c")).unwrap();
        assert_eq!(
            third
                .ahead()
                .iter()
                .map(|h| h.op_id.as_str())
                .collect::<Vec<_>>(),
            vec!["op1", "op2"]
        );

        let order = Arc::new(Mutex::new(Vec::new()));
        // Start the later tickets *first*: arrival order, not polling order, wins.
        let mut handles = Vec::new();
        for (name, ticket) in [("op3", third), ("op2", second)] {
            let order = order.clone();
            handles.push(tokio::spawn(async move {
                let _lease = ticket
                    .acquire(&CancellationToken::new(), &no_observe)
                    .await
                    .unwrap();
                order.lock().push(name);
            }));
        }
        tokio::task::yield_now().await;
        assert!(order.lock().is_empty(), "nothing may run before op1");
        let lease1 = first
            .acquire(&CancellationToken::new(), &no_observe)
            .await
            .unwrap();
        order.lock().push("op1");
        drop(lease1);
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(*order.lock(), vec!["op1", "op2", "op3"]);
    }

    #[tokio::test]
    async fn a_cancelled_waiter_gives_its_place_up() {
        let td = tempdir().unwrap();
        let q = queue(td.path());
        let key = td.path().to_path_buf();
        let holder = q
            .enqueue(&key, HolderInfo::new("op1", "a"))
            .unwrap()
            .acquire(&CancellationToken::new(), &no_observe)
            .await
            .unwrap();
        let waiter = q.enqueue(&key, HolderInfo::new("op2", "b")).unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = waiter.acquire(&cancel, &no_observe).await.unwrap_err();
        assert!(matches!(err, QueueError::Cancelled));
        drop(holder);
        // op3 must not be stuck behind the cancelled op2.
        let t3 = q.enqueue(&key, HolderInfo::new("op3", "c")).unwrap();
        assert!(t3.ahead().is_empty());
    }

    #[tokio::test]
    async fn the_waiter_is_observed_with_whoever_is_ahead() {
        let td = tempdir().unwrap();
        let q = queue(td.path());
        let key = td.path().to_path_buf();
        let lease = q
            .enqueue(&key, HolderInfo::new("op1", "cargo nextest run"))
            .unwrap()
            .acquire(&CancellationToken::new(), &no_observe)
            .await
            .unwrap();
        let waiter = q.enqueue(&key, HolderInfo::new("op2", "b")).unwrap();
        let seen = Arc::new(AtomicUsize::new(0));
        let seen2 = seen.clone();
        let handle = tokio::spawn(async move {
            let observe = move |ahead: &[HolderInfo]| {
                if ahead.iter().any(|h| h.op_id == "op1") {
                    seen2.fetch_add(1, Ordering::SeqCst);
                }
            };
            waiter
                .acquire(&CancellationToken::new(), &observe)
                .await
                .map(|_| ())
        });
        // The first observation happens before the first wait.
        for _ in 0..100 {
            if seen.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(seen.load(Ordering::SeqCst) > 0);
        drop(lease);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn probe_reports_an_in_process_holder_and_then_free() {
        let td = tempdir().unwrap();
        let q = queue(td.path());
        let key = td.path().to_path_buf();
        assert_eq!(q.probe(&key), LeaseProbe::Free);
        let lease = q
            .enqueue(&key, HolderInfo::new("op1", "cargo test"))
            .unwrap()
            .acquire(&CancellationToken::new(), &no_observe)
            .await
            .unwrap();
        match q.probe(&key) {
            LeaseProbe::Held { holder: Some(h) } => assert_eq!(h.op_id, "op1"),
            other => panic!("expected held by op1, got {other:?}"),
        }
        drop(lease);
        assert_eq!(q.probe(&key), LeaseProbe::Free);
    }

    #[tokio::test]
    async fn the_holder_is_published_outside_the_workspace_and_withdrawn() {
        let td = tempdir().unwrap();
        let q = queue(td.path());
        let key = td.path().join("ws");
        std::fs::create_dir_all(&key).unwrap();
        let lease = q
            .enqueue(&key, HolderInfo::new("op9", "cargo build"))
            .unwrap()
            .acquire(&CancellationToken::new(), &no_observe)
            .await
            .unwrap();
        let published = q.read_holder(&key).expect("holder published");
        assert_eq!(published.op_id, "op9");
        // Nothing is written into the workspace itself.
        assert_eq!(std::fs::read_dir(&key).unwrap().count(), 0);
        drop(lease);
        assert!(q.read_holder(&key).is_none(), "withdrawn on release");
    }

    #[tokio::test]
    async fn another_process_holding_the_kernel_lock_blocks_and_is_probed() {
        // flock conflicts between two open file descriptions even inside one
        // process, so holding the rendezvous file directly stands in for a
        // second ahma process.
        let td = tempdir().unwrap();
        let q = queue(td.path());
        let key = td.path().join("ws");
        std::fs::create_dir_all(&key).unwrap();
        let foreign = FsLock::acquire(&q.lock_path(&key).unwrap()).unwrap();
        assert_eq!(q.probe(&key), LeaseProbe::Held { holder: None });

        let ticket = q.enqueue(&key, HolderInfo::new("op1", "x")).unwrap();
        let handle = tokio::spawn(async move {
            ticket
                .acquire(&CancellationToken::new(), &no_observe)
                .await
                .map(drop)
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!handle.is_finished(), "must wait for the foreign holder");
        drop(foreign);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn waiting_behind_names_who_is_ahead_until_granted() {
        let td = tempdir().unwrap();
        let q = queue(td.path());
        let key = td.path().to_path_buf();
        let lease = q
            .enqueue(&key, HolderInfo::new("op1", "cargo nextest run"))
            .unwrap()
            .acquire(&CancellationToken::new(), &no_observe)
            .await
            .unwrap();
        let ticket = q
            .enqueue(&key, HolderInfo::new("op2", "sed -i x f"))
            .unwrap();
        assert_eq!(q.waiting_behind("op1"), None, "op1 is running");
        let ahead = q.waiting_behind("op2").expect("op2 waits");
        assert_eq!(ahead.len(), 1);
        assert_eq!(ahead[0].op_id, "op1");
        drop(lease);
        let _lease2 = ticket
            .acquire(&CancellationToken::new(), &no_observe)
            .await
            .unwrap();
        assert_eq!(q.waiting_behind("op2"), None);
        assert_eq!(q.waiting_behind("unknown"), None);
    }

    #[tokio::test]
    async fn a_disabled_queue_orders_nothing() {
        let q = WorkspaceQueue::disabled();
        assert!(
            q.enqueue(Path::new("/x"), HolderInfo::new("a", "b"))
                .is_none()
        );
        assert_eq!(q.probe(Path::new("/x")), LeaseProbe::Free);
    }

    #[test]
    fn child_lease_env_accumulates_without_duplicates() {
        // Runs in its own nextest process, so the env is private to it.
        unsafe { std::env::remove_var(HELD_LEASE_ENV) };
        let a = Path::new("/ws/a");
        let b = Path::new("/ws/b");
        let one = child_lease_env(a);
        assert_eq!(one, key_id(a));
        unsafe { std::env::set_var(HELD_LEASE_ENV, &one) };
        assert!(inherited_lease(a));
        assert!(!inherited_lease(b));
        assert_eq!(child_lease_env(a), one);
        assert_eq!(child_lease_env(b), format!("{one},{}", key_id(b)));
        unsafe { std::env::remove_var(HELD_LEASE_ENV) };
    }
}

#[cfg(test)]
mod holder_listing_tests {
    use super::*;
    use tempfile::tempdir;

    /// `ahma queue` reads the published holder records, so a human (or an
    /// agent whose every command is waiting) can see *who* holds a workspace
    /// and whether that process is even alive (SPEC R2.7.9).
    #[test]
    fn list_holders_reports_each_workspace_and_its_liveness() {
        let td = tempdir().unwrap();
        let lock_dir = td.path().join("locks");
        std::fs::create_dir_all(&lock_dir).unwrap();
        let live = HolderInfo {
            op_id: "op_live".into(),
            title: "cargo nextest run".into(),
            pid: 1,
            since_unix: 1_000,
            ..HolderInfo::new("", "")
        };
        let dead = HolderInfo {
            op_id: "op_dead".into(),
            title: "cargo build".into(),
            pid: 2,
            since_unix: 2_000,
            ..HolderInfo::new("", "")
        };
        for (key, h) in [("/ws/a", &live), ("/ws/b", &dead)] {
            let path = lock_dir.join(format!("ws-{}.holder.json", key_id(Path::new(key))));
            std::fs::write(&path, serde_json::to_string(h).unwrap()).unwrap();
        }
        let listed = list_holders(&lock_dir, &|pid| pid == 1).unwrap();
        assert_eq!(listed.len(), 2);
        let a = listed.iter().find(|r| r.holder.op_id == "op_live").unwrap();
        assert!(a.alive);
        let b = listed.iter().find(|r| r.holder.op_id == "op_dead").unwrap();
        assert!(!b.alive, "a dead holder is shown as such, not hidden");
        assert!(
            list_holders(td.path().join("nope").as_path(), &|_| true)
                .unwrap()
                .is_empty()
        );
    }
}

/// One workspace lease as published beside its lock, with whether the holder
/// process is still alive (SPEC R2.7.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueHolderRecord {
    /// The hashed workspace key (`ws-<id>`), the only identity the record has.
    pub key_id: String,
    pub holder: HolderInfo,
    pub alive: bool,
}

/// Every published holder under `lock_dir`, for `ahma queue`. Reading the
/// records takes no lock and never queues: it is what an agent whose every
/// command is waiting can still run to learn *who* it is waiting for. A dead
/// holder is listed as dead rather than hidden, because the OS lock it left
/// behind has already been released (R2.7.7) and the stale record is the only
/// thing still pointing at it.
/// Every workspace lease published in `lock_dir`. A directory that does not
/// exist means nothing is held; one that exists but cannot be read is an
/// error, never "nothing is held" (SPEC R2.7.9): inside ahma's sandbox the
/// runtime directory is out of scope by design, and an empty answer there
/// told a waiting agent no one was in its way while someone was.
pub fn list_holders(
    lock_dir: &Path,
    alive: &dyn Fn(u32) -> bool,
) -> std::io::Result<Vec<QueueHolderRecord>> {
    let entries = match std::fs::read_dir(lock_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out: Vec<QueueHolderRecord> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let key_id = name
                .strip_prefix("ws-")?
                .strip_suffix(".holder.json")?
                .to_string();
            let holder: HolderInfo =
                serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok()?;
            let alive = alive(holder.pid);
            Some(QueueHolderRecord {
                key_id,
                holder,
                alive,
            })
        })
        .collect();
    out.sort_by_key(|r| r.holder.since_unix);
    Ok(out)
}

#[cfg(test)]
mod edit_conflict_tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    /// A holder record written by an older ahma has no effect or footprint;
    /// it must keep today's conservative meaning: it blocks every edit.
    #[test]
    fn an_old_holder_record_blocks_every_edit() {
        let old = r#"{"op_id":"op_1","title":"cargo nextest run","pid":1,"since_unix":0}"#;
        let h: HolderInfo = serde_json::from_str(old).unwrap();
        assert_eq!(h.effect, SourceEffect::Unknown);
        assert!(h.blocks_edit_of(Path::new("/repo/src/lib.rs")));
    }

    #[test]
    fn a_source_reader_never_blocks_and_a_footprint_bounds_the_rest() {
        let reader = HolderInfo::new("op_1", "cargo nextest run");
        assert_eq!(reader.effect, SourceEffect::ReadsSources);
        assert!(!reader.blocks_edit_of(Path::new("/repo/src/lib.rs")));

        let fmt = HolderInfo::new("op_2", "cargo fmt").with_footprint(PathBuf::from("/repo/rust"));
        assert!(fmt.blocks_edit_of(Path::new("/repo/rust/src/lib.rs")));
        assert!(
            !fmt.blocks_edit_of(Path::new("/repo/android/Main.kt")),
            "an edit outside the subtree the writer runs in cannot race it"
        );
    }

    async fn held(queue: &WorkspaceQueue, key: &Path, holder: HolderInfo) -> Lease {
        queue
            .enqueue(key, holder)
            .unwrap()
            .acquire(&CancellationToken::new(), &|_| {})
            .await
            .unwrap()
    }

    /// SPEC R2.7.8: an edit during a build or test is allowed; the drift
    /// report says the run may have seen it.
    #[tokio::test]
    async fn an_edit_during_a_test_run_is_not_refused() {
        let td = tempfile::tempdir().unwrap();
        let repo = dunce::canonicalize(td.path()).unwrap().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let queue = WorkspaceQueue::with_lock_dir(true, Some(td.path().join("locks")));
        let _lease = held(&queue, &repo, HolderInfo::new("op_1", "cargo nextest run")).await;
        assert!(queue.edit_conflict(&repo.join("src/lib.rs"), &[]).is_none());
    }

    /// A refusal is said in full once per running command; repeats are one
    /// line that says what is still running and for how long.
    #[tokio::test]
    async fn a_repeated_refusal_is_one_line() {
        let td = tempfile::tempdir().unwrap();
        let repo = dunce::canonicalize(td.path()).unwrap().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let queue = WorkspaceQueue::with_lock_dir(true, Some(td.path().join("locks")));
        let _lease = held(
            &queue,
            &repo,
            HolderInfo::new("op_7", "cargo fmt --all").with_typical_secs(Some(40)),
        )
        .await;
        let first = queue
            .edit_conflict(&repo.join("src/lib.rs"), &[])
            .expect("a formatter blocks the edit");
        assert!(first.contains("Not edited"), "{first}");
        assert!(first.contains("cargo fmt --all"), "{first}");
        assert!(first.contains("usually takes 40s"), "{first}");
        let second = queue
            .edit_conflict(&repo.join("src/main.rs"), &[])
            .expect("still blocked");
        assert!(!second.contains('\n'), "a repeat is one line: {second}");
        assert!(second.contains("op_7"), "{second}");
        assert!(second.len() < first.len() / 2, "{second}");
    }
}

#[cfg(test)]
mod unreadable_lock_dir_tests {
    use super::*;

    /// A lock directory that exists but cannot be read is an error, not an
    /// empty queue (SPEC R2.7.9).
    #[cfg(unix)]
    #[test]
    fn an_unreadable_lock_dir_is_an_error_not_an_empty_queue() {
        use std::os::unix::fs::PermissionsExt;
        let td = tempfile::tempdir().unwrap();
        let dir = td.path().join("locks");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = list_holders(&dir, &|_| true);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        if nix_is_root() {
            return; // root reads anything; nothing to assert
        }
        assert!(result.is_err(), "{result:?}");
    }

    #[cfg(unix)]
    fn nix_is_root() -> bool {
        // SAFETY: getuid has no preconditions.
        unsafe { libc::getuid() == 0 }
    }
}
