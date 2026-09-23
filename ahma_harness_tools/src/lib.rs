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
//! | [`list_dir`] | List directory entries within scope |
//! | [`replace_in_file`] / [`multi_edit`] | Exact-match edits, unique by default, atomic |
//! | [`apply_patch`] | Codex-format multi-file patch, all or nothing |
//! | [`file_search`] | Glob file discovery, .gitignore-aware, newest first |
//! | [`grep_search`] | Text/regex search with context and output modes, .gitignore-aware |
//! | [`fetch_webpage`] | Fetch a URL and render its HTML as plain text |

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub mod edit;
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
    /// Lines before the match, when context was asked for.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub before: Vec<String>,
    /// Lines after the match, when context was asked for.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
}

/// What [`grep_search`] returns per file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GrepOutputMode {
    /// Matching lines (with optional context).
    #[default]
    Content,
    /// Only the paths of files with a match.
    Files,
    /// The number of matching lines per file.
    Count,
}

/// A [`grep_search`] query.
#[derive(Debug, Clone, Default)]
pub struct GrepOptions {
    pub query: String,
    /// Treat `query` as a regex; otherwise it is literal text.
    pub is_regex: bool,
    /// Default: case-insensitive for literal text, case-sensitive for a regex.
    pub case_sensitive: Option<bool>,
    /// Glob on the path relative to the search root (e.g. `**/*.rs`).
    pub include_pattern: Option<String>,
    /// Cap on matches (content) or files (files, count). Default 200.
    pub max_results: Option<usize>,
    /// Lines of context before and after each match.
    pub context: usize,
    pub output_mode: GrepOutputMode,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileCount {
    pub path: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum GrepOutput {
    Matches(Vec<GrepMatch>),
    Files(Vec<String>),
    Counts(Vec<FileCount>),
}

impl GrepOutput {
    pub fn len(&self) -> usize {
        match self {
            Self::Matches(v) => v.len(),
            Self::Files(v) => v.len(),
            Self::Counts(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WebFetchResult {
    pub url: String,
    pub title: Option<String>,
    pub text: String,
}

/// Check `canonical` against a set of already-canonicalized scope roots,
/// returning an error if it falls outside all of them. Shared by the sync
/// and async validators, which differ only in how they canonicalize the
/// scopes (`std::fs` vs `tokio::fs`) before calling this.
fn check_allowed(canonical: &Path, canonical_scopes: &[PathBuf]) -> Result<()> {
    let allowed = canonical_scopes
        .iter()
        .any(|scope| canonical.starts_with(scope));

    if !allowed {
        return Err(anyhow!(
            "Path '{}' is outside allowed scopes",
            canonical.display()
        ));
    }

    Ok(())
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

    let canonical_scopes: Vec<PathBuf> = scopes
        .iter()
        .filter_map(|s| std::fs::canonicalize(s).ok())
        .collect();

    check_allowed(&canonical, &canonical_scopes)?;

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

    let mut canonical_scopes = Vec::with_capacity(scopes.len());
    for s in scopes {
        if let Ok(scope) = tokio::fs::canonicalize(s).await {
            canonical_scopes.push(scope);
        }
    }

    check_allowed(&canonical, &canonical_scopes)?;

    Ok(canonical)
}

/// Lines returned by [`read_file`] when the caller gives no end line.
pub const DEFAULT_READ_LINES: usize = 2000;
/// Characters kept of any one line in [`read_file`] output.
pub const MAX_LINE_CHARS: usize = 2000;

/// Read a file as numbered lines (`     7\tline`, like `cat -n`), from
/// `start_line` (1-based, default 1) to `end_line` (inclusive, default
/// `start + DEFAULT_READ_LINES - 1`).
///
/// Bounded on purpose: an unbounded read of a generated or minified file used
/// to flood the model's context in one call. When more lines follow, the
/// output says how many and where to continue; a binary file is refused with
/// its size rather than returned as mojibake. The line numbers are not part of
/// the file — edits must use the text after the tab.
pub async fn read_file(
    scopes: &[PathBuf],
    path: &Path,
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> Result<String> {
    let safe_path = validate_path_in_scopes_async(path, scopes).await?;
    let bytes = tokio::fs::read(&safe_path)
        .await
        .with_context(|| format!("Failed to read file: {}", safe_path.display()))?;
    if bytes.iter().take(8192).any(|&b| b == 0) {
        bail!(
            "{} is a binary file ({} bytes); not shown",
            safe_path.display(),
            bytes.len()
        );
    }
    let content = String::from_utf8_lossy(&bytes);
    if content.is_empty() {
        return Ok("(empty file)".to_string());
    }

    let start = start_line.unwrap_or(1).max(1);
    let end = end_line.unwrap_or(start.saturating_add(DEFAULT_READ_LINES - 1));
    let mut out = Vec::new();
    let mut total = 0;
    for (idx, line) in content.lines().enumerate() {
        total = idx + 1;
        if total < start || total > end {
            continue;
        }
        let line = line.strip_suffix('\r').unwrap_or(line);
        let shown = if line.chars().count() > MAX_LINE_CHARS {
            let cut: String = line.chars().take(MAX_LINE_CHARS).collect();
            format!("{cut}… [line truncated]")
        } else {
            line.to_string()
        };
        out.push(format!("{total:>6}\t{shown}"));
    }
    if start > total {
        bail!(
            "{} has {total} lines; start_line {start} is past the end",
            safe_path.display()
        );
    }
    if total > end {
        out.push(format!(
            "… {} more lines (read on with start_line={})",
            total - end,
            end + 1
        ));
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

/// Replace `old_str` with `new_str` in a file. `old_str` must occur exactly
/// once unless `replace_all` is set (see [`edit`] for the rules).
pub async fn replace_in_file(
    scopes: &[PathBuf],
    path: &Path,
    old_str: &str,
    new_str: &str,
    replace_all: bool,
) -> Result<edit::EditOutcome> {
    let edits = [edit::Edit {
        old_str: old_str.to_string(),
        new_str: new_str.to_string(),
        replace_all,
    }];
    multi_edit(scopes, path, &edits).await
}

/// Apply several edits to one file, in order, all or nothing.
pub async fn multi_edit(
    scopes: &[PathBuf],
    path: &Path,
    edits: &[edit::Edit],
) -> Result<edit::EditOutcome> {
    let safe_path = validate_path_in_scopes_async(path, scopes).await?;
    edit::edit_file(&safe_path, edits).await
}

/// What [`apply_patch`] changed, one line per file, e.g. `M src/lib.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchOutcome {
    pub changes: Vec<String>,
}

/// Resolve a patch path against `base_dir` and check it is in scope — also for
/// a file (and directories) that do not exist yet, via its nearest existing
/// ancestor. `..` components are refused outright.
pub fn resolve_patch_path(scopes: &[PathBuf], base_dir: &Path, path: &Path) -> Result<PathBuf> {
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        bail!("patch path {} must not contain `..`", path.display());
    }
    let full = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    };
    let mut existing = full.as_path();
    let mut rest = Vec::new();
    while !existing.exists() {
        rest.push(
            existing
                .file_name()
                .ok_or_else(|| anyhow!("no existing ancestor for {}", full.display()))?,
        );
        existing = existing
            .parent()
            .ok_or_else(|| anyhow!("no existing ancestor for {}", full.display()))?;
    }
    let mut resolved = validate_path_in_scopes_sync(existing, scopes)?;
    for part in rest.iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

/// Apply a Codex-format patch (`*** Begin Patch` … `*** End Patch`) under
/// `base_dir`. Every operation is computed before anything is written, so a
/// patch that does not apply changes nothing; each file is then replaced
/// atomically.
pub async fn apply_patch(scopes: &[PathBuf], base_dir: &Path, patch: &str) -> Result<PatchOutcome> {
    let ops = edit::parse_patch(patch)?;

    // Plan: (target path, new content or None to delete, summary line).
    let mut plan: Vec<(PathBuf, Option<String>, String)> = Vec::new();
    for op in &ops {
        match op {
            edit::PatchOp::Add { path, content } => {
                let target = resolve_patch_path(scopes, base_dir, path)?;
                if target.exists() {
                    bail!(
                        "Add File {}: it already exists — use Update File",
                        path.display()
                    );
                }
                plan.push((
                    target,
                    Some(content.clone()),
                    format!("A {}", path.display()),
                ));
            }
            edit::PatchOp::Delete { path } => {
                let target = resolve_patch_path(scopes, base_dir, path)?;
                if !target.is_file() {
                    bail!("Delete File {}: no such file", path.display());
                }
                plan.push((target, None, format!("D {}", path.display())));
            }
            edit::PatchOp::Update {
                path,
                move_to,
                hunks,
            } => {
                let source = resolve_patch_path(scopes, base_dir, path)?;
                let current = tokio::fs::read_to_string(&source)
                    .await
                    .with_context(|| format!("Update File {}: cannot read it", path.display()))?;
                let updated = edit::apply_hunks(&current, hunks, path)?;
                match move_to {
                    Some(dest) => {
                        let target = resolve_patch_path(scopes, base_dir, dest)?;
                        plan.push((
                            target,
                            Some(updated),
                            format!("R {} -> {}", path.display(), dest.display()),
                        ));
                        plan.push((source, None, String::new()));
                    }
                    None => plan.push((source, Some(updated), format!("M {}", path.display()))),
                }
            }
        }
    }

    let mut changes = Vec::new();
    for (target, content, summary) in plan {
        match content {
            Some(text) => {
                if let Some(parent) = target.parent() {
                    tokio::fs::create_dir_all(parent).await.with_context(|| {
                        format!("Failed to create directory {}", parent.display())
                    })?;
                }
                edit::atomic_write(&target, &text).await?;
            }
            None => tokio::fs::remove_file(&target)
                .await
                .with_context(|| format!("Failed to delete {}", target.display()))?,
        }
        if !summary.is_empty() {
            changes.push(summary);
        }
    }
    Ok(PatchOutcome { changes })
}

/// Files larger than this are skipped by [`grep_search`].
const GREP_MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
/// Most files [`file_search`] returns.
pub const FILE_SEARCH_MAX: usize = 1000;

/// Walk `root` the way ripgrep does: `.gitignore`/`.ignore` respected, hidden
/// files and directories skipped. The old walker descended into `target/` and
/// `.git/` and filled the result cap with build artifacts.
fn walk_files(root: &Path) -> impl Iterator<Item = PathBuf> {
    ignore::WalkBuilder::new(root)
        .build()
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.into_path())
}

fn include_matcher(pattern: Option<&str>) -> Result<Option<globset::GlobMatcher>> {
    pattern
        .map(|p| {
            globset::Glob::new(p)
                .map(|g| g.compile_matcher())
                .with_context(|| format!("Invalid glob pattern: {p}"))
        })
        .transpose()
}

/// Files under `base_dir` whose path relative to it matches the glob
/// `pattern`, most recently modified first, `.gitignore` respected, at most
/// [`FILE_SEARCH_MAX`].
pub fn file_search(scopes: &[PathBuf], base_dir: &Path, pattern: &str) -> Result<Vec<String>> {
    let safe_base = validate_path_in_scopes_sync(base_dir, scopes)?;
    let matcher = include_matcher(Some(pattern))?.expect("pattern given");
    let mut hits: Vec<(std::time::SystemTime, PathBuf)> = walk_files(&safe_base)
        .filter(|p| matcher.is_match(p.strip_prefix(&safe_base).unwrap_or(p)))
        .map(|p| {
            let modified = std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            (modified, p)
        })
        .collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    Ok(hits
        .into_iter()
        .take(FILE_SEARCH_MAX)
        .map(|(_, p)| p.to_string_lossy().to_string())
        .collect())
}

/// Search file contents under `base_dir` (see [`GrepOptions`]). Walks like
/// ripgrep (`.gitignore` respected, hidden files skipped); skips binary files and files over 10 MB.
pub fn grep_search(scopes: &[PathBuf], base_dir: &Path, opts: &GrepOptions) -> Result<GrepOutput> {
    let safe_base = validate_path_in_scopes_sync(base_dir, scopes)?;
    let max = opts.max_results.unwrap_or(200).max(1);
    let case_sensitive = opts.case_sensitive.unwrap_or(opts.is_regex);
    let source = if opts.is_regex {
        opts.query.clone()
    } else {
        regex::escape(&opts.query)
    };
    let regex = regex::RegexBuilder::new(&source)
        .case_insensitive(!case_sensitive)
        .build()
        .with_context(|| format!("Invalid regex: {}", opts.query))?;
    let include = include_matcher(opts.include_pattern.as_deref())?;

    let mut matches = Vec::new();
    let mut files = Vec::new();
    let mut counts = Vec::new();
    for path in walk_files(&safe_base) {
        let rel = path.strip_prefix(&safe_base).unwrap_or(&path);
        if include.as_ref().is_some_and(|m| !m.is_match(rel)) {
            continue;
        }
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > GREP_MAX_FILE_BYTES) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if bytes.iter().take(8192).any(|&b| b == 0) {
            continue;
        }
        let content = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = content.lines().collect();
        let shown = path.to_string_lossy().to_string();
        let mut in_file = 0;
        for (idx, line) in lines.iter().enumerate() {
            if !regex.is_match(line) {
                continue;
            }
            in_file += 1;
            if opts.output_mode != GrepOutputMode::Content {
                if opts.output_mode == GrepOutputMode::Files {
                    break;
                }
                continue;
            }
            let from = idx.saturating_sub(opts.context);
            let to = (idx + 1 + opts.context).min(lines.len());
            matches.push(GrepMatch {
                path: shown.clone(),
                line_number: idx + 1,
                line: line.to_string(),
                before: lines[from..idx].iter().map(|l| l.to_string()).collect(),
                after: lines[idx + 1..to].iter().map(|l| l.to_string()).collect(),
            });
            if matches.len() >= max {
                return Ok(GrepOutput::Matches(matches));
            }
        }
        if in_file == 0 {
            continue;
        }
        match opts.output_mode {
            GrepOutputMode::Files => files.push(shown),
            GrepOutputMode::Count => counts.push(FileCount {
                path: shown,
                count: in_file,
            }),
            GrepOutputMode::Content => {}
        }
        if files.len() >= max || counts.len() >= max {
            break;
        }
    }
    Ok(match opts.output_mode {
        GrepOutputMode::Content => GrepOutput::Matches(matches),
        GrepOutputMode::Files => GrepOutput::Files(files),
        GrepOutputMode::Count => GrepOutput::Counts(counts),
    })
}

/// Render a fetched HTML body into a [`WebFetchResult`]: HTML → plain text,
/// optional case-insensitive line filter, and `<title>` extraction. Pure, so
/// the parsing/filtering behaviour is unit-testable without any network.
fn render_webpage(url: &str, body: &str, query: Option<&str>) -> Result<WebFetchResult> {
    let rendered =
        html2text::from_read(body.as_bytes(), 120).context("Failed to render HTML as text")?;

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

    Ok(WebFetchResult {
        url: url.to_string(),
        title,
        text: cap_fetched_text(filtered),
    })
}

/// Characters of page text [`fetch_webpage`] returns.
pub const MAX_FETCH_CHARS: usize = 50_000;

/// A page can be megabytes of text; returned whole it floods the context.
/// Keep the head and say how much was cut and how to narrow the fetch.
fn cap_fetched_text(text: String) -> String {
    let total = text.chars().count();
    if total <= MAX_FETCH_CHARS {
        return text;
    }
    let head: String = text.chars().take(MAX_FETCH_CHARS).collect();
    format!(
        "{head}\n\n… [{} more characters not shown — pass `query` to keep only the \
         lines that mention it]",
        total - MAX_FETCH_CHARS
    )
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
    render_webpage(url, &body, query)
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
        assert_eq!(
            slice,
            "     2\ttwo\n… 1 more lines (read on with start_line=3)"
        );
    }

    #[test]
    fn grep_search_finds_matches() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_path_buf();
        std::fs::write(root.join("f.txt"), "alpha\nbeta\ngamma").unwrap();

        let opts = GrepOptions {
            query: "beta".into(),
            max_results: Some(10),
            ..Default::default()
        };
        let GrepOutput::Matches(hits) =
            grep_search(std::slice::from_ref(&root), &root, &opts).unwrap()
        else {
            panic!("content mode returns matches");
        };

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

        // Two matches: refused unless replace_all says to change both.
        assert!(
            replace_in_file(std::slice::from_ref(&root), &file, "alpha", "gamma", false)
                .await
                .is_err()
        );
        let replaced = replace_in_file(std::slice::from_ref(&root), &file, "alpha", "gamma", true)
            .await
            .unwrap();
        assert_eq!(replaced.replacements, 2);

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

        let res_empty =
            replace_in_file(std::slice::from_ref(&root), &file, "", "world", false).await;
        assert!(res_empty.is_err());

        let res_missing = replace_in_file(
            std::slice::from_ref(&root),
            &file,
            "missing",
            "world",
            false,
        )
        .await;
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
        assert!(found.iter().any(|f| f.ends_with("lib.rs")));
        assert!(found.iter().any(|f| f.ends_with("main.rs")));
    }

    /// Search walks like ripgrep: `.gitignore`d build output is not searched,
    /// so it cannot fill the result cap.
    #[test]
    fn search_respects_gitignore() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_path_buf();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/gen.rs"), "needle").unwrap();
        std::fs::write(root.join("src.rs"), "needle").unwrap();

        let files = file_search(std::slice::from_ref(&root), &root, "**/*.rs").unwrap();
        assert_eq!(files.len(), 1, "{files:?}");
        let opts = GrepOptions {
            query: "needle".into(),
            output_mode: GrepOutputMode::Files,
            ..Default::default()
        };
        let GrepOutput::Files(hits) =
            grep_search(std::slice::from_ref(&root), &root, &opts).unwrap()
        else {
            panic!("files mode");
        };
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].ends_with("src.rs"));
    }

    #[test]
    fn grep_gives_context_counts_and_honours_case() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_path_buf();
        std::fs::write(root.join("a.txt"), "one\nTwo\nthree\ntwo\n").unwrap();

        let content = GrepOptions {
            query: "two".into(),
            context: 1,
            ..Default::default()
        };
        let GrepOutput::Matches(m) =
            grep_search(std::slice::from_ref(&root), &root, &content).unwrap()
        else {
            panic!("content");
        };
        assert_eq!(m.len(), 2, "literal text is case-insensitive by default");
        assert_eq!(
            (m[0].before.clone(), m[0].after.clone()),
            (vec!["one".to_string()], vec!["three".to_string()])
        );

        let count = GrepOptions {
            query: "two".into(),
            case_sensitive: Some(true),
            output_mode: GrepOutputMode::Count,
            ..Default::default()
        };
        let GrepOutput::Counts(c) =
            grep_search(std::slice::from_ref(&root), &root, &count).unwrap()
        else {
            panic!("count");
        };
        assert_eq!(c[0].count, 1);
    }

    #[tokio::test]
    async fn read_file_is_numbered_bounded_and_refuses_binary() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_path_buf();
        let big = root.join("big.txt");
        let text: String = (1..=2500).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&big, text).unwrap();
        let out = read_file(std::slice::from_ref(&root), &big, None, None)
            .await
            .unwrap();
        assert!(out.starts_with("     1\tline 1\n"));
        assert!(out.contains("  2000\tline 2000"));
        assert!(!out.contains("line 2001\n"));
        assert!(
            out.ends_with("… 500 more lines (read on with start_line=2001)"),
            "{}",
            &out[out.len() - 80..]
        );

        let bin = root.join("b.bin");
        std::fs::write(&bin, [0u8, 1, 2, 3]).unwrap();
        let err = read_file(std::slice::from_ref(&root), &bin, None, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("binary"), "{err}");
    }

    #[tokio::test]
    async fn a_patch_applies_everything_or_nothing() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_path_buf();
        std::fs::write(root.join("keep.txt"), "a\nb\n").unwrap();
        let scopes = std::slice::from_ref(&root);

        // Second op cannot apply: the first must not have happened either.
        let bad = "*** Begin Patch\n*** Add File: new.txt\n+hi\n*** Update File: keep.txt\n-zzz\n+y\n*** End Patch";
        assert!(apply_patch(scopes, &root, bad).await.is_err());
        assert!(!root.join("new.txt").exists(), "nothing written");

        let good = "*** Begin Patch\n*** Add File: dir/new.txt\n+hi\n*** Update File: keep.txt\n-b\n+c\n*** End Patch";
        let out = apply_patch(scopes, &root, good).await.unwrap();
        assert_eq!(out.changes, vec!["A dir/new.txt", "M keep.txt"]);
        assert_eq!(
            std::fs::read_to_string(root.join("keep.txt")).unwrap(),
            "a\nc\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("dir/new.txt")).unwrap(),
            "hi\n"
        );

        let escape = "*** Begin Patch\n*** Add File: ../x.txt\n+no\n*** End Patch";
        assert!(apply_patch(scopes, &root, escape).await.is_err());
    }

    #[test]
    fn long_fetches_are_capped_with_a_way_forward() {
        let long = "x".repeat(MAX_FETCH_CHARS + 10);
        let capped = cap_fetched_text(long);
        assert!(capped.contains("10 more characters"));
        assert!(capped.contains("query"));
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
        let full = render_webpage("http://x/", body, None).unwrap();
        assert_eq!(full.title, Some("Hi".to_string()));
        assert!(full.text.contains("alpha") && full.text.contains("beta"));

        let filtered = render_webpage("http://x/", body, Some("beta")).unwrap();
        assert!(filtered.text.contains("beta"));
        assert!(!filtered.text.contains("alpha"));
    }
}
