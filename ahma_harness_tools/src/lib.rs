//! # Ahma Harness Tools
//!
//! Scope-validated file I/O, glob and regex search, and web-page fetch
//! utilities that back the sandboxed MCP harness tools in the ahma workspace.
//!
//! Every operation that touches the filesystem accepts a `scopes: &[PathBuf]`
//! allowlist.  Paths that canonicalize outside every listed scope are rejected
//! with an error, so callers can enforce the same confinement boundaries as the
//! kernel sandbox without duplicating the validation logic.
//!
//! ## Public API
//!
//! | Function | Description |
//! |----------|-------------|
//! | [`read_file`] | Read a file (optionally line-sliced) within scope |
//! | [`write_file`] | Create or overwrite a file within scope |
//! | [`replace_in_file`] | In-place string substitution within scope |
//! | [`list_dir`] | List directory entries within scope |
//! | [`file_search`] | Glob-pattern file discovery within scope |
//! | [`grep_search`] | Plain-text or regex line search within scope |
//! | [`fetch_webpage`] | Fetch a URL and render its HTML as plain text |

use anyhow::{Context, Result, anyhow};
use regex::Regex;
use serde::Serialize;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub mod egress_guard;

#[derive(Debug, Clone, Serialize)]
pub struct DirEntryInfo {
    pub name: String,
    pub is_dir: bool,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GrepMatch {
    pub path: String,
    pub line_number: usize,
    pub line: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WebFetchResult {
    pub url: String,
    pub title: Option<String>,
    pub text: String,
}

fn validate_path_in_scopes_sync(path: &Path, scopes: &[PathBuf]) -> Result<PathBuf> {
    let canonical = if path.exists() {
        std::fs::canonicalize(path)
            .with_context(|| format!("Failed to canonicalize path: {}", path.display()))?
    } else {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("Path has no parent: {}", path.display()))?;
        let canonical_parent = std::fs::canonicalize(parent)
            .with_context(|| format!("Failed to canonicalize parent path: {}", parent.display()))?;
        canonical_parent.join(path.file_name().unwrap_or_default())
    };

    if scopes.is_empty() {
        return Ok(canonical);
    }

    let allowed = scopes.iter().any(|s| {
        std::fs::canonicalize(s)
            .map(|scope| canonical.starts_with(scope))
            .unwrap_or(false)
    });

    if !allowed {
        return Err(anyhow!(
            "Path '{}' is outside allowed scopes",
            canonical.display()
        ));
    }

    Ok(canonical)
}

async fn validate_path_in_scopes_async(path: &Path, scopes: &[PathBuf]) -> Result<PathBuf> {
    let canonical = if path.exists() {
        tokio::fs::canonicalize(path)
            .await
            .with_context(|| format!("Failed to canonicalize path: {}", path.display()))?
    } else {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("Path has no parent: {}", path.display()))?;
        let canonical_parent = tokio::fs::canonicalize(parent)
            .await
            .with_context(|| format!("Failed to canonicalize parent path: {}", parent.display()))?;
        canonical_parent.join(path.file_name().unwrap_or_default())
    };

    if scopes.is_empty() {
        return Ok(canonical);
    }

    let mut allowed = false;
    for s in scopes {
        if tokio::fs::canonicalize(s)
            .await
            .map(|scope| canonical.starts_with(scope))
            .unwrap_or(false)
        {
            allowed = true;
            break;
        }
    }

    if !allowed {
        return Err(anyhow!(
            "Path '{}' is outside allowed scopes",
            canonical.display()
        ));
    }

    Ok(canonical)
}

pub async fn read_file(
    scopes: &[PathBuf],
    path: &Path,
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> Result<String> {
    let safe_path = validate_path_in_scopes_async(path, scopes).await?;
    let content = tokio::fs::read_to_string(&safe_path)
        .await
        .with_context(|| format!("Failed to read file: {}", safe_path.display()))?;

    let start = start_line.unwrap_or(1).max(1);
    let end = end_line.unwrap_or(usize::MAX);

    let mut out = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let ln = idx + 1;
        if ln >= start && ln <= end {
            out.push(line.to_string());
        }
    }

    Ok(out.join("\n"))
}

pub async fn list_dir(scopes: &[PathBuf], path: &Path) -> Result<Vec<DirEntryInfo>> {
    let safe_path = validate_path_in_scopes_async(path, scopes).await?;
    let mut entries = tokio::fs::read_dir(&safe_path)
        .await
        .with_context(|| format!("Failed to read directory: {}", safe_path.display()))?;

    let mut out = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let meta = entry.metadata().await?;
        out.push(DirEntryInfo {
            name: entry.file_name().to_string_lossy().to_string(),
            is_dir: meta.is_dir(),
            size_bytes: if meta.is_file() { meta.len() } else { 0 },
        });
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub async fn write_file(scopes: &[PathBuf], path: &Path, content: &str) -> Result<()> {
    let safe_path = validate_path_in_scopes_async(path, scopes).await?;
    if let Some(parent) = safe_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("Failed to create parent directory: {}", parent.display()))?;
    }
    tokio::fs::write(&safe_path, content)
        .await
        .with_context(|| format!("Failed to write file: {}", safe_path.display()))?;
    Ok(())
}

pub async fn replace_in_file(
    scopes: &[PathBuf],
    path: &Path,
    old_str: &str,
    new_str: &str,
) -> Result<usize> {
    if old_str.is_empty() {
        return Err(anyhow!("old_str must not be empty"));
    }

    let safe_path = validate_path_in_scopes_async(path, scopes).await?;
    let content = tokio::fs::read_to_string(&safe_path)
        .await
        .with_context(|| format!("Failed to read file: {}", safe_path.display()))?;

    let count = content.matches(old_str).count();
    if count == 0 {
        return Err(anyhow!(
            "String not found in file: '{}'",
            safe_path.display()
        ));
    }

    let updated = content.replace(old_str, new_str);
    tokio::fs::write(&safe_path, updated)
        .await
        .with_context(|| format!("Failed to write file: {}", safe_path.display()))?;
    Ok(count)
}

pub fn file_search(scopes: &[PathBuf], base_dir: &Path, pattern: &str) -> Result<Vec<String>> {
    let safe_base = validate_path_in_scopes_sync(base_dir, scopes)?;
    let glob_pattern = safe_base.join(pattern).to_string_lossy().to_string();

    let mut out = Vec::new();
    for path in (glob::glob(&glob_pattern)
        .with_context(|| format!("Invalid glob pattern: {}", pattern))?)
    .flatten()
    {
        if path.is_file() && validate_path_in_scopes_sync(&path, scopes).is_ok() {
            out.push(path.to_string_lossy().to_string());
        }
    }
    out.sort();
    Ok(out)
}

pub fn grep_search(
    scopes: &[PathBuf],
    base_dir: &Path,
    query: &str,
    is_regex: bool,
    include_pattern: Option<&str>,
    max_results: Option<usize>,
) -> Result<Vec<GrepMatch>> {
    let safe_base = validate_path_in_scopes_sync(base_dir, scopes)?;
    let max = max_results.unwrap_or(200);

    let regex = if is_regex {
        Some(Regex::new(query).with_context(|| format!("Invalid regex: {query}"))?)
    } else {
        None
    };

    let include_glob = include_pattern
        .map(|p| glob::Pattern::new(p).with_context(|| format!("Invalid includePattern: {p}")))
        .transpose()?;

    let mut matches = Vec::new();

    for entry in WalkDir::new(&safe_base).into_iter().flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        if let Some(g) = &include_glob {
            let rel = path.strip_prefix(&safe_base).unwrap_or(path);
            if !g.matches_path(rel) {
                continue;
            }
        }

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        for (idx, line) in content.lines().enumerate() {
            let hit = if let Some(r) = &regex {
                r.is_match(line)
            } else {
                line.to_lowercase().contains(&query.to_lowercase())
            };

            if hit {
                matches.push(GrepMatch {
                    path: path.to_string_lossy().to_string(),
                    line_number: idx + 1,
                    line: line.to_string(),
                });
                if matches.len() >= max {
                    return Ok(matches);
                }
            }
        }
    }

    Ok(matches)
}

/// Render a fetched HTML body into a [`WebFetchResult`]: HTML → plain text,
/// optional case-insensitive line filter, and `<title>` extraction. Pure, so
/// the parsing/filtering behaviour is unit-testable without any network.
fn render_webpage(url: &str, body: &str, query: Option<&str>) -> WebFetchResult {
    let rendered = html2text::from_read(body.as_bytes(), 120);

    let filtered = match query {
        Some(q) if !q.trim().is_empty() => {
            let ql = q.to_lowercase();
            rendered
                .lines()
                .filter(|l| l.to_lowercase().contains(&ql))
                .take(200)
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => rendered,
    };

    let title = body
        .split("<title>")
        .nth(1)
        .and_then(|s| s.split("</title>").next())
        .map(|s| s.trim().to_string());

    WebFetchResult {
        url: url.to_string(),
        title,
        text: filtered,
    }
}

/// Fetch a URL and render its HTML as plain text.
///
/// Outbound access is guarded against SSRF: the target must be `http`/`https`,
/// requests to private/loopback/link-local/cloud-metadata addresses are blocked
/// at connection time (including across redirects and DNS-rebinding), and the
/// redirect chain is bounded. See [`egress_guard`].
pub async fn fetch_webpage(url: &str, query: Option<&str>) -> Result<WebFetchResult> {
    fetch_webpage_guarded(url, query, true, None).await
}

/// Like [`fetch_webpage`], but additionally refuses a cross-domain redirect to a
/// host the caller's `[web]` policy does not approve (SPEC R-WEB.8). Used by the
/// MCP harness so an approved domain cannot 30x-launder egress to an unapproved
/// one. See [`egress_guard::RedirectDomainGuard`].
pub async fn fetch_webpage_with_redirect_guard(
    url: &str,
    query: Option<&str>,
    domain_guard: egress_guard::RedirectDomainGuard,
) -> Result<WebFetchResult> {
    fetch_webpage_guarded(url, query, true, Some(domain_guard)).await
}

/// Implementation of [`fetch_webpage`] with the SSRF private-range block as a
/// parameter. `block_private` is always `true` in production; tests set it
/// `false` to reach a loopback mock server (the R-WEB.3.3 dev opt-out).
/// `domain_guard`, when set, additionally enforces the cross-domain redirect
/// policy (R-WEB.8).
async fn fetch_webpage_guarded(
    url: &str,
    query: Option<&str>,
    block_private: bool,
    domain_guard: Option<egress_guard::RedirectDomainGuard>,
) -> Result<WebFetchResult> {
    egress_guard::check_url(url, block_private)?;
    let client = egress_guard::guarded_client(block_private, domain_guard)
        .context("Failed to build guarded HTTP client")?;
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("Failed to fetch URL: {url}"))?;
    let body = resp.text().await.context("Failed to read response body")?;
    Ok(render_webpage(url, &body, query))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_and_list_dir_work() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_path_buf();
        let file = root.join("a.txt");
        tokio::fs::write(&file, "one\ntwo\nthree").await.unwrap();

        let listed = list_dir(std::slice::from_ref(&root), &root).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "a.txt");

        let slice = read_file(std::slice::from_ref(&root), &file, Some(2), Some(2))
            .await
            .unwrap();
        assert_eq!(slice, "two");
    }

    #[test]
    fn grep_search_finds_matches() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_path_buf();
        std::fs::write(root.join("f.txt"), "alpha\nbeta\ngamma").unwrap();

        let hits = grep_search(
            std::slice::from_ref(&root),
            &root,
            "beta",
            false,
            None,
            Some(10),
        )
        .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line_number, 2);
    }

    #[tokio::test]
    async fn write_file_and_replace_in_file_work() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_path_buf();
        let file = root.join("edit.txt");

        write_file(std::slice::from_ref(&root), &file, "alpha beta alpha")
            .await
            .unwrap();

        let initial = tokio::fs::read_to_string(&file).await.unwrap();
        assert_eq!(initial, "alpha beta alpha");

        let replaced = replace_in_file(std::slice::from_ref(&root), &file, "alpha", "gamma")
            .await
            .unwrap();
        assert_eq!(replaced, 2);

        let final_text = tokio::fs::read_to_string(&file).await.unwrap();
        assert_eq!(final_text, "gamma beta gamma");
    }

    #[tokio::test]
    async fn write_file_out_of_scope() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_path_buf();

        let outer_td = tempfile::tempdir().unwrap();
        let file_outside = outer_td.path().join("outside.txt");

        let res = write_file(std::slice::from_ref(&root), &file_outside, "leak").await;
        assert!(res.is_err());
        assert!(
            res.unwrap_err()
                .to_string()
                .contains("outside allowed scopes")
        );
    }

    #[tokio::test]
    async fn replace_in_file_errors() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_path_buf();
        let file = root.join("test.txt");
        write_file(std::slice::from_ref(&root), &file, "hello")
            .await
            .unwrap();

        let res_empty = replace_in_file(std::slice::from_ref(&root), &file, "", "world").await;
        assert!(res_empty.is_err());

        let res_missing =
            replace_in_file(std::slice::from_ref(&root), &file, "missing", "world").await;
        assert!(res_missing.is_err());
    }

    #[test]
    fn file_search_glob_matching() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_path_buf();

        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn run() {}").unwrap();
        std::fs::write(root.join("README.md"), "# Hello").unwrap();

        let found = file_search(std::slice::from_ref(&root), &root, "src/*.rs").unwrap();
        assert_eq!(found.len(), 2);
        assert!(found[0].ends_with("lib.rs"));
        assert!(found[1].ends_with("main.rs"));
    }

    #[test]
    fn file_search_sandbox_escape_via_dotdot() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_path_buf();

        let td_parent = root.parent().unwrap();
        let sibling = td_parent.join("sibling_dir");
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("secret.txt"), "secret").unwrap();

        let found =
            file_search(std::slice::from_ref(&root), &root, "../sibling_dir/*.txt").unwrap();
        assert_eq!(found.len(), 0);
    }

    #[test]
    fn file_search_sandbox_escape_via_absolute() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_path_buf();

        let outer_td = tempfile::tempdir().unwrap();
        let secret_file = outer_td.path().join("secret.txt");
        std::fs::write(&secret_file, "secret").unwrap();

        let abs_pattern = format!("{}/*.txt", outer_td.path().to_string_lossy());
        let found = file_search(std::slice::from_ref(&root), &root, &abs_pattern).unwrap();
        assert_eq!(found.len(), 0);
    }

    #[tokio::test]
    async fn fetch_webpage_invalid_url() {
        let res = fetch_webpage("http://this-is-a-completely-invalid-url-domain.xyz", None).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn fetch_webpage_success_mock() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
                let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html><head><title>Test Title</title></head><body><h1>Hello</h1><p>Paragraph text here.</p></body></html>";
                let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
            }
        });

        // Permissive mode (block_private=false) so the loopback mock is reachable;
        // this exercises the real guarded client + render path end to end.
        let url = format!("http://{}", addr);
        let res = fetch_webpage_guarded(&url, Some("Paragraph"), false, None)
            .await
            .unwrap();
        assert_eq!(res.title, Some("Test Title".to_string()));
        assert!(res.text.contains("Paragraph text here"));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
                let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html><head><title>Test Title 2</title></head><body><h1>Hello 2</h1></body></html>";
                let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
            }
        });

        let url = format!("http://{}", addr);
        let res = fetch_webpage_guarded(&url, None, false, None)
            .await
            .unwrap();
        assert_eq!(res.title, Some("Test Title 2".to_string()));
        assert!(res.text.contains("Hello 2"));
    }

    #[tokio::test]
    async fn fetch_webpage_blocks_unapproved_cross_domain_redirect() {
        use egress_guard::RedirectDomainGuard;
        use std::sync::Arc;

        // A mock origin that 302-redirects to a *different* host. The origin is
        // reached via `localhost`; the redirect points at `127.0.0.1` — a
        // different host string, i.e. a cross-domain hop under R-WEB.8.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
                let response = "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/\r\nContent-Length: 0\r\n\r\n";
                let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
            }
        });

        // A guard whose origin is `localhost` and which approves no other host.
        let guard = RedirectDomainGuard::new("localhost", Arc::new(|_h: &str| false));
        let url = format!("http://localhost:{}/", addr.port());
        // block_private=false so the loopback mock is reachable; the redirect must
        // still be refused by the *domain* guard, not the SSRF resolver.
        let res = fetch_webpage_guarded(&url, None, false, Some(guard)).await;
        assert!(res.is_err(), "cross-domain redirect must be blocked");
        let msg = format!("{:#}", res.unwrap_err());
        assert!(
            msg.contains("different domain") || msg.contains("R-WEB.8"),
            "error should explain the cross-domain block, got: {msg}"
        );
    }

    #[tokio::test]
    async fn fetch_webpage_follows_same_host_redirect_under_guard() {
        use egress_guard::RedirectDomainGuard;
        use std::sync::Arc;

        // Two loopback listeners on the same host (127.0.0.1); the first redirects
        // to the second by absolute URL. Same host string ⇒ not cross-domain ⇒ the
        // guard must let it through even though `allow` approves nothing.
        let dest = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dest_addr = dest.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = dest.accept().await {
                let mut buf = [0u8; 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
                let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html><head><title>Dest</title></head><body>ok</body></html>";
                let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
            }
        });

        let src = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let src_addr = src.local_addr().unwrap();
        let dest_port = dest_addr.port();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = src.accept().await {
                let mut buf = [0u8; 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
                let response = format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{dest_port}/\r\nContent-Length: 0\r\n\r\n"
                );
                let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
            }
        });

        let guard = RedirectDomainGuard::new("127.0.0.1", Arc::new(|_h: &str| false));
        let url = format!("http://127.0.0.1:{}/", src_addr.port());
        let res = fetch_webpage_guarded(&url, None, false, Some(guard))
            .await
            .expect("same-host redirect must be followed");
        assert_eq!(res.title, Some("Dest".to_string()));
    }

    #[tokio::test]
    async fn fetch_webpage_blocks_ssrf_targets() {
        // Cloud-metadata, loopback admin, and RFC-1918 hosts must be refused by
        // the strict (production) path before any connection is attempted.
        for u in [
            "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
            "http://127.0.0.1:8080/admin",
            "http://192.168.0.1/",
            "http://[::1]:9000/",
        ] {
            let res = fetch_webpage(u, None).await;
            assert!(res.is_err(), "{u} must be blocked, got {res:?}");
        }
    }

    #[tokio::test]
    async fn fetch_webpage_rejects_non_http_scheme() {
        assert!(fetch_webpage("file:///etc/passwd", None).await.is_err());
    }

    #[test]
    fn render_webpage_extracts_title_and_filters() {
        let body =
            "<html><head><title>  Hi  </title></head><body><p>alpha</p><p>beta</p></body></html>";
        let full = render_webpage("http://x/", body, None);
        assert_eq!(full.title, Some("Hi".to_string()));
        assert!(full.text.contains("alpha") && full.text.contains("beta"));

        let filtered = render_webpage("http://x/", body, Some("beta"));
        assert!(filtered.text.contains("beta"));
        assert!(!filtered.text.contains("alpha"));
    }
}
