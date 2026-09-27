//! # Command Mutex Groups
//!
//! Provides [`CommandMutexRegistry`]: a registry of per-`(group, working_directory)`
//! semaphores that serialise commands matching configured prefix patterns.
//!
//! ## Why this exists
//!
//! Some CLI tools (most prominently `cargo`) use a shared file-system lock on a
//! build artefact directory (`target/`).  Running multiple such commands
//! concurrently does not cause correctness issues — the tools handle the lock
//! internally — but it **does** cause:
//!
//! * Heavy I/O contention and duplicated compilation work.
//! * "Blocking waiting for file lock on build directory" log spam.
//! * Cascading timeouts when an AI agent queues several cargo commands at once.
//!
//! [`CommandMutexRegistry`] gates these commands behind a per-workspace semaphore
//! so at most one runs at a time **within each workspace** — the repository root
//! (SPEC R2.7.2), not the working directory: `cargo test` in `crate_a/` and
//! `cargo build` at the root share one `target/`, so they share one gate.
//! Commands in different workspaces never block each other.
//!
//! ## Two layers of serialisation
//!
//! 1. **In-memory semaphore** — prevents thundering-herd wakeups within a
//!    single process: N async tasks don't all race to the filesystem lock.
//!
//! 2. **Cross-process filesystem advisory lock** ([`FsLock`]) — serialises
//!    across ahma sessions, hook invocations, and any other OS processes.
//!    The lock is automatically released by the OS when the owning process
//!    exits for any reason (including panic, `SIGKILL`, or crash).  No stale
//!    locks, no manual cleanup.  The rendezvous file lives in the per-user
//!    runtime lock directory, **never in the workspace**: a file the agent can
//!    delete (or `git clean -fdx` can sweep) while it is held lets a second
//!    holder in, and an untracked lock file is `git status` noise.
//!
//! ## Configuration
//!
//! Groups are defined in `~/.ahma/settings.toml` under `[tools].mutex_groups`.
//! The default ships a single `cargo` group.  Set `mutex_groups = []` to disable
//! all gating.

use ahma_common::config::MutexGroupConfig;
use ahma_common::fs_lock::FsLock;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{OwnedSemaphorePermit, RwLock, Semaphore};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur while waiting for a mutex group permit.
#[derive(Debug, thiserror::Error)]
pub enum MutexGroupError {
    /// The command waited longer than `max_wait_secs` and gave up.
    #[error("Timed out waiting for mutex group '{group}' after {secs}s")]
    Timeout { group: String, secs: u64 },
    /// The semaphore was closed — should never happen in normal operation.
    #[error("Mutex group '{group}' semaphore was unexpectedly closed")]
    Closed { group: String },
}

// ---------------------------------------------------------------------------
// Guard — holds both in-memory and filesystem locks
// ---------------------------------------------------------------------------

/// RAII guard returned by [`CommandMutexRegistry::acquire`].
///
/// Holds both the in-memory semaphore permit (intra-process) and an optional
/// filesystem advisory lock (cross-process).  Both are released when the guard
/// is dropped.
#[derive(Debug)]
pub struct MutexGroupGuard {
    /// In-memory semaphore permit — released on drop.
    _permit: OwnedSemaphorePermit,
    /// Cross-process filesystem lock — released on drop (OS closes the fd).
    /// `None` if the filesystem lock could not be acquired (degraded mode).
    _fs_lock: Option<FsLock>,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Inner state: maps `(group_name, canonical_working_dir)` → shared semaphore.
type SemaphoreMap = RwLock<HashMap<(String, PathBuf), Arc<Semaphore>>>;

/// Registry of per-`(group, working_directory)` semaphores.
///
/// Constructed once at service startup from [`MutexGroupConfig`] list.
/// Shared across all async operations via [`Arc`].
#[derive(Debug)]
pub struct CommandMutexRegistry {
    /// Configured groups (used for prefix matching).
    groups: Vec<MutexGroupConfig>,
    /// Lazily-created semaphores, one per `(group_name, canonical_dir)` pair.
    semaphores: Arc<SemaphoreMap>,
    /// Where the cross-process rendezvous files live. `None` = in-process only.
    lock_dir: Option<PathBuf>,
}

impl CommandMutexRegistry {
    /// Create a registry from the provided group configurations.
    ///
    /// Pass an empty slice to disable all mutex gating.
    pub fn from_config(groups: &[MutexGroupConfig]) -> Self {
        Self::from_config_with_lock_dir(groups, super::workspace_queue::default_lock_dir())
    }

    /// [`Self::from_config`] with an explicit rendezvous directory (tests).
    pub fn from_config_with_lock_dir(
        groups: &[MutexGroupConfig],
        lock_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            groups: groups.to_vec(),
            semaphores: Arc::new(RwLock::new(HashMap::new())),
            lock_dir,
        }
    }

    /// Find the first group whose `prefixes` list contains the first
    /// whitespace-separated token of `command`.
    ///
    /// Returns `None` if `command` is not gated by any group.
    pub fn find_group<'a>(&'a self, command: &str) -> Option<&'a MutexGroupConfig> {
        let first_token = command.split_whitespace().next()?;
        let low = first_token.to_ascii_lowercase();
        self.groups
            .iter()
            .find(|g| g.prefixes.iter().any(|p| p.to_ascii_lowercase() == low))
    }

    /// Acquire the mutex group gate for `(group.name, canonical(workspace))`.
    ///
    /// `workspace` is the workspace key (SPEC R2.7.2), not the working directory.
    /// This acquires **two layers** of serialisation:
    ///
    /// 1. An in-memory semaphore permit (prevents intra-process thundering herd).
    /// 2. A cross-process filesystem advisory lock on
    ///    `<lock dir>/grp-<group>-<workspace id>.lock` (serialises across ahma
    ///    sessions, hooks, and external processes).
    ///
    /// Blocks until:
    /// * Both locks become available, or
    /// * `group.max_wait_secs` elapses → returns [`MutexGroupError::Timeout`].
    ///
    /// The returned [`MutexGroupGuard`] must be held for the entire duration
    /// of the command; dropping it releases both locks.
    pub async fn acquire(
        &self,
        group: &MutexGroupConfig,
        working_dir: &Path,
    ) -> Result<MutexGroupGuard, MutexGroupError> {
        let canonical =
            dunce::canonicalize(working_dir).unwrap_or_else(|_| working_dir.to_path_buf());
        let key = (group.name.clone(), canonical.clone());

        // Fast path: semaphore already exists.
        let sem = {
            let read = self.semaphores.read().await;
            read.get(&key).cloned()
        };

        let sem = match sem {
            Some(s) => s,
            None => {
                // Slow path: first command in this (group, dir) pair.
                let mut write = self.semaphores.write().await;
                // Re-check under write lock to avoid races.
                write
                    .entry(key)
                    .or_insert_with(|| Arc::new(Semaphore::new(1)))
                    .clone()
            }
        };

        // ── Layer 1: in-memory semaphore (intra-process) ────────────────
        let permit = match tokio::time::timeout(
            std::time::Duration::from_secs(group.max_wait_secs),
            sem.acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => {
                return Err(MutexGroupError::Closed {
                    group: group.name.clone(),
                });
            }
            Err(_) => {
                return Err(MutexGroupError::Timeout {
                    group: group.name.clone(),
                    secs: group.max_wait_secs,
                });
            }
        };

        // ── Layer 2: cross-process filesystem advisory lock ─────────────
        // The rendezvous file lives in the runtime lock directory, keyed by the
        // workspace — never inside the workspace, where the agent could delete
        // it while held (see the module docs).
        let Some(lock_path) = self.lock_dir.as_ref().map(|dir| {
            dir.join(format!(
                "grp-{}-{}.lock",
                sanitize_group_name(&group.name),
                super::workspace_queue::key_id(&canonical)
            ))
        }) else {
            return Ok(MutexGroupGuard {
                _permit: permit,
                _fs_lock: None,
            });
        };
        let fs_lock = tokio::task::spawn_blocking(move || {
            match FsLock::acquire(&lock_path) {
                Ok(lock) => Some(lock),
                Err(e) => {
                    // Degrade gracefully: intra-process gate still holds, just
                    // no cross-process protection.
                    tracing::warn!(
                        "Could not acquire filesystem lock {}: {e} \
                         (intra-process gate still active)",
                        lock_path.display()
                    );
                    None
                }
            }
        })
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("spawn_blocking for fs lock panicked: {e}");
            None
        });

        Ok(MutexGroupGuard {
            _permit: permit,
            _fs_lock: fs_lock,
        })
    }

    /// Returns the number of `(group, dir)` semaphores currently tracked.
    #[cfg(test)]
    pub fn lock_dir(&self) -> Option<&Path> {
        self.lock_dir.as_deref()
    }

    /// Returns the number of `(group, dir)` semaphores currently tracked.
    ///
    /// Primarily for tests and diagnostics.
    #[cfg(test)]
    pub async fn semaphore_count(&self) -> usize {
        self.semaphores.read().await.len()
    }
}

/// A group name made safe for a file name.
fn sanitize_group_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ahma_common::config::MutexGroupConfig;
    use std::sync::Arc;
    use tempfile::{TempDir, tempdir};
    use tokio::time::{Duration, sleep};

    fn cargo_group() -> MutexGroupConfig {
        MutexGroupConfig {
            name: "cargo".to_string(),
            prefixes: vec!["cargo".to_string()],
            max_wait_secs: 5,
        }
    }

    /// A registry whose rendezvous files go to a private temp dir, so tests
    /// never touch the user's runtime directory.
    fn registry() -> (TempDir, CommandMutexRegistry) {
        let locks = tempdir().unwrap();
        let reg = CommandMutexRegistry::from_config_with_lock_dir(
            &[cargo_group()],
            Some(locks.path().join("locks")),
        );
        (locks, reg)
    }

    // ── find_group ──────────────────────────────────────────────────────────

    #[test]
    fn test_find_group_matches_cargo_prefix() {
        let (_locks, reg) = registry();
        assert!(reg.find_group("cargo build").is_some());
        assert!(reg.find_group("cargo nextest run --all").is_some());
        assert!(reg.find_group("CARGO check").is_some()); // case-insensitive
    }

    #[test]
    fn test_find_group_no_match_for_unrelated_command() {
        let (_locks, reg) = registry();
        assert!(reg.find_group("git status").is_none());
        assert!(reg.find_group("echo hello").is_none());
        assert!(reg.find_group("").is_none());
    }

    #[test]
    fn test_find_group_empty_registry_never_matches() {
        let reg = CommandMutexRegistry::from_config(&[]);
        assert!(reg.find_group("cargo build").is_none());
    }

    // ── same-dir serialisation ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_same_dir_serialises_commands() {
        let td = tempdir().unwrap();
        let (_locks, reg) = registry();
        let reg = Arc::new(reg);
        let group = cargo_group();

        // Acquire the permit in task A.
        let guard_a = reg.acquire(&group, td.path()).await.unwrap();

        // Task B tries to acquire — must wait.
        let reg2 = reg.clone();
        let dir = td.path().to_path_buf();
        let g2 = cargo_group();
        let handle = tokio::spawn(async move { reg2.acquire(&g2, &dir).await });

        // Give B a moment to queue up.
        sleep(Duration::from_millis(50)).await;

        // Drop A's guard — B should now succeed.
        drop(guard_a);
        let result = handle.await.unwrap();
        assert!(result.is_ok(), "task B should succeed after A releases");
    }

    // ── different-dir independence ───────────────────────────────────────────

    #[tokio::test]
    async fn test_different_dirs_run_in_parallel() {
        let td1 = tempdir().unwrap();
        let td2 = tempdir().unwrap();
        let (_locks, reg) = registry();
        let reg = Arc::new(reg);
        let group = cargo_group();

        // Acquire permit for dir1.
        let _guard1 = reg.acquire(&group, td1.path()).await.unwrap();

        // Acquiring for dir2 must NOT block (different semaphore key).
        let reg2 = reg.clone();
        let dir2 = td2.path().to_path_buf();
        let g2 = cargo_group();
        let result = tokio::time::timeout(
            Duration::from_millis(200),
            tokio::spawn(async move { reg2.acquire(&g2, &dir2).await }),
        )
        .await;

        assert!(result.is_ok(), "dir2 acquire should not be blocked by dir1");
    }

    // ── timeout ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_acquire_times_out_when_held() {
        let td = tempdir().unwrap();
        let (_locks, reg) = registry();
        let short_group = MutexGroupConfig {
            name: "cargo".to_string(),
            prefixes: vec!["cargo".to_string()],
            max_wait_secs: 0, // instant timeout
        };

        // Hold the guard.
        let _held = reg.acquire(&short_group, td.path()).await.unwrap();

        // Second acquire must time out.
        let result = reg.acquire(&short_group, td.path()).await;
        assert!(
            matches!(result, Err(MutexGroupError::Timeout { .. })),
            "expected Timeout, got: {result:?}"
        );
    }

    // ── rendezvous file location ─────────────────────────────────────────────

    #[tokio::test]
    async fn the_lock_file_is_never_created_inside_the_workspace() {
        let workspace = tempdir().unwrap();
        let (_locks, reg) = registry();
        let _guard = reg.acquire(&cargo_group(), workspace.path()).await.unwrap();
        assert_eq!(
            std::fs::read_dir(workspace.path()).unwrap().count(),
            0,
            "the workspace must stay untouched"
        );
        let lock_dir = reg.lock_dir().unwrap();
        let files: Vec<_> = std::fs::read_dir(lock_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(files.len(), 1);
        assert!(files[0].starts_with("grp-cargo-"), "{files:?}");
    }

    #[tokio::test]
    async fn two_registries_serialise_through_the_shared_lock_dir() {
        // Two registries stand in for two ahma processes: they share nothing
        // in memory, only the rendezvous directory.
        let workspace = tempdir().unwrap();
        let locks = tempdir().unwrap();
        let dir = locks.path().join("locks");
        let a =
            CommandMutexRegistry::from_config_with_lock_dir(&[cargo_group()], Some(dir.clone()));
        let b = Arc::new(CommandMutexRegistry::from_config_with_lock_dir(
            &[cargo_group()],
            Some(dir),
        ));
        let held = a.acquire(&cargo_group(), workspace.path()).await.unwrap();
        let ws = workspace.path().to_path_buf();
        let b2 = b.clone();
        let handle = tokio::spawn(async move { b2.acquire(&cargo_group(), &ws).await.map(|_| ()) });
        sleep(Duration::from_millis(100)).await;
        assert!(!handle.is_finished(), "b must wait for a");
        drop(held);
        handle.await.unwrap().unwrap();
    }

    // ── lazy semaphore creation ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_semaphore_created_lazily_per_dir() {
        let td1 = tempdir().unwrap();
        let td2 = tempdir().unwrap();
        let (_locks, reg) = registry();
        let group = cargo_group();

        assert_eq!(reg.semaphore_count().await, 0);

        let _p1 = reg.acquire(&group, td1.path()).await.unwrap();
        assert_eq!(reg.semaphore_count().await, 1);

        let _p2 = reg.acquire(&group, td2.path()).await.unwrap();
        assert_eq!(reg.semaphore_count().await, 2);
    }
}
