//! Unit tests for harness tool argument parsing and error paths.
//!
//! These tests use mock implementations of [`FileOpsProvider`] and
//! [`WebPageFetcher`] so they run entirely in-process without touching the
//! real filesystem or network.  The goal is to raise coverage for the
//! argument-validation and error-path branches in `harness_tools.rs`.

use crate::file_ops::{FileOpsProvider, WebPageFetcher};
use crate::mcp_service::{AhmaMcpService, GuidanceConfig};
use crate::operation_monitor::{MonitorConfig, OperationMonitor};
use crate::sandbox::{Sandbox, SandboxMode};
use crate::{
    adapter::Adapter,
    shell_pool::{ShellPoolConfig, ShellPoolManager},
};
use ahma_harness_tools::{DirEntryInfo, GrepMatch, WebFetchResult};
use anyhow::Result;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ── Mock providers ────────────────────────────────────────────────────────────

/// A mock that always succeeds with configurable string output.
struct MockFileOpsProvider {
    read_content: String,
    list_entries: Vec<DirEntryInfo>,
    search_matches: Vec<String>,
    grep_matches: Vec<GrepMatch>,
    replace_count: usize,
}

impl Default for MockFileOpsProvider {
    fn default() -> Self {
        Self {
            read_content: "mock file content".to_string(),
            list_entries: vec![],
            search_matches: vec![],
            grep_matches: vec![],
            replace_count: 1,
        }
    }
}

#[async_trait::async_trait]
impl FileOpsProvider for MockFileOpsProvider {
    async fn read_file(
        &self,
        _scopes: &[PathBuf],
        _path: &Path,
        _start_line: Option<usize>,
        _end_line: Option<usize>,
    ) -> Result<String> {
        Ok(self.read_content.clone())
    }

    async fn list_dir(&self, _scopes: &[PathBuf], _path: &Path) -> Result<Vec<DirEntryInfo>> {
        Ok(self.list_entries.clone())
    }

    async fn write_file(&self, _scopes: &[PathBuf], _path: &Path, _content: &str) -> Result<()> {
        Ok(())
    }

    async fn replace_in_file(
        &self,
        _scopes: &[PathBuf],
        _path: &Path,
        _old_str: &str,
        _new_str: &str,
    ) -> Result<usize> {
        Ok(self.replace_count)
    }

    async fn file_search(
        &self,
        _scopes: &[PathBuf],
        _base_dir: &Path,
        _pattern: &str,
    ) -> Result<Vec<String>> {
        Ok(self.search_matches.clone())
    }

    async fn grep_search(
        &self,
        _scopes: &[PathBuf],
        _base_dir: &Path,
        _query: &str,
        _is_regex: bool,
        _include_pattern: Option<&str>,
        _max_results: Option<usize>,
    ) -> Result<Vec<GrepMatch>> {
        Ok(self.grep_matches.clone())
    }
}

/// A mock that always errors.
struct FailingFileOpsProvider;

#[async_trait::async_trait]
impl FileOpsProvider for FailingFileOpsProvider {
    async fn read_file(
        &self,
        _scopes: &[PathBuf],
        _path: &Path,
        _start_line: Option<usize>,
        _end_line: Option<usize>,
    ) -> Result<String> {
        Err(anyhow::anyhow!("read_file failed"))
    }

    async fn list_dir(&self, _scopes: &[PathBuf], _path: &Path) -> Result<Vec<DirEntryInfo>> {
        Err(anyhow::anyhow!("list_dir failed"))
    }

    async fn write_file(&self, _scopes: &[PathBuf], _path: &Path, _content: &str) -> Result<()> {
        Err(anyhow::anyhow!("write_file failed"))
    }

    async fn replace_in_file(
        &self,
        _scopes: &[PathBuf],
        _path: &Path,
        _old_str: &str,
        _new_str: &str,
    ) -> Result<usize> {
        Err(anyhow::anyhow!("replace_in_file failed"))
    }

    async fn file_search(
        &self,
        _scopes: &[PathBuf],
        _base_dir: &Path,
        _pattern: &str,
    ) -> Result<Vec<String>> {
        Err(anyhow::anyhow!("file_search failed"))
    }

    async fn grep_search(
        &self,
        _scopes: &[PathBuf],
        _base_dir: &Path,
        _query: &str,
        _is_regex: bool,
        _include_pattern: Option<&str>,
        _max_results: Option<usize>,
    ) -> Result<Vec<GrepMatch>> {
        Err(anyhow::anyhow!("grep_search failed"))
    }
}

struct MockWebPageFetcher {
    result: WebFetchResult,
}

impl Default for MockWebPageFetcher {
    fn default() -> Self {
        Self {
            result: WebFetchResult {
                url: "https://example.com".to_string(),
                title: Some("Example".to_string()),
                text: "page content".to_string(),
            },
        }
    }
}

#[async_trait::async_trait]
impl WebPageFetcher for MockWebPageFetcher {
    async fn fetch(&self, _url: &str, _query: Option<&str>) -> Result<WebFetchResult> {
        Ok(self.result.clone())
    }
}

struct FailingWebPageFetcher;

#[async_trait::async_trait]
impl WebPageFetcher for FailingWebPageFetcher {
    async fn fetch(&self, _url: &str, _query: Option<&str>) -> Result<WebFetchResult> {
        Err(anyhow::anyhow!("fetch failed"))
    }
}

// ── Test helper ───────────────────────────────────────────────────────────────

async fn make_service_with(
    file_ops: Arc<dyn FileOpsProvider>,
    web: Arc<dyn crate::file_ops::WebPageFetcher>,
) -> AhmaMcpService {
    let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
        std::time::Duration::from_secs(30),
    )));
    let shell_pool = Arc::new(ShellPoolManager::new(ShellPoolConfig::default()));
    let sandbox = Sandbox::new(
        vec![std::env::current_dir().unwrap()],
        SandboxMode::Test,
        false,
        false,
        false,
    )
    .unwrap();
    let adapter = Arc::new(
        Adapter::new(Arc::clone(&monitor), shell_pool, Arc::new(sandbox)).unwrap(),
    );
    let service = AhmaMcpService::new(
        adapter,
        monitor,
        Arc::new(std::collections::HashMap::new()),
        Arc::new(None::<GuidanceConfig>),
        false,
        false,
    )
    .await
    .unwrap();
    service.with_file_ops_provider(file_ops).with_web_page_fetcher(web)
}

fn make_args(pairs: &[(&str, Value)]) -> Map<String, Value> {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), v.clone());
    }
    m
}

// ── handle_read_file ─────────────────────────────────────────────────────────

#[tokio::test]
async fn read_file_missing_path_returns_error() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[]);
    let err = svc.handle_read_file(args).await.unwrap_err();
    assert_eq!(err.code.0, -32602, "'path' required → invalid_params");
}

#[tokio::test]
async fn read_file_success_returns_content() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[("path", json!("/tmp/test.txt"))]);
    let result = svc.handle_read_file(args).await.unwrap();
    let text = result.content[0].as_text().unwrap().text.as_str();
    assert_eq!(text, "mock file content");
}

#[tokio::test]
async fn read_file_with_line_range() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[
        ("path", json!("/tmp/test.txt")),
        ("start_line", json!(1u64)),
        ("end_line", json!(10u64)),
    ]);
    // The mock always returns the same content, so we just verify no error
    let result = svc.handle_read_file(args).await.unwrap();
    assert!(!result.content.is_empty());
}

#[tokio::test]
async fn read_file_provider_error_becomes_mcp_error() {
    let svc = make_service_with(
        Arc::new(FailingFileOpsProvider),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[("path", json!("/tmp/test.txt"))]);
    let err = svc.handle_read_file(args).await.unwrap_err();
    // Internal error code is -32603
    assert_eq!(err.code.0, -32603);
}

// ── handle_list_dir ──────────────────────────────────────────────────────────

#[tokio::test]
async fn list_dir_defaults_to_dot_when_no_path() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    // No "path" key → defaults to "."
    let args = make_args(&[]);
    let result = svc.handle_list_dir(args).await.unwrap();
    // Empty list from mock → serializes to "[]"
    let text = result.content[0].as_text().unwrap().text.as_str();
    assert_eq!(text.trim(), "[]");
}

#[tokio::test]
async fn list_dir_with_explicit_path() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[("path", json!("/tmp"))]);
    let result = svc.handle_list_dir(args).await.unwrap();
    assert!(!result.content.is_empty());
}

#[tokio::test]
async fn list_dir_provider_error_becomes_mcp_error() {
    let svc = make_service_with(
        Arc::new(FailingFileOpsProvider),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[("path", json!("/tmp"))]);
    let err = svc.handle_list_dir(args).await.unwrap_err();
    assert_eq!(err.code.0, -32603);
}

// ── handle_file_search ───────────────────────────────────────────────────────

#[tokio::test]
async fn file_search_missing_pattern_returns_error() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[]);
    let err = svc.handle_file_search(args).await.unwrap_err();
    assert_eq!(err.code.0, -32602);
}

#[tokio::test]
async fn file_search_success() {
    let mut mock = MockFileOpsProvider::default();
    mock.search_matches = vec!["src/main.rs".to_string(), "src/lib.rs".to_string()];
    let svc = make_service_with(Arc::new(mock), Arc::new(MockWebPageFetcher::default())).await;
    let args = make_args(&[("pattern", json!("**/*.rs"))]);
    let result = svc.handle_file_search(args).await.unwrap();
    let text = result.content[0].as_text().unwrap().text.as_str();
    assert!(text.contains("main.rs"));
}

#[tokio::test]
async fn file_search_with_explicit_base_dir() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[
        ("pattern", json!("*.rs")),
        ("base_dir", json!("/tmp/project")),
    ]);
    let result = svc.handle_file_search(args).await.unwrap();
    assert!(!result.content.is_empty());
}

#[tokio::test]
async fn file_search_provider_error_becomes_mcp_error() {
    let svc = make_service_with(
        Arc::new(FailingFileOpsProvider),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[("pattern", json!("*.rs"))]);
    let err = svc.handle_file_search(args).await.unwrap_err();
    assert_eq!(err.code.0, -32603);
}

// ── handle_grep_search ───────────────────────────────────────────────────────

#[tokio::test]
async fn grep_search_missing_query_returns_error() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[]);
    let err = svc.handle_grep_search(args).await.unwrap_err();
    assert_eq!(err.code.0, -32602);
}

#[tokio::test]
async fn grep_search_success_with_defaults() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[("query", json!("fn main"))]);
    let result = svc.handle_grep_search(args).await.unwrap();
    assert!(!result.content.is_empty());
}

#[tokio::test]
async fn grep_search_with_all_options() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[
        ("query", json!("fn main")),
        ("is_regex", json!(true)),
        ("base_dir", json!("/tmp")),
        ("include_pattern", json!("*.rs")),
        ("max_results", json!(10u64)),
    ]);
    let result = svc.handle_grep_search(args).await.unwrap();
    assert!(!result.content.is_empty());
}

#[tokio::test]
async fn grep_search_provider_error_becomes_mcp_error() {
    let svc = make_service_with(
        Arc::new(FailingFileOpsProvider),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[("query", json!("fn main"))]);
    let err = svc.handle_grep_search(args).await.unwrap_err();
    assert_eq!(err.code.0, -32603);
}

// ── handle_fetch_webpage ─────────────────────────────────────────────────────

#[tokio::test]
async fn fetch_webpage_missing_url_returns_error() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[]);
    let err = svc.handle_fetch_webpage(args).await.unwrap_err();
    assert_eq!(err.code.0, -32602);
}

#[tokio::test]
async fn fetch_webpage_success() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[("url", json!("https://example.com"))]);
    let result = svc.handle_fetch_webpage(args).await.unwrap();
    let text = result.content[0].as_text().unwrap().text.as_str();
    assert!(text.contains("page content") || text.contains("example.com") || text.contains("Example"));
}

#[tokio::test]
async fn fetch_webpage_with_optional_query() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[
        ("url", json!("https://example.com")),
        ("query", json!("rust programming")),
    ]);
    let result = svc.handle_fetch_webpage(args).await.unwrap();
    assert!(!result.content.is_empty());
}

#[tokio::test]
async fn fetch_webpage_provider_error_becomes_mcp_error() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(FailingWebPageFetcher),
    )
    .await;
    let args = make_args(&[("url", json!("https://example.com"))]);
    let err = svc.handle_fetch_webpage(args).await.unwrap_err();
    assert_eq!(err.code.0, -32603);
}

// ── handle_write_file ────────────────────────────────────────────────────────

#[tokio::test]
async fn write_file_missing_path_returns_error() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[("content", json!("hello"))]);
    let err = svc.handle_write_file(args).await.unwrap_err();
    assert_eq!(err.code.0, -32602);
}

#[tokio::test]
async fn write_file_missing_content_returns_error() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[("path", json!("/tmp/test.txt"))]);
    let err = svc.handle_write_file(args).await.unwrap_err();
    assert_eq!(err.code.0, -32602);
}

#[tokio::test]
async fn write_file_success() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[
        ("path", json!("/tmp/test.txt")),
        ("content", json!("hello world")),
    ]);
    let result = svc.handle_write_file(args).await.unwrap();
    let text = result.content[0].as_text().unwrap().text.as_str();
    assert_eq!(text, "File written");
}

#[tokio::test]
async fn write_file_provider_error_becomes_mcp_error() {
    let svc = make_service_with(
        Arc::new(FailingFileOpsProvider),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[
        ("path", json!("/tmp/test.txt")),
        ("content", json!("hello")),
    ]);
    let err = svc.handle_write_file(args).await.unwrap_err();
    assert_eq!(err.code.0, -32603);
}

// ── handle_replace_in_file ───────────────────────────────────────────────────

#[tokio::test]
async fn replace_in_file_missing_path_returns_error() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[
        ("old_str", json!("old")),
        ("new_str", json!("new")),
    ]);
    let err = svc.handle_replace_in_file(args).await.unwrap_err();
    assert_eq!(err.code.0, -32602);
}

#[tokio::test]
async fn replace_in_file_missing_old_str_returns_error() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[
        ("path", json!("/tmp/test.txt")),
        ("new_str", json!("new")),
    ]);
    let err = svc.handle_replace_in_file(args).await.unwrap_err();
    assert_eq!(err.code.0, -32602);
}

#[tokio::test]
async fn replace_in_file_missing_new_str_returns_error() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[
        ("path", json!("/tmp/test.txt")),
        ("old_str", json!("old")),
    ]);
    let err = svc.handle_replace_in_file(args).await.unwrap_err();
    assert_eq!(err.code.0, -32602);
}

#[tokio::test]
async fn replace_in_file_success() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[
        ("path", json!("/tmp/test.txt")),
        ("old_str", json!("old")),
        ("new_str", json!("new")),
    ]);
    let result = svc.handle_replace_in_file(args).await.unwrap();
    let text = result.content[0].as_text().unwrap().text.as_str();
    assert!(text.contains("1"), "mock returns 1 replacement");
}

#[tokio::test]
async fn replace_in_file_provider_error_becomes_mcp_error() {
    let svc = make_service_with(
        Arc::new(FailingFileOpsProvider),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let args = make_args(&[
        ("path", json!("/tmp/test.txt")),
        ("old_str", json!("old")),
        ("new_str", json!("new")),
    ]);
    let err = svc.handle_replace_in_file(args).await.unwrap_err();
    assert_eq!(err.code.0, -32603);
}

// ── Schema functions (pure, no I/O) ─────────────────────────────────────────

#[test]
fn read_file_schema_has_required_path() {
    let schema = super::read_file_schema();
    let required = schema
        .get("required")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(required.iter().any(|v| v.as_str() == Some("path")));
}

#[test]
fn list_dir_schema_has_no_required_fields() {
    let schema = super::list_dir_schema();
    // list_dir makes "path" optional
    let required = schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(required, 0, "list_dir has no required fields");
}

#[test]
fn file_search_schema_requires_pattern() {
    let schema = super::file_search_schema();
    let required = schema
        .get("required")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(required.iter().any(|v| v.as_str() == Some("pattern")));
}

#[test]
fn grep_search_schema_requires_query() {
    let schema = super::grep_search_schema();
    let required = schema
        .get("required")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(required.iter().any(|v| v.as_str() == Some("query")));
}

#[test]
fn fetch_webpage_schema_requires_url() {
    let schema = super::fetch_webpage_schema();
    let required = schema
        .get("required")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(required.iter().any(|v| v.as_str() == Some("url")));
}

#[test]
fn write_file_schema_requires_path_and_content() {
    let schema = super::write_file_schema();
    let required = schema
        .get("required")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(required.iter().any(|v| v.as_str() == Some("path")));
    assert!(required.iter().any(|v| v.as_str() == Some("content")));
}

#[test]
fn replace_in_file_schema_requires_all_three() {
    let schema = super::replace_in_file_schema();
    let required = schema
        .get("required")
        .and_then(|v| v.as_array())
        .unwrap();
    let required_strs: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
    assert!(required_strs.contains(&"path"));
    assert!(required_strs.contains(&"old_str"));
    assert!(required_strs.contains(&"new_str"));
}
