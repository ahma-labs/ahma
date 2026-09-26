use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use super::conversion::analyze_file;
use super::exclusion::{ExclusionMatcher, is_generated_content};
use super::external::{AnalyzerRegistry, ExternalMetrics};
use super::workspace::workspace_analysis_dirs;
use crate::models::Language;

// ---------------------------------------------------------------------------
// Public analysis API (drop-in replacement for the old CLI-based version)
// ---------------------------------------------------------------------------

/// Which files a scan covers and how much work it does per file: the extension
/// filter, the exclusion globs, an optional allowlist of paths git reported as
/// changed (`--diff`), and whether to compute complexity metrics at all —
/// parsing every file is the dominant cost, so a run that selected only
/// whole-project lenses skips it.
pub struct ScanOptions<'a> {
    pub extensions: &'a [String],
    pub excludes: &'a [String],
    pub changed: Option<&'a HashSet<PathBuf>>,
    pub compute_metrics: bool,
}

/// Analyses all source files under `dir`, writes per-file TOML metric results
/// into `output_dir`, and optionally runs external analyzers via `registry`.
///
/// Returns a map of absolute file path → external metrics for any files that
/// were covered by an external analyzer.  The map is empty when `registry` is
/// `None` or no analyzers are available.
///
/// `sources` accumulates the text of every file visited, for the whole-project
/// lenses (duplicate detection) that cannot work from the per-file TOML mirror.
pub fn run_analysis(
    dir: &Path,
    output_dir: &Path,
    options: &ScanOptions<'_>,
    registry: Option<&AnalyzerRegistry>,
    sources: &mut Vec<(PathBuf, String)>,
) -> Result<HashMap<PathBuf, ExternalMetrics>> {
    eprintln!("Analyzing {}...", dir.display());

    let allowed_exts: HashSet<&str> = options
        .extensions
        .iter()
        .map(|e| e.trim_start_matches('.'))
        .collect();

    // Run rca analysis and collect the set of languages seen.
    let mut languages_present: HashSet<Language> = HashSet::new();
    let mut analyzed_count = 0usize;
    let count = source_files(dir, &allowed_exts, options).try_fold(
        0usize,
        |count, path| -> Result<usize> {
            let lang = Language::from_path(&path);
            if lang != Language::Unknown {
                languages_present.insert(lang);
            }
            if let Ok(text) = fs::read_to_string(&path) {
                sources.push((path.clone(), text));
            }
            if options.compute_metrics && write_metrics_toml(&path, dir, output_dir)? {
                analyzed_count += 1;
            }
            Ok(count + 1)
        },
    )?;

    eprintln!("  Analyzed {count} files ({analyzed_count} with metrics).");

    // Run external analyzers when a registry is provided.
    let external = match registry {
        Some(reg) if !languages_present.is_empty() => reg.run_for_project(dir, &languages_present),
        _ => HashMap::new(),
    };

    Ok(external)
}

/// Check if a file matches the extension filter, is not excluded, and — when
/// `--diff` narrowed the scan — is one of the files git reported as changed.
fn is_matching_source_file(
    path: &Path,
    allowed_exts: &HashSet<&str>,
    options: &ScanOptions<'_>,
    matcher: &ExclusionMatcher,
) -> bool {
    let has_valid_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| {
            allowed_exts.is_empty() || allowed_exts.contains(ext.to_lowercase().as_str())
        });
    if !has_valid_ext {
        return false;
    }
    if matcher.is_excluded_path(path) {
        return false;
    }
    if let Some(changed) = options.changed
        && !changed.contains(path)
    {
        return false;
    }
    if is_generated_content(path) {
        return false;
    }
    true
}

/// Iterate source files in `dir` matching extension and exclusion filters.
/// Respects `.gitignore`, `.ignore`, global gitignore, and skips hidden directories.
fn source_files<'a>(
    dir: &'a Path,
    allowed_exts: &'a std::collections::HashSet<&'a str>,
    options: &'a ScanOptions<'a>,
) -> impl Iterator<Item = PathBuf> + 'a {
    let matcher = ExclusionMatcher::new(options.excludes);
    let filter_matcher = matcher.clone();
    ignore::WalkBuilder::new(dir)
        .standard_filters(true)
        .hidden(true)
        .require_git(false)
        .filter_entry(move |entry| {
            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                !filter_matcher.is_excluded_dir(entry.path())
            } else {
                true
            }
        })
        .build()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()))
        .map(ignore::DirEntry::into_path)
        .filter(move |path| is_matching_source_file(path, allowed_exts, options, &matcher))
}

/// Ensure the parent directory of `path` exists.
fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    Ok(())
}

/// Analyse a single file and write its metrics as TOML into `output_dir`.
/// Returns `true` if metrics were produced (rca could analyze the file),
/// `false` if the file was iterated but rca had no metrics for it.
fn write_metrics_toml(path: &Path, dir: &Path, output_dir: &Path) -> Result<bool> {
    let Some(results) = analyze_file(path) else {
        return Ok(false);
    };
    let toml_content = toml::to_string(&results).context("Failed to serialize metrics to TOML")?;
    let relative = path.strip_prefix(dir).unwrap_or(path);
    let toml_path = output_dir.join(relative.with_extension("toml"));
    ensure_parent_dir(&toml_path)?;
    fs::write(&toml_path, toml_content)
        .with_context(|| format!("Failed to write {}", toml_path.display()))?;
    Ok(true)
}

/// Result of a whole-project scan: external metrics per file, plus the source
/// text of every file visited for the whole-project lenses.
pub struct ScanResult {
    pub external: HashMap<PathBuf, ExternalMetrics>,
    pub sources: Vec<(PathBuf, String)>,
}

pub fn perform_analysis(
    directory: &Path,
    output: &Path,
    is_workspace: bool,
    options: &ScanOptions<'_>,
    registry: Option<&AnalyzerRegistry>,
) -> Result<ScanResult> {
    let dirs = workspace_analysis_dirs(directory, is_workspace)?;
    let mut external: HashMap<PathBuf, ExternalMetrics> = HashMap::new();
    let mut sources: Vec<(PathBuf, String)> = Vec::new();
    for dir in &dirs {
        external.extend(run_analysis(dir, output, options, registry, &mut sources)?);
    }
    Ok(ScanResult { external, sources })
}
