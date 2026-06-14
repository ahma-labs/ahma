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
//! [`CommandMutexRegistry`] gates these commands behind a per-directory semaphore
//! so at most one runs at a time **within each working directory**.  Commands in
//! different directories are entirely independent and never block each other.
//!
//! ## Configuration
//!
//! Groups are defined in `~/.ahma/settings.toml` under `[tools].mutex_groups`.
//! The default ships a single `cargo` group.  Set `mutex_groups = []` to disable
//! all gating.

use ahma_common::config::MutexGroupConfig;
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
}

impl CommandMutexRegistry {
    /// Create a registry from the provided group configurations.
    ///
    /// Pass an empty slice to disable all mutex gating.
    pub fn from_config(groups: &[MutexGroupConfig]) -> Self {
        Self {
            groups: groups.to_vec(),
            semaphores: Arc::new(RwLock::new(HashMap::new())),
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

    /// Acquire the semaphore permit for `(group.name, canonical(working_dir))`.
    ///
    /// Blocks until:
    /// * A permit becomes available (another command in the same group/dir finished), or
    /// * `group.max_wait_secs` elapses → returns [`MutexGroupError::Timeout`].
    ///
    /// The returned [`OwnedSemaphorePermit`] must be held for the entire duration
    /// of the command; dropping it releases the slot for the next queued command.
    pub async fn acquire(
        &self,
        group: &MutexGroupConfig,
        working_dir: &Path,
    ) -> Result<OwnedSemaphorePermit, MutexGroupError> {
        let canonical =
            dunce::canonicalize(working_dir).unwrap_or_else(|_| working_dir.to_path_buf());
        let key = (group.name.clone(), canonical);

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

        match tokio::time::timeout(
            std::time::Duration::from_secs(group.max_wait_secs),
            sem.acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => Ok(permit),
            Ok(Err(_)) => Err(MutexGroupError::Closed {
                group: group.name.clone(),
            }),
            Err(_) => Err(MutexGroupError::Timeout {
                group: group.name.clone(),
                secs: group.max_wait_secs,
            }),
        }
    }

    /// Returns the number of `(group, dir)` semaphores currently tracked.
    ///
    /// Primarily for tests and diagnostics.
    #[cfg(test)]
    pub async fn semaphore_count(&self) -> usize {
        self.semaphores.read().await.len()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ahma_common::config::MutexGroupConfig;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::time::{Duration, sleep};

    fn cargo_group() -> MutexGroupConfig {
        MutexGroupConfig {
            name: "cargo".to_string(),
            prefixes: vec!["cargo".to_string()],
            max_wait_secs: 5,
        }
    }

    fn registry() -> CommandMutexRegistry {
        CommandMutexRegistry::from_config(&[cargo_group()])
    }

    // ── find_group ──────────────────────────────────────────────────────────

    #[test]
    fn test_find_group_matches_cargo_prefix() {
        let reg = registry();
        assert!(reg.find_group("cargo build").is_some());
        assert!(reg.find_group("cargo nextest run --all").is_some());
        assert!(reg.find_group("CARGO check").is_some()); // case-insensitive
    }

    #[test]
    fn test_find_group_no_match_for_unrelated_command() {
        let reg = registry();
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
        let reg = Arc::new(registry());
        let group = cargo_group();

        // Acquire the permit in task A.
        let permit_a = reg.acquire(&group, td.path()).await.unwrap();

        // Task B tries to acquire — must wait.
        let reg2 = reg.clone();
        let dir = td.path().to_path_buf();
        let g2 = cargo_group();
        let handle = tokio::spawn(async move { reg2.acquire(&g2, &dir).await });

        // Give B a moment to queue up.
        sleep(Duration::from_millis(50)).await;

        // Drop A's permit — B should now succeed.
        drop(permit_a);
        let result = handle.await.unwrap();
        assert!(result.is_ok(), "task B should succeed after A releases");
    }

    // ── different-dir independence ───────────────────────────────────────────

    #[tokio::test]
    async fn test_different_dirs_run_in_parallel() {
        let td1 = tempdir().unwrap();
        let td2 = tempdir().unwrap();
        let reg = Arc::new(registry());
        let group = cargo_group();

        // Acquire permit for dir1.
        let _permit1 = reg.acquire(&group, td1.path()).await.unwrap();

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
        let reg = registry();
        let short_group = MutexGroupConfig {
            name: "cargo".to_string(),
            prefixes: vec!["cargo".to_string()],
            max_wait_secs: 0, // instant timeout
        };

        // Hold the permit.
        let _held = reg.acquire(&short_group, td.path()).await.unwrap();

        // Second acquire must time out.
        let result = reg.acquire(&short_group, td.path()).await;
        assert!(
            matches!(result, Err(MutexGroupError::Timeout { .. })),
            "expected Timeout, got: {result:?}"
        );
    }

    // ── lazy semaphore creation ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_semaphore_created_lazily_per_dir() {
        let td1 = tempdir().unwrap();
        let td2 = tempdir().unwrap();
        let reg = registry();
        let group = cargo_group();

        assert_eq!(reg.semaphore_count().await, 0);

        let _p1 = reg.acquire(&group, td1.path()).await.unwrap();
        assert_eq!(reg.semaphore_count().await, 1);

        let _p2 = reg.acquire(&group, td2.path()).await.unwrap();
        assert_eq!(reg.semaphore_count().await, 2);
    }
}
