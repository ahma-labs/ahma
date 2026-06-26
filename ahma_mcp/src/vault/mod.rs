//! # Task Vault
//!
//! A Task Vault is a per-question isolated working directory that enforces the
//! "dedicated folder per task" security principle by construction.  Every vault
//! gets its own kernel-sandbox scope, two-phase trash, and append-only audit log.
//!
//! ## Directory layout
//!
//! ```text
//! ~/.ahma/tasks/<utc>-<slug>-<hex>/
//!   inputs/       — copies of user-provided files (read intent: never modified in-place)
//!   workdir/      — sandbox scope root; agent commands run here
//!   outputs/      — artifacts produced by tools
//!   trash/        — staged deletions; permanently removed only after explicit purge
//!   audit.jsonl   — append-only JSONL audit log of all operations
//! ```
//!
//! ## Usage
//!
//! ```no_run
//! use ahma_mcp::vault::TaskVault;
//!
//! // Create a new vault for a user question
//! let vault = TaskVault::create("summarise-q4-report")?;
//! println!("Vault root: {}", vault.path().display());
//! println!("Sandbox scope: {}", vault.sandbox_scope().display());
//! # Ok::<(), anyhow::Error>(())
//! ```

pub mod audit;
pub mod rm_interceptor;
pub mod trash;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use rand::RngExt as _;

// ─────────────────────────────────────────────────────────────────────────────
// TaskVault
// ─────────────────────────────────────────────────────────────────────────────

/// A per-task isolated working directory with a kernel-enforced sandbox scope,
/// staged deletions, and an append-only audit log.
#[derive(Debug, Clone)]
pub struct TaskVault {
    /// Absolute path to the vault root directory.
    pub root: PathBuf,
    /// `<root>/inputs/` — read-only input copies.
    pub inputs: PathBuf,
    /// `<root>/workdir/` — sandbox scope for agent commands.
    pub workdir: PathBuf,
    /// `<root>/outputs/` — artifacts produced by tools.
    pub outputs: PathBuf,
    /// `<root>/trash/` — staged-deletion staging area.
    pub trash: PathBuf,
    /// `<root>/audit.jsonl` — append-only audit log.
    pub audit_log: PathBuf,
}

impl TaskVault {
    /// Create a new vault at `~/.ahma/tasks/<utc-date>-<slug>-<hex>/`.
    ///
    /// Creates the full directory tree and touches the audit log.
    pub fn create(slug: &str) -> Result<Self> {
        let base = Self::vault_base_dir()?;
        let name = Self::generate_name(slug);
        let root = base.join(name);
        Self::create_at(root)
    }

    /// Open an existing vault rooted at `root`.
    ///
    /// Returns an error if the directory does not exist.
    pub fn open(root: PathBuf) -> Result<Self> {
        anyhow::ensure!(
            root.exists(),
            "Vault directory does not exist: {}",
            root.display()
        );
        Ok(Self::from_root(root))
    }

    /// Create a vault rooted at an explicit path.
    ///
    /// Creates the full directory tree if it does not already exist.
    /// This is used by `--task-vault <path>` in serve mode.
    pub fn create_at(root: PathBuf) -> Result<Self> {
        let vault = Self::from_root(root);
        std::fs::create_dir_all(&vault.inputs)
            .with_context(|| format!("Failed to create inputs dir: {}", vault.inputs.display()))?;
        std::fs::create_dir_all(&vault.workdir)
            .with_context(|| format!("Failed to create workdir: {}", vault.workdir.display()))?;
        std::fs::create_dir_all(&vault.outputs).with_context(|| {
            format!("Failed to create outputs dir: {}", vault.outputs.display())
        })?;
        std::fs::create_dir_all(&vault.trash)
            .with_context(|| format!("Failed to create trash dir: {}", vault.trash.display()))?;
        if !vault.audit_log.exists() {
            std::fs::write(&vault.audit_log, b"").with_context(|| {
                format!("Failed to create audit log: {}", vault.audit_log.display())
            })?;
        }
        Ok(vault)
    }

    /// The sandbox scope for this vault — agent commands run in `workdir/`.
    pub fn sandbox_scope(&self) -> &Path {
        &self.workdir
    }

    /// Absolute path to this vault's root directory.
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Return a [`audit::AuditWriter`] bound to this vault's audit log.
    pub fn audit_writer(&self) -> audit::AuditWriter {
        audit::AuditWriter::new(&self.audit_log)
    }

    /// Return a [`trash::TrashManager`] bound to this vault's trash directory.
    pub fn trash_manager(&self) -> trash::TrashManager {
        trash::TrashManager::new(&self.trash)
    }

    // ── internal ─────────────────────────────────────────────────────────────

    fn from_root(root: PathBuf) -> Self {
        let inputs = root.join("inputs");
        let workdir = root.join("workdir");
        let outputs = root.join("outputs");
        let trash = root.join("trash");
        let audit_log = root.join("audit.jsonl");
        Self {
            root,
            inputs,
            workdir,
            outputs,
            trash,
            audit_log,
        }
    }

    fn vault_base_dir() -> Result<PathBuf> {
        let home = dirs::home_dir().context("Cannot determine home directory for vault storage")?;
        let base = home.join(".ahma").join("tasks");
        std::fs::create_dir_all(&base)
            .with_context(|| format!("Failed to create vault base dir: {}", base.display()))?;
        Ok(base)
    }

    fn generate_name(slug: &str) -> String {
        let now = Utc::now().format("%Y%m%dT%H%M%SZ");
        let id: u64 = rand::rng().random();
        // Sanitize slug: keep alphanumeric / hyphen / underscore; truncate to 32 chars.
        let sanitized: String = slug
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .take(32)
            .collect();
        format!("{}-{}-{:016x}", now, sanitized, id)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn vault_in_tempdir(tmp: &TempDir, slug: &str) -> TaskVault {
        let root = tmp.path().join(TaskVault::generate_name(slug));
        TaskVault::create_at(root).unwrap()
    }

    #[test]
    fn vault_creates_directory_tree() {
        let tmp = TempDir::new().unwrap();
        let vault = vault_in_tempdir(&tmp, "test-task");

        assert!(vault.inputs.is_dir(), "inputs dir must exist");
        assert!(vault.workdir.is_dir(), "workdir must exist");
        assert!(vault.outputs.is_dir(), "outputs dir must exist");
        assert!(vault.trash.is_dir(), "trash dir must exist");
        assert!(vault.audit_log.is_file(), "audit.jsonl must exist");
    }

    #[test]
    fn sandbox_scope_is_workdir() {
        let tmp = TempDir::new().unwrap();
        let vault = vault_in_tempdir(&tmp, "scope-check");
        assert_eq!(vault.sandbox_scope(), vault.workdir.as_path());
    }

    #[test]
    fn open_existing_vault() {
        let tmp = TempDir::new().unwrap();
        let vault = vault_in_tempdir(&tmp, "openable");
        let reopened = TaskVault::open(vault.root.clone()).unwrap();
        assert_eq!(reopened.path(), vault.path());
    }

    #[test]
    fn open_missing_vault_errors() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert!(TaskVault::open(missing).is_err());
    }

    #[test]
    fn generate_name_sanitizes_slug() {
        let name = TaskVault::generate_name("hello world! ../etc");
        // Must not contain spaces or slashes
        assert!(!name.contains(' '));
        assert!(!name.contains('/'));
        assert!(!name.contains('.'));
    }

    #[test]
    fn generate_name_truncates_slug_to_32_chars() {
        // Covers the `.take(32)` truncation branch (lines 158-162).
        let long = "a".repeat(50);
        let name = TaskVault::generate_name(&long);
        // Exactly 32 'a's must appear; 33 must not.
        assert!(name.contains(&"a".repeat(32)), "slug should keep 32 chars");
        assert!(
            !name.contains(&"a".repeat(33)),
            "slug must be truncated to 32 chars, got: {name}"
        );
    }

    #[test]
    fn generate_name_strips_unicode_and_keeps_ascii_word_chars() {
        // Non-ASCII letters/symbols are filtered out; ASCII alnum/-/_ kept.
        let name = TaskVault::generate_name("café-π_naïve9");
        // The sanitized portion is between the timestamp and the hex suffix.
        // It must consist only of the retained ASCII word characters.
        assert!(name.contains("caf-_nave9"), "unexpected name: {name}");
        // Multi-byte chars must be gone.
        assert!(!name.contains('é'));
        assert!(!name.contains('π'));
        assert!(!name.contains('ï'));
    }

    #[test]
    fn generate_name_empty_slug_yields_double_hyphen() {
        // Empty sanitized slug => "<ts>--<hex>".
        let name = TaskVault::generate_name("");
        assert!(name.contains("--"), "empty slug should leave '--': {name}");
    }

    #[test]
    fn generate_name_all_special_chars_yields_double_hyphen() {
        // Every char filtered out -> sanitized is empty.
        let name = TaskVault::generate_name("!@#$%^&*()/\\. ");
        assert!(
            name.contains("--"),
            "all-special slug should leave '--': {name}"
        );
        // Hex suffix is 16 lowercase hex digits.
        let hex = name.rsplit('-').next().unwrap();
        assert_eq!(hex.len(), 16, "hex suffix must be 16 chars: {name}");
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_name_has_utc_timestamp_prefix() {
        let name = TaskVault::generate_name("ts");
        // Format "%Y%m%dT%H%M%SZ" => 8 digits, 'T', 6 digits, 'Z'.
        let ts = &name[..16];
        assert_eq!(ts.len(), 16);
        assert_eq!(&ts[8..9], "T");
        assert_eq!(&ts[15..16], "Z");
        assert!(ts[..8].chars().all(|c| c.is_ascii_digit()));
        assert!(ts[9..15].chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn generate_name_is_unique_across_calls() {
        // The random hex suffix should differ between two calls.
        let a = TaskVault::generate_name("same");
        let b = TaskVault::generate_name("same");
        assert_ne!(a, b, "random suffix should make names unique");
    }

    #[test]
    fn from_root_derives_all_subpaths() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("derive-root");
        let vault = TaskVault::from_root(root.clone());
        assert_eq!(vault.root, root);
        assert_eq!(vault.inputs, root.join("inputs"));
        assert_eq!(vault.workdir, root.join("workdir"));
        assert_eq!(vault.outputs, root.join("outputs"));
        assert_eq!(vault.trash, root.join("trash"));
        assert_eq!(vault.audit_log, root.join("audit.jsonl"));
        assert_eq!(vault.path(), root.as_path());
    }

    #[test]
    fn audit_writer_bound_to_vault_audit_log() {
        let tmp = TempDir::new().unwrap();
        let vault = vault_in_tempdir(&tmp, "audit-bind");
        let writer = vault.audit_writer();
        assert_eq!(writer.path(), vault.audit_log.as_path());
    }

    #[test]
    fn trash_manager_bound_to_vault_trash_dir() {
        let tmp = TempDir::new().unwrap();
        let vault = vault_in_tempdir(&tmp, "trash-bind");
        let manager = vault.trash_manager();
        assert_eq!(manager.path(), vault.trash.as_path());
    }

    #[test]
    fn create_at_is_idempotent_and_preserves_audit_log() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("idempotent");
        let vault = TaskVault::create_at(root.clone()).unwrap();
        // Write content into the audit log to detect any overwrite.
        std::fs::write(&vault.audit_log, b"existing-entry\n").unwrap();
        // Re-create at the same root: the `if !audit_log.exists()` false branch
        // (line 100) must skip the write and preserve content.
        let again = TaskVault::create_at(root.clone()).unwrap();
        assert_eq!(again.root, vault.root);
        let contents = std::fs::read(&vault.audit_log).unwrap();
        assert_eq!(contents, b"existing-entry\n", "audit log must be preserved");
    }

    #[test]
    fn create_at_fresh_audit_log_is_empty() {
        let tmp = TempDir::new().unwrap();
        let vault = vault_in_tempdir(&tmp, "fresh-log");
        let contents = std::fs::read(&vault.audit_log).unwrap();
        assert!(contents.is_empty(), "fresh audit log must be empty");
    }

    #[test]
    fn create_at_errors_when_parent_is_a_file() {
        // Force a create_dir_all failure: make the would-be parent a regular file.
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("not-a-dir");
        std::fs::write(&file_path, b"blocker").unwrap();
        // Root nested under a file path => create_dir_all(inputs) must fail,
        // exercising the with_context error branch (lines 91-92).
        let root = file_path.join("vault");
        let err = TaskVault::create_at(root).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Failed to create"),
            "error should carry context, got: {msg}"
        );
    }
}
