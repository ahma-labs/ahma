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
        ahma_harness_tools::file_search(scopes, base_dir, pattern)
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
        ahma_harness_tools::grep_search(
            scopes,
            base_dir,
            query,
            is_regex,
            include_pattern,
            max_results,
        )
    }
}

#[async_trait::async_trait]
pub trait WebPageFetcher: Send + Sync {
    async fn fetch(&self, url: &str, query: Option<&str>) -> Result<WebFetchResult>;
}

#[derive(Debug, Clone, Default)]
pub struct DefaultWebPageFetcher;

#[async_trait::async_trait]
impl WebPageFetcher for DefaultWebPageFetcher {
    async fn fetch(&self, url: &str, query: Option<&str>) -> Result<WebFetchResult> {
        ahma_harness_tools::fetch_webpage(url, query).await
    }
}
