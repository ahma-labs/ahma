use ahma_harness_tools::{DirEntryInfo, GrepMatch, WebFetchResult};
use anyhow::Result;
use std::path::{Path, PathBuf};

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
    async fn replace_in_file(
        &self,
        scopes: &[PathBuf],
        path: &Path,
        old_str: &str,
        new_str: &str,
    ) -> Result<usize>;
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
        query: &str,
        is_regex: bool,
        include_pattern: Option<&str>,
        max_results: Option<usize>,
    ) -> Result<Vec<GrepMatch>>;
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

    async fn write_file(&self, scopes: &[PathBuf], path: &Path, content: &str) -> Result<()> {
        ahma_harness_tools::write_file(scopes, path, content).await
    }

    async fn replace_in_file(
        &self,
        scopes: &[PathBuf],
        path: &Path,
        old_str: &str,
        new_str: &str,
    ) -> Result<usize> {
        ahma_harness_tools::replace_in_file(scopes, path, old_str, new_str).await
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
        query: &str,
        is_regex: bool,
        include_pattern: Option<&str>,
        max_results: Option<usize>,
    ) -> Result<Vec<GrepMatch>> {
        // Synchronous WalkDir + per-file reads; keep it off the async workers.
        let scopes = scopes.to_vec();
        let base_dir = base_dir.to_path_buf();
        let query = query.to_string();
        let include_pattern = include_pattern.map(str::to_string);
        tokio::task::spawn_blocking(move || {
            ahma_harness_tools::grep_search(
                &scopes,
                &base_dir,
                &query,
                is_regex,
                include_pattern.as_deref(),
                max_results,
            )
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
        assert_eq!(content, "line one\nline two\nline three");
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
        assert_eq!(slice, "b\nc\nd");
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

        let count = provider
            .replace_in_file(&scopes, &file, "foo", "qux")
            .await
            .expect("replace_in_file should succeed");
        assert_eq!(count, 3);

        let updated = provider
            .read_file(&scopes, &file, None, None)
            .await
            .expect("read updated file");
        assert_eq!(updated, "qux bar qux baz qux");
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
            .replace_in_file(&scopes, &file, "missing", "x")
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

        let matches = provider
            .grep_search(&scopes, &base, "target", false, None, None)
            .await
            .expect("grep_search should succeed");

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
        let all = provider
            .grep_search(&scopes, &base, r"\d+", true, None, None)
            .await
            .expect("regex grep should succeed");
        assert_eq!(all.len(), 3, "matches: {all:?}");

        // max_results bound stops scanning early.
        let bounded = provider
            .grep_search(&scopes, &base, r"\d+", true, None, Some(2))
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
