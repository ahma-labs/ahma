//! Unit tests for harness tool argument parsing and error paths.
//!
//! These tests use mock implementations of [`FileOpsProvider`] and
//! [`WebPageFetcher`] so they run entirely in-process without touching the
//! real filesystem or network.  The goal is to raise coverage for the
//! argument-validation and error-path branches in `harness_tools.rs`.

use crate::egress::web_audit::FetchAction;
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
use rmcp::model::{
    ClientCapabilities, ClientInfo, CreateElicitationRequestParams, CreateElicitationResult,
    ElicitationAction, ElicitationCapability, FormElicitationCapability, Implementation,
};
use rmcp::service::{RequestContext, RunningService};
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::{ClientHandler, RoleClient, RoleServer, ServiceExt};
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
    let adapter =
        Arc::new(Adapter::new(Arc::clone(&monitor), shell_pool, Arc::new(sandbox)).unwrap());
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
    service
        .with_file_ops_provider(file_ops)
        .with_web_page_fetcher(web)
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
    let mock = MockFileOpsProvider {
        search_matches: vec!["src/main.rs".to_string(), "src/lib.rs".to_string()],
        ..Default::default()
    };
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
    assert!(
        text.contains("page content") || text.contains("example.com") || text.contains("Example")
    );
}

#[tokio::test]
async fn fetch_webpage_never_allow_blocks() {
    // Isolate settings to a temp home so the [web] policy is deterministic.
    // SAFETY: nextest runs each test in its own process.
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".ahma")).unwrap();
    std::fs::write(
        home.path().join(".ahma").join("settings.toml"),
        "[web]\nnever_allow = [\"blocked.example\"]\n",
    )
    .unwrap();
    unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };

    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;

    let blocked = svc
        .handle_fetch_webpage(make_args(&[("url", json!("https://blocked.example/x"))]))
        .await;
    let allowed = svc
        .handle_fetch_webpage(make_args(&[("url", json!("https://allowed.example/x"))]))
        .await;

    unsafe { std::env::remove_var("AHMA_TEST_HOME") };

    let err = blocked.expect_err("never_allow domain must be blocked");
    assert!(
        err.message.contains("web egress blocked"),
        "expected an egress-blocked message, got: {}",
        err.message
    );
    // A domain not on never_allow still passes (default policy is allow).
    assert!(allowed.is_ok(), "unlisted domain must pass in allow mode");
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
    let args = make_args(&[("old_str", json!("old")), ("new_str", json!("new"))]);
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
    let args = make_args(&[("path", json!("/tmp/test.txt")), ("new_str", json!("new"))]);
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
    let args = make_args(&[("path", json!("/tmp/test.txt")), ("old_str", json!("old"))]);
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
    let required = schema.get("required").and_then(|v| v.as_array()).unwrap();
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
    let required = schema.get("required").and_then(|v| v.as_array()).unwrap();
    assert!(required.iter().any(|v| v.as_str() == Some("pattern")));
}

#[test]
fn grep_search_schema_requires_query() {
    let schema = super::grep_search_schema();
    let required = schema.get("required").and_then(|v| v.as_array()).unwrap();
    assert!(required.iter().any(|v| v.as_str() == Some("query")));
}

#[test]
fn fetch_webpage_schema_requires_url() {
    let schema = super::fetch_webpage_schema();
    let required = schema.get("required").and_then(|v| v.as_array()).unwrap();
    assert!(required.iter().any(|v| v.as_str() == Some("url")));
}

#[test]
fn write_file_schema_requires_path_and_content() {
    let schema = super::write_file_schema();
    let required = schema.get("required").and_then(|v| v.as_array()).unwrap();
    assert!(required.iter().any(|v| v.as_str() == Some("path")));
    assert!(required.iter().any(|v| v.as_str() == Some("content")));
}

#[test]
fn replace_in_file_schema_requires_all_three() {
    let schema = super::replace_in_file_schema();
    let required = schema.get("required").and_then(|v| v.as_array()).unwrap();
    let required_strs: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
    assert!(required_strs.contains(&"path"));
    assert!(required_strs.contains(&"old_str"));
    assert!(required_strs.contains(&"new_str"));
}

// ── approve_web_egress: dedup, no-surface fail-safe, TUI-hub delivery ──────────
//
// These drive `approve_web_egress` directly (it's a private method on
// `AhmaMcpService`, reachable because this test file is a child module of
// `harness_tools.rs`), with `peer` at its default `None` — no MCP wiring
// needed for the "nothing can prompt" and "TUI hub" branches.

#[tokio::test]
async fn approve_web_egress_dedup_denies_second_concurrent_request() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    // Pre-seed an in-flight decision for the domain so the internal `begin()`
    // call inside `approve_web_egress` returns `None` (dedup).
    let _first = svc
        .web_approval
        .begin(
            "dedup.example",
            "https://dedup.example/x",
            Some("test".to_string()),
        )
        .expect("first begin should succeed");

    let action = svc
        .approve_web_egress("dedup.example", "https://dedup.example/x")
        .await;
    match action {
        FetchAction::Deny(msg) => {
            assert!(msg.contains("ahma web allow dedup.example"), "{msg}");
        }
        other => panic!("expected Deny for an in-flight duplicate, got {other:?}"),
    }
}

#[tokio::test]
async fn approve_web_egress_no_peer_no_tui_cancels_and_denies() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    // No peer (default) and no TUI sender configured: nothing can prompt.
    let action = svc
        .approve_web_egress("nosurface.example", "https://nosurface.example/x")
        .await;
    match action {
        FetchAction::Deny(msg) => {
            assert!(msg.contains("ahma web allow nosurface.example"), "{msg}");
        }
        other => panic!("expected Deny, got {other:?}"),
    }
}

#[tokio::test]
async fn approve_web_egress_tui_hub_send_success_denies_with_hint() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    svc.set_web_approval_sender(tx);

    let action = svc
        .approve_web_egress("tui.example", "https://tui.example/x")
        .await;
    match action {
        FetchAction::Deny(msg) => {
            assert!(msg.contains("ahma TUI"), "{msg}");
            assert!(msg.contains("tui.example"), "{msg}");
            assert!(msg.contains("ahma web allow tui.example"), "{msg}");
        }
        other => panic!("expected Deny with TUI hint, got {other:?}"),
    }
    let received = rx
        .try_recv()
        .expect("request should be forwarded to the TUI hub");
    assert_eq!(received.domain, "tui.example");
}

#[tokio::test]
async fn approve_web_egress_tui_hub_send_failure_falls_back_to_cancel_and_deny() {
    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    drop(rx); // No live receiver: tx.send(..) returns Err, exercising the fallback.
    svc.set_web_approval_sender(tx);

    let action = svc
        .approve_web_egress("tuidown.example", "https://tuidown.example/x")
        .await;
    match action {
        FetchAction::Deny(msg) => {
            assert!(
                !msg.contains("ahma TUI"),
                "expected the fail-safe hint (no live TUI), got: {msg}"
            );
            assert!(msg.contains("ahma web allow tuidown.example"), "{msg}");
        }
        other => panic!("expected Deny, got {other:?}"),
    }
}

// ── approve_web_egress: real in-process elicitation via a scripted MCP peer ────
//
// Wires `AhmaMcpService` to a real `rmcp::Peer<RoleServer>` over an in-memory
// `tokio::io::duplex` transport (mirrors `test_utils::in_process::wire_in_process_mcp`),
// paired with a fake client that declares the elicitation capability and answers
// `elicitation/create` with a scripted decision. This drives the
// `elicit_with_timeout` match and the `WebResolveOutcome` match in
// `approve_web_egress` end-to-end, with no mocking of rmcp internals.

/// One scripted answer to an `elicitation/create` round-trip.
enum ElicitReply {
    /// Accept with `{"decision": <str>}` content (e.g. "once"/"session"/"always"/"deny").
    Accept(String),
    /// Explicit decline (`ElicitationAction::Decline`).
    Decline,
    /// User cancelled/dismissed (`ElicitationAction::Cancel`).
    Cancel,
}

/// A `ClientHandler` that optionally advertises the elicitation capability and
/// answers each `create_elicitation` call with the next scripted reply.
struct ScriptedElicitClient {
    capable: bool,
    replies: Arc<std::sync::Mutex<std::collections::VecDeque<ElicitReply>>>,
}

impl ClientHandler for ScriptedElicitClient {
    fn get_info(&self) -> ClientInfo {
        let mut caps = ClientCapabilities::default();
        if self.capable {
            caps.elicitation = Some(ElicitationCapability {
                form: Some(FormElicitationCapability::default()),
                url: None,
            });
        }
        ClientInfo::new(caps, Implementation::default())
    }

    async fn create_elicitation(
        &self,
        _request: CreateElicitationRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<CreateElicitationResult, rmcp::ErrorData> {
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(ElicitReply::Decline);
        let (action, content) = match reply {
            ElicitReply::Accept(decision) => (
                ElicitationAction::Accept,
                Some(serde_json::json!({ "decision": decision })),
            ),
            ElicitReply::Decline => (ElicitationAction::Decline, None),
            ElicitReply::Cancel => (ElicitationAction::Cancel, None),
        };
        Ok(CreateElicitationResult {
            action,
            content,
            meta: None,
        })
    }
}

/// Wire an `AhmaMcpService` to a scripted elicitation-capable (or not) client
/// over an in-memory duplex transport. Returns the service plus both running
/// handles (which must stay alive for the background I/O loops to keep running).
async fn wire_elicit_service(
    file_ops: Arc<dyn FileOpsProvider>,
    web: Arc<dyn crate::file_ops::WebPageFetcher>,
    capable: bool,
    replies: Vec<ElicitReply>,
) -> (
    AhmaMcpService,
    RunningService<RoleClient, ScriptedElicitClient>,
    RunningService<RoleServer, AhmaMcpService>,
) {
    let svc = make_service_with(file_ops, web).await;

    let (client_stream, server_stream) = tokio::io::duplex(65536);
    let (client_read, client_write) = tokio::io::split(client_stream);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let client_transport = AsyncRwTransport::new_client(client_read, client_write);
    let server_transport = AsyncRwTransport::new_server(server_read, server_write);

    let client_handler = ScriptedElicitClient {
        capable,
        replies: Arc::new(std::sync::Mutex::new(replies.into_iter().collect())),
    };

    let (client_result, server_result) = tokio::join!(
        client_handler.serve(client_transport),
        svc.clone().serve(server_transport),
    );
    let client = client_result.expect("client handshake must succeed");
    let server = server_result.expect("server handshake must succeed");

    // `serve()` returns once `InitializeResult` is sent, not once the client's
    // `notifications/initialized` has been processed server-side (that is what
    // stores the `Peer` handle on `AhmaMcpService.peer` via `on_initialized`).
    // Poll briefly for it before handing control back to the test.
    for _ in 0..200 {
        if svc.peer.read().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        svc.peer.read().unwrap().is_some(),
        "peer handshake did not complete in time"
    );

    (svc, client, server)
}

#[tokio::test]
async fn approve_web_egress_elicit_allow_once_proceeds() {
    let (svc, _client, _server) = wire_elicit_service(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
        true,
        vec![ElicitReply::Accept("once".to_string())],
    )
    .await;

    let action = svc
        .approve_web_egress("once.example", "https://once.example/x")
        .await;
    assert_eq!(action, FetchAction::Proceed);
}

#[tokio::test]
async fn approve_web_egress_elicit_allow_session_proceeds() {
    let (svc, _client, _server) = wire_elicit_service(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
        true,
        vec![ElicitReply::Accept("session".to_string())],
    )
    .await;

    let action = svc
        .approve_web_egress("session.example", "https://session.example/x")
        .await;
    assert_eq!(action, FetchAction::Proceed);
}

#[tokio::test]
async fn approve_web_egress_elicit_allow_always_persists_and_proceeds() {
    // Isolate settings to a temp home so persistence never touches the real
    // ~/.ahma/settings.toml. SAFETY: nextest runs each test in its own process
    // (see `fetch_webpage_never_allow_blocks` above for the same pattern).
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".ahma")).unwrap();
    unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };

    let (svc, _client, _server) = wire_elicit_service(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
        true,
        vec![ElicitReply::Accept("always".to_string())],
    )
    .await;

    let action = svc
        .approve_web_egress("always.example", "https://always.example/x")
        .await;

    unsafe { std::env::remove_var("AHMA_TEST_HOME") };

    assert_eq!(action, FetchAction::Proceed);
    let saved = std::fs::read_to_string(home.path().join(".ahma").join("settings.toml"))
        .unwrap_or_default();
    assert!(
        saved.contains("always.example"),
        "expected domain persisted to [web].always_allow, got: {saved}"
    );
}

#[tokio::test]
async fn approve_web_egress_elicit_deny_answer_denies() {
    let (svc, _client, _server) = wire_elicit_service(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
        true,
        vec![ElicitReply::Accept("deny".to_string())],
    )
    .await;

    let action = svc
        .approve_web_egress("denyanswer.example", "https://denyanswer.example/x")
        .await;
    match action {
        FetchAction::Deny(msg) => {
            assert!(msg.contains("ahma web allow denyanswer.example"), "{msg}");
        }
        other => panic!("expected Deny, got {other:?}"),
    }
}

#[tokio::test]
async fn approve_web_egress_elicit_explicit_decline_denies() {
    let (svc, _client, _server) = wire_elicit_service(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
        true,
        vec![ElicitReply::Decline],
    )
    .await;

    let action = svc
        .approve_web_egress("decline.example", "https://decline.example/x")
        .await;
    match action {
        FetchAction::Deny(msg) => {
            assert!(msg.contains("ahma web allow decline.example"), "{msg}");
        }
        other => panic!("expected Deny, got {other:?}"),
    }
}

#[tokio::test]
async fn approve_web_egress_elicit_cancel_denies_without_remembering() {
    let (svc, _client, _server) = wire_elicit_service(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
        true,
        vec![ElicitReply::Cancel],
    )
    .await;

    // Cancel/timeout/transport errors take the catch-all `Err(e)` arm, which
    // cancels the in-flight decision (rather than resolving it) and denies.
    let action = svc
        .approve_web_egress("cancel.example", "https://cancel.example/x")
        .await;
    match action {
        FetchAction::Deny(msg) => {
            assert!(msg.contains("ahma web allow cancel.example"), "{msg}");
        }
        other => panic!("expected Deny, got {other:?}"),
    }
}

#[tokio::test]
async fn approve_web_egress_elicitation_unsupported_falls_back_to_tui_hub() {
    // The client is connected (a real Peer) but never declares the elicitation
    // capability, so `elicit_with_timeout` returns `CapabilityNotSupported`
    // without ever invoking `create_elicitation` on the fake client.
    let (svc, _client, _server) = wire_elicit_service(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
        false,
        vec![],
    )
    .await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    svc.set_web_approval_sender(tx);

    let action = svc
        .approve_web_egress("nocap.example", "https://nocap.example/x")
        .await;
    match action {
        FetchAction::Deny(msg) => {
            assert!(msg.contains("ahma TUI"), "{msg}");
        }
        other => panic!("expected Deny with TUI hint, got {other:?}"),
    }
    let received = rx.try_recv().expect("forwarded to the TUI hub");
    assert_eq!(received.domain, "nocap.example");
}

// ── handle_fetch_webpage: Prompt-decision wiring (strict `deny` mode) ──────────

#[tokio::test]
async fn fetch_webpage_prompt_mode_denies_without_peer() {
    // Isolate settings so the [web] policy is deterministic and never touches
    // the real ~/.ahma/settings.toml. SAFETY: nextest runs each test in its
    // own process (see `fetch_webpage_never_allow_blocks` above).
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".ahma")).unwrap();
    std::fs::write(
        home.path().join(".ahma").join("settings.toml"),
        "[web]\ndefault_policy = \"deny\"\n",
    )
    .unwrap();
    unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };

    let svc = make_service_with(
        Arc::new(MockFileOpsProvider::default()),
        Arc::new(MockWebPageFetcher::default()),
    )
    .await;

    let result = svc
        .handle_fetch_webpage(make_args(&[(
            "url",
            json!("https://unknown.example.test/x"),
        )]))
        .await;

    unsafe { std::env::remove_var("AHMA_TEST_HOME") };

    let err =
        result.expect_err("unknown domain in strict deny mode with no prompt surface must deny");
    assert!(err.message.contains("is not approved"), "{}", err.message);
    assert!(
        err.message.contains("ahma web allow unknown.example.test"),
        "{}",
        err.message
    );
}
