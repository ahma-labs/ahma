use crate::sandbox::exec_config::{
    ExecConfigClass, classify, escape_hatch, needs_git_dir_resolution, resolve_git_dirs_async,
};
use ahma_harness_tools::edit::{Edit, EditOutcome};
use ahma_harness_tools::{DirEntryInfo, GrepOptions, GrepOutput, PatchOutcome, WebFetchResult};
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

/// Guard a write against the auto-executing-configuration classes in
/// [`crate::sandbox::exec_config`].
///
/// * [`ExecConfigClass::DenyWrite`] → `Err` with an actionable message naming the
///   file, why it is refused, and — for the two rules that have an operator
///   opt-in — the exact flag and settings key that permit it
///   ([`crate::sandbox::exec_config::escape_hatch`]). A denial with no stated way
///   out is what makes people switch the sandbox off wholesale, which is far
///   worse than a narrow, disclosed opt-in.
/// * [`ExecConfigClass::Disclose`] → `Ok(Some(`[`ExecConfigDisclosure`]`))`. The
///   write proceeds; the caller **must** surface `notice` in the tool result, and
///   the impl that lands the bytes records the durable half in the execution
///   audit log (see [`record_trust_handoff_write`]).
/// * anything else → `Ok(None)`.
///
/// # Enforcement asymmetry — stated, not papered over (SPEC R7)
///
/// ahma never silently disables enforcement, so the uneven story here is written
/// down rather than implied:
///
/// * **macOS**: the `DenyWrite` *subpath* set (git hooks under every resolved git
///   dir, `<workspace>/.ahma`) is **kernel-enforced** by Seatbelt, emitted as the
///   last filesystem word in the profile. This check is a better error message in
///   front of a real wall.
/// * **Linux**: Landlock ABI V1 is additive-allow. There is no way to carve a deny
///   hole inside an already-allowed subpath, so the `DenyWrite` set is enforced
///   **only at this application layer** and is bypassable by any command run
///   through `run_terminal_command`. That is a real gap, not a rounding error.
/// * **Windows**: AppContainer spawn isolation is still pending (SPEC R6.3), so
///   the same application-layer-only caveat applies.
/// * **Everywhere**: the venv-shaped rule (`pyvenv.cfg`, `*/bin/python*`) is
///   application-layer only on *all* platforms by choice — a kernel deny on
///   `*/bin/python*` would break a legitimate `python -m venv`, and the attack
///   this defends against is the agent authoring the fake interpreter directly
///   through a write tool.
///
/// `Disclose` is application-layer everywhere by definition: it is a warning, and
/// warnings cannot be emitted by a kernel policy.
pub async fn exec_config_write_guard(
    scopes: &[PathBuf],
    path: &Path,
) -> Result<Option<ExecConfigDisclosure>> {
    let root = workspace_root_for(scopes, path);

    // Cheap pass first: every rule except the git-directory ones is pure
    // lexical, so an ordinary source-file write costs zero syscalls.
    let mut hit = classify(path, &root, &[]);
    if hit.is_none() && needs_git_dir_resolution(path) {
        let git_dirs = resolve_git_dirs_async(&root).await;
        hit = classify(path, &root, &git_dirs);
    }

    let Some((class, reason)) = hit else {
        return Ok(None);
    };
    let shown = display_path(path, &root);
    match class {
        ExecConfigClass::DenyWrite => {
            let way_out = escape_hatch(reason).map(str::to_string).unwrap_or_else(|| {
                "There is no flag for this one — nothing legitimate writes it. If \
                 this is genuinely what you want, create the file yourself outside \
                 the agent session"
                    .to_string()
            });
            bail!(
                "Refusing to write {shown}: {reason}. ahma blocks this class of write \
                 because it hands execution to a component outside the sandbox. {way_out}."
            )
        }
        ExecConfigClass::Disclose => Ok(Some(ExecConfigDisclosure {
            notice: format!(
                "\n\n⚠ wrote {shown} — {reason}. Review it before your next \
                 editor/git operation."
            ),
            path: shown,
            trigger: reason.to_string(),
        })),
    }
}

/// A `Disclose`-tier write: allowed, but the caller owes the user a warning *and*
/// the audit log a durable record.
///
/// The two halves are deliberately separate. `notice` is the transient half — it
/// goes in the tool result and scrolls away with the conversation. `path` and
/// `trigger` are the durable half, written to the execution audit log by
/// [`record_trust_handoff_write`]. Trust-handoff attacks execute *later*, long
/// after the transcript that carried the warning is gone (SPEC R-HANDOFF), so a
/// warning nobody can go back and find is not a control.
#[derive(Debug, Clone)]
pub struct ExecConfigDisclosure {
    /// Warning line to append to the tool result.
    pub notice: String,
    /// Workspace-relative spelling of the file that was written.
    pub path: String,
    /// What will execute it, and on which trigger.
    pub trigger: String,
}

impl ExecConfigDisclosure {
    /// The warning line, consuming the disclosure.
    pub fn into_notice(self) -> String {
        self.notice
    }
}

/// Record a landed `Disclose`-tier write in the execution audit log.
///
/// Called *after* the bytes reach disk, never before: a refused or failed write
/// is not a trust handoff, and an audit log that reports handoffs that did not
/// happen is as useless as one that misses the ones that did.
pub async fn record_trust_handoff_write(
    disclosure: Option<&ExecConfigDisclosure>,
    tool_name: &str,
) {
    if let Some(d) = disclosure {
        crate::adapter::audit::record_trust_handoff(&d.path, &d.trigger, tool_name).await;
    }
}

/// Pick the workspace root to classify against: the longest scope that contains
/// `path`, falling back to the first scope, then to the path's own parent.
///
/// Longest-match matters when scopes nest (a container plus a project inside it):
/// classifying `<container>/<project>/.vscode/tasks.json` against the container
/// would still match, but `<project>/.ahma` must be judged relative to the
/// project, not the container.
fn workspace_root_for(scopes: &[PathBuf], path: &Path) -> PathBuf {
    scopes
        .iter()
        .filter(|s| path.starts_with(s))
        .max_by_key(|s| s.components().count())
        .or_else(|| scopes.first())
        .cloned()
        .or_else(|| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Workspace-relative spelling for messages, so the warning reads
/// `.vscode/tasks.json` rather than a 90-character absolute path. Falls back to
/// the path as given when it is not under the root.
fn display_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[async_trait::async_trait]
pub trait FileOpsProvider: Send + Sync {
    async fn read_file(
        &self,
        scopes: &[PathBuf],
        path: &Path,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> Result<String>;
    async fn list_dir(&self, scopes: &[PathBuf], path: &Path) -> Result<Vec<DirEntryInfo>>;
    async fn write_file(&self, scopes: &[PathBuf], path: &Path, content: &str) -> Result<()>;
    /// Replace `old_str` (which must occur once unless `replace_all`).
    async fn replace_in_file(
        &self,
        scopes: &[PathBuf],
        path: &Path,
        old_str: &str,
        new_str: &str,
        replace_all: bool,
    ) -> Result<EditOutcome>;

    /// Several edits to one file, all or nothing. The default goes through
    /// the same write guard and audit record as every other write.
    async fn multi_edit(
        &self,
        scopes: &[PathBuf],
        path: &Path,
        edits: &[Edit],
    ) -> Result<EditOutcome> {
        let disclosure = exec_config_write_guard(scopes, path).await?;
        let outcome = ahma_harness_tools::multi_edit(scopes, path, edits).await?;
        record_trust_handoff_write(disclosure.as_ref(), "multi_edit").await;
        Ok(outcome)
    }

    /// A Codex-format patch under `base_dir`. Every path it writes or removes
    /// (including a move target) passes the write guard before anything is
    /// changed; each `Disclose`-tier write is audited once it has landed.
    async fn apply_patch(
        &self,
        scopes: &[PathBuf],
        base_dir: &Path,
        patch: &str,
    ) -> Result<PatchOutcome> {
        let ops = ahma_harness_tools::edit::parse_patch(patch)?;
        let mut disclosures = Vec::new();
        for op in &ops {
            for p in op.paths() {
                let full = ahma_harness_tools::resolve_patch_path(scopes, base_dir, p)?;
                disclosures.push(exec_config_write_guard(scopes, &full).await?);
            }
        }
        let outcome = ahma_harness_tools::apply_patch(scopes, base_dir, patch).await?;
        for d in &disclosures {
            record_trust_handoff_write(d.as_ref(), "apply_patch").await;
        }
        Ok(outcome)
    }

    async fn file_search(
        &self,
        scopes: &[PathBuf],
        base_dir: &Path,
        pattern: &str,
    ) -> Result<Vec<String>>;
    async fn grep_search(
        &self,
        scopes: &[PathBuf],
        base_dir: &Path,
        opts: &GrepOptions,
    ) -> Result<GrepOutput>;
}

#[derive(Debug, Clone, Default)]
pub struct DefaultFileOpsProvider;

#[async_trait::async_trait]
impl FileOpsProvider for DefaultFileOpsProvider {
    async fn read_file(
        &self,
        scopes: &[PathBuf],
        path: &Path,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> Result<String> {
        ahma_harness_tools::read_file(scopes, path, start_line, end_line).await
    }

    async fn list_dir(&self, scopes: &[PathBuf], path: &Path) -> Result<Vec<DirEntryInfo>> {
        ahma_harness_tools::list_dir(scopes, path).await
    }

    /// Writes through the exec-config guard (see [`exec_config_write_guard`]).
    ///
    /// The guard also runs in the MCP handler, which is where the `Disclose`
    /// warning reaches the caller. It runs *here* as well because this is the
    /// impl that actually touches the disk: a `DenyWrite` refusal must not
    /// depend on which caller happened to reach the provider.
    ///
    /// The audit record is written *here* rather than in the handler for the same
    /// reason, plus one more: it is emitted after the bytes land, so the log never
    /// claims a handoff that a failed write did not actually create.
    async fn write_file(&self, scopes: &[PathBuf], path: &Path, content: &str) -> Result<()> {
        let disclosure = exec_config_write_guard(scopes, path).await?;
        ahma_harness_tools::write_file(scopes, path, content).await?;
        record_trust_handoff_write(disclosure.as_ref(), "write_file").await;
        Ok(())
    }

    async fn replace_in_file(
        &self,
        scopes: &[PathBuf],
        path: &Path,
        old_str: &str,
        new_str: &str,
        replace_all: bool,
    ) -> Result<EditOutcome> {
        let disclosure = exec_config_write_guard(scopes, path).await?;
        let outcome =
            ahma_harness_tools::replace_in_file(scopes, path, old_str, new_str, replace_all)
                .await?;
        record_trust_handoff_write(disclosure.as_ref(), "replace_in_file").await;
        Ok(outcome)
    }

    async fn file_search(
        &self,
        scopes: &[PathBuf],
        base_dir: &Path,
        pattern: &str,
    ) -> Result<Vec<String>> {
        // The underlying search is a synchronous filesystem walk; run it on
        // the blocking pool so a workspace-wide walk never stalls a tokio
        // worker thread.
        let scopes = scopes.to_vec();
        let base_dir = base_dir.to_path_buf();
        let pattern = pattern.to_string();
        tokio::task::spawn_blocking(move || {
            ahma_harness_tools::file_search(&scopes, &base_dir, &pattern)
        })
        .await?
    }

    async fn grep_search(
        &self,
        scopes: &[PathBuf],
        base_dir: &Path,
        opts: &GrepOptions,
    ) -> Result<GrepOutput> {
        // Synchronous walk + per-file reads; keep it off the async workers.
        let scopes = scopes.to_vec();
        let base_dir = base_dir.to_path_buf();
        let opts = opts.clone();
        tokio::task::spawn_blocking(move || {
            ahma_harness_tools::grep_search(&scopes, &base_dir, &opts)
        })
        .await?
    }
}

#[async_trait::async_trait]
pub trait WebPageFetcher: Send + Sync {
    async fn fetch(&self, url: &str, query: Option<&str>) -> Result<WebFetchResult>;

    /// Fetch with a cross-domain redirect guard applied (SPEC R-WEB.8): a redirect
    /// to a host the `[web]` policy would not approve is refused rather than
    /// followed. The default ignores the guard and delegates to [`Self::fetch`] —
    /// adequate for mock/test fetchers that never follow real redirects; the
    /// production [`DefaultWebPageFetcher`] overrides it to enforce the guard.
    async fn fetch_with_redirect_guard(
        &self,
        url: &str,
        query: Option<&str>,
        guard: ahma_harness_tools::egress_guard::RedirectDomainGuard,
    ) -> Result<WebFetchResult> {
        let _ = guard;
        self.fetch(url, query).await
    }
}

#[derive(Debug, Clone, Default)]
pub struct DefaultWebPageFetcher;

#[async_trait::async_trait]
impl WebPageFetcher for DefaultWebPageFetcher {
    async fn fetch(&self, url: &str, query: Option<&str>) -> Result<WebFetchResult> {
        ahma_harness_tools::fetch_webpage(url, query).await
    }

    async fn fetch_with_redirect_guard(
        &self,
        url: &str,
        query: Option<&str>,
        guard: ahma_harness_tools::egress_guard::RedirectDomainGuard,
    ) -> Result<WebFetchResult> {
        ahma_harness_tools::fetch_webpage_with_redirect_guard(url, query, guard).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Canonicalize the tempdir so its path matches what the underlying
    /// harness produces after canonicalizing scopes (on macOS `/var` ->
    /// `/private/var`, on Windows verbatim-prefix normalization, etc.).
    fn scope_dir() -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("create tempdir");
        let canonical = dunce::canonicalize(dir.path()).expect("canonicalize tempdir");
        (dir, canonical)
    }

    #[tokio::test]
    async fn write_then_read_round_trips_content() {
        let (_dir, base) = scope_dir();
        let scopes = vec![base.clone()];
        let provider = DefaultFileOpsProvider;
        let file = base.join("hello.txt");

        provider
            .write_file(&scopes, &file, "line one\nline two\nline three")
            .await
            .expect("write_file should succeed within scope");

        let content = provider
            .read_file(&scopes, &file, None, None)
            .await
            .expect("read_file should succeed");
        assert_eq!(
            content,
            "     1\tline one\n     2\tline two\n     3\tline three"
        );
    }

    #[tokio::test]
    async fn read_file_honors_start_and_end_line() {
        let (_dir, base) = scope_dir();
        let scopes = vec![base.clone()];
        let provider = DefaultFileOpsProvider;
        let file = base.join("ranged.txt");

        provider
            .write_file(&scopes, &file, "a\nb\nc\nd\ne")
            .await
            .expect("write_file should succeed");

        // Lines are 1-indexed; request lines 2..=4 inclusive.
        let slice = provider
            .read_file(&scopes, &file, Some(2), Some(4))
            .await
            .expect("read_file slice should succeed");
        assert_eq!(
            slice,
            "     2\tb\n     3\tc\n     4\td\n… 1 more lines (read on with start_line=5)"
        );
    }

    #[tokio::test]
    async fn list_dir_returns_created_entries() {
        let (_dir, base) = scope_dir();
        let scopes = vec![base.clone()];
        let provider = DefaultFileOpsProvider;

        provider
            .write_file(&scopes, &base.join("alpha.txt"), "alpha")
            .await
            .expect("write alpha");
        provider
            .write_file(&scopes, &base.join("beta.txt"), "beta-body")
            .await
            .expect("write beta");

        let entries = provider
            .list_dir(&scopes, &base)
            .await
            .expect("list_dir should succeed");

        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"alpha.txt"), "names: {names:?}");
        assert!(names.contains(&"beta.txt"), "names: {names:?}");

        let beta = entries
            .iter()
            .find(|e| e.name == "beta.txt")
            .expect("beta entry present");
        assert!(!beta.is_dir);
        assert_eq!(beta.size_bytes, "beta-body".len() as u64);
    }

    #[tokio::test]
    async fn replace_in_file_reports_count_and_updates_content() {
        let (_dir, base) = scope_dir();
        let scopes = vec![base.clone()];
        let provider = DefaultFileOpsProvider;
        let file = base.join("replace.txt");

        provider
            .write_file(&scopes, &file, "foo bar foo baz foo")
            .await
            .expect("write_file should succeed");

        let outcome = provider
            .replace_in_file(&scopes, &file, "foo", "qux", true)
            .await
            .expect("replace_in_file should succeed");
        assert_eq!(outcome.replacements, 3);

        let updated = provider
            .read_file(&scopes, &file, None, None)
            .await
            .expect("read updated file");
        assert_eq!(updated, "     1\tqux bar qux baz qux");
    }

    #[tokio::test]
    async fn replace_in_file_errors_when_string_absent() {
        let (_dir, base) = scope_dir();
        let scopes = vec![base.clone()];
        let provider = DefaultFileOpsProvider;
        let file = base.join("absent.txt");

        provider
            .write_file(&scopes, &file, "nothing to see here")
            .await
            .expect("write_file should succeed");

        let result = provider
            .replace_in_file(&scopes, &file, "missing", "x", false)
            .await;
        assert!(result.is_err(), "expected error when old_str not present");
    }

    #[tokio::test]
    async fn file_search_finds_matching_file_by_glob() {
        let (_dir, base) = scope_dir();
        let scopes = vec![base.clone()];
        let provider = DefaultFileOpsProvider;

        provider
            .write_file(&scopes, &base.join("needle.log"), "x")
            .await
            .expect("write needle");
        provider
            .write_file(&scopes, &base.join("other.txt"), "y")
            .await
            .expect("write other");

        let hits = provider
            .file_search(&scopes, &base, "*.log")
            .await
            .expect("file_search should succeed");

        assert_eq!(hits.len(), 1, "hits: {hits:?}");
        assert!(
            hits[0].ends_with("needle.log"),
            "expected needle.log, got {hits:?}"
        );
    }

    #[tokio::test]
    async fn grep_search_finds_plain_text_match() {
        let (_dir, base) = scope_dir();
        let scopes = vec![base.clone()];
        let provider = DefaultFileOpsProvider;
        let file = base.join("grep.txt");

        provider
            .write_file(&scopes, &file, "first line\nTARGET here\nlast line")
            .await
            .expect("write grep file");

        let opts = ahma_harness_tools::GrepOptions {
            query: "target".into(),
            ..Default::default()
        };
        let GrepOutput::Matches(matches) = provider
            .grep_search(&scopes, &base, &opts)
            .await
            .expect("grep_search should succeed")
        else {
            panic!("content mode");
        };

        assert_eq!(matches.len(), 1, "matches: {matches:?}");
        assert_eq!(matches[0].line_number, 2);
        assert_eq!(matches[0].line, "TARGET here");
        assert!(matches[0].path.ends_with("grep.txt"));
    }

    #[tokio::test]
    async fn grep_search_supports_regex_and_max_results_bound() {
        let (_dir, base) = scope_dir();
        let scopes = vec![base.clone()];
        let provider = DefaultFileOpsProvider;
        let file = base.join("regex.txt");

        provider
            .write_file(&scopes, &file, "abc123\nno digits\nxyz789\nq42q")
            .await
            .expect("write regex file");

        // Regex matches the three lines that contain digit runs.
        let mut opts = ahma_harness_tools::GrepOptions {
            query: r"\d+".into(),
            is_regex: true,
            ..Default::default()
        };
        let all = provider
            .grep_search(&scopes, &base, &opts)
            .await
            .expect("regex grep should succeed");
        assert_eq!(all.len(), 3, "matches: {all:?}");

        // max_results bound stops scanning early.
        opts.max_results = Some(2);
        let bounded = provider
            .grep_search(&scopes, &base, &opts)
            .await
            .expect("bounded grep should succeed");
        assert_eq!(bounded.len(), 2, "bounded: {bounded:?}");
    }

    #[tokio::test]
    async fn read_file_outside_scope_is_rejected() {
        let (_dir, base) = scope_dir();
        // A second, unrelated tempdir that is NOT in the scopes allowlist.
        let (_outside_dir, outside) = scope_dir();
        let scopes = vec![base.clone()];
        let provider = DefaultFileOpsProvider;
        let target = outside.join("secret.txt");
        std::fs::write(&target, "secret").expect("create out-of-scope file");

        let result = provider.read_file(&scopes, &target, None, None).await;
        assert!(result.is_err(), "out-of-scope read must be rejected");
    }

    #[tokio::test]
    async fn ordinary_writes_are_not_flagged() {
        let (_dir, base) = scope_dir();
        let scopes = vec![base.clone()];
        assert!(
            exec_config_write_guard(&scopes, &base.join("src/main.rs"))
                .await
                .expect("plain source write must be allowed")
                .is_none()
        );
    }

    #[tokio::test]
    async fn writing_a_git_hook_is_refused_with_an_actionable_message() {
        let (_dir, base) = scope_dir();
        let scopes = vec![base.clone()];
        tokio::fs::create_dir_all(base.join(".git/hooks"))
            .await
            .unwrap();

        let err = exec_config_write_guard(&scopes, &base.join(".git/hooks/post-checkout"))
            .await
            .expect_err("git hook write must be refused");
        let msg = err.to_string();
        assert!(msg.contains("post-checkout"), "must name the file: {msg}");
        assert!(
            msg.contains("git operation"),
            "must say why it is refused: {msg}"
        );
        // The single most important part of the message: how to permit it. A
        // denial that leaves the reader guessing is what makes people reach for
        // `--no-sandbox` instead.
        assert!(
            msg.contains("--allow-git-hooks"),
            "must name the CLI flag: {msg}"
        );
        assert!(
            msg.contains("allow_git_hooks = true"),
            "must name the settings key: {msg}"
        );
    }

    /// End-to-end through the real provider: the refusal must happen *before* the
    /// bytes land, not after.
    #[tokio::test]
    async fn provider_refuses_a_git_hook_and_writes_nothing() {
        let (_dir, base) = scope_dir();
        let scopes = vec![base.clone()];
        tokio::fs::create_dir_all(base.join(".git/hooks"))
            .await
            .unwrap();
        let hook = base.join(".git/hooks/pre-commit");

        let result = DefaultFileOpsProvider
            .write_file(&scopes, &hook, "exfiltrate\n")
            .await;
        assert!(result.is_err(), "provider must refuse the hook write");
        assert!(!hook.exists(), "no bytes may reach disk on a refusal");
    }

    #[tokio::test]
    async fn ahma_tool_config_write_is_refused() {
        let (_dir, base) = scope_dir();
        let scopes = vec![base.clone()];
        let err = exec_config_write_guard(&scopes, &base.join(".ahma/tools/evil.json"))
            .await
            .expect_err(".ahma writes must be refused");
        let msg = err.to_string();
        assert!(msg.contains("MTDF"), "{msg}");
        assert!(
            msg.contains("--allow-project-tool-config"),
            "must name the CLI flag: {msg}"
        );
        assert!(
            msg.contains("allow_project_tool_config = true"),
            "must name the settings key: {msg}"
        );
    }

    /// A denial with no operator opt-in must say so plainly rather than dangle a
    /// flag that does not exist.
    #[tokio::test]
    async fn a_refusal_without_an_opt_in_says_there_is_no_flag() {
        let (_dir, base) = scope_dir();
        let scopes = vec![base.clone()];
        let err = exec_config_write_guard(&scopes, &base.join("venv/bin/python"))
            .await
            .expect_err("fake interpreter must be refused");
        let msg = err.to_string();
        assert!(msg.contains("no flag for this one"), "{msg}");
        assert!(!msg.contains("--allow-"), "must not invent a flag: {msg}");
    }

    #[tokio::test]
    async fn fake_python_interpreter_write_is_refused() {
        let (_dir, base) = scope_dir();
        let scopes = vec![base.clone()];
        for rel in ["venv/bin/python3", "pyvenv.cfg"] {
            assert!(
                exec_config_write_guard(&scopes, &base.join(rel))
                    .await
                    .is_err(),
                "{rel} must be refused"
            );
        }
    }

    #[tokio::test]
    async fn vscode_tasks_write_succeeds_but_discloses() {
        let (_dir, base) = scope_dir();
        let scopes = vec![base.clone()];
        let target = base.join(".vscode/tasks.json");

        let disclosure = exec_config_write_guard(&scopes, &target)
            .await
            .expect("disclose must not block the write")
            .expect("a warning must be produced");
        let notice = &disclosure.notice;
        assert!(notice.contains('⚠'), "warning must be visible: {notice}");
        assert!(
            notice.contains(".vscode") && notice.contains("tasks.json"),
            "warning must name the file: {notice}"
        );
        assert!(
            notice.contains("folderOpen"),
            "warning must say what auto-executes: {notice}"
        );
        // The structured half the audit log records must carry the same two facts
        // the warning does — the file, and what will execute it.
        assert!(
            disclosure.path.contains("tasks.json"),
            "audit path: {}",
            disclosure.path
        );
        assert!(
            disclosure.trigger.contains("folderOpen"),
            "audit trigger: {}",
            disclosure.trigger
        );

        // …and the write really does go through (the underlying writer requires
        // an existing parent, which is orthogonal to this guard).
        tokio::fs::create_dir_all(base.join(".vscode"))
            .await
            .unwrap();
        DefaultFileOpsProvider
            .write_file(&scopes, &target, "{}")
            .await
            .expect("a disclosed write must still succeed");
        assert!(target.exists());
    }

    #[tokio::test]
    async fn git_config_write_is_disclosed_not_blocked() {
        let (_dir, base) = scope_dir();
        let scopes = vec![base.clone()];
        tokio::fs::create_dir_all(base.join(".git")).await.unwrap();

        let disclosure = exec_config_write_guard(&scopes, &base.join(".git/config"))
            .await
            .expect("`git config user.email` must keep working")
            .expect("but it must warn");
        assert!(
            disclosure.notice.contains("core.hooksPath"),
            "{}",
            disclosure.notice
        );
    }

    #[test]
    fn workspace_root_prefers_the_longest_containing_scope() {
        use crate::test_utils::path_helpers::test_abs;
        let container = test_abs(&["container"]);
        let project = test_abs(&["container", "project"]);
        let target = project.join(".ahma/x.json");
        let root = workspace_root_for(&[container, project.clone()], &target);
        assert_eq!(
            root, project,
            "a nested project's .ahma must be judged against the project"
        );
    }

    #[test]
    fn web_page_fetcher_constructs_via_default() {
        // `fetch` performs real network I/O, so it is intentionally not
        // exercised here; we cover construction of the default fetcher.
        let fetcher = DefaultWebPageFetcher;
        let cloned = fetcher.clone();
        // Confirm the Debug impl is wired up (exercises derive output).
        assert!(format!("{cloned:?}").contains("DefaultWebPageFetcher"));

        // The provider is also Default/Clone/Debug.
        let provider = DefaultFileOpsProvider;
        let provider2 = provider.clone();
        assert!(format!("{provider2:?}").contains("DefaultFileOpsProvider"));
    }
}
