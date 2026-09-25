use super::analysis::{get_package_name, get_relative_path};
use super::models::{
    AltitudeChain, DeadSymbol, DuplicateGroup, FileSimplicity, Language, MultiLensReport,
};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub struct RepoSummary {
    pub avg_score: f64,
    pub language_summaries: HashMap<Language, LanguageSummary>,
}

pub struct LanguageSummary {
    pub score: f64,
    pub package_scores: Vec<(String, f64)>,
}

fn average<I>(values: I) -> f64
where
    I: IntoIterator<Item = f64>,
{
    let (sum, count) = values
        .into_iter()
        .fold((0.0, 0usize), |(sum, count), value| {
            (sum + value, count + 1)
        });

    if count == 0 { 0.0 } else { sum / count as f64 }
}

fn sorted_languages<T>(language_map: &HashMap<Language, T>) -> Vec<&Language> {
    let mut languages: Vec<_> = language_map.keys().collect();
    languages.sort_by(|a, b| a.display_name().cmp(b.display_name()));
    languages
}

fn language_group_label(lang: &Language, is_workspace: bool) -> &'static str {
    match (lang, is_workspace) {
        (Language::Rust, true) => "Crate",
        (Language::Rust, false) => "Module",
        (Language::Python | Language::JavaScript | Language::TypeScript, _) => "Module",
        (Language::Kotlin | Language::Java, _) => "Package",
        _ => "Directory",
    }
}

fn relative_path_string(path: &Path, base_dir: &Path) -> String {
    get_relative_path(path, base_dir)
        .to_string_lossy()
        .to_string()
}

fn path_components(path: &Path) -> Vec<String> {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect()
}

/// Aggregates one language's files into its `LanguageSummary`: the language-wide
/// average score plus each package's average score, sorted by score desc then name.
fn build_language_summary(lang_files: &[&FileSimplicity], base_dir: &Path) -> LanguageSummary {
    let lang_avg = average(lang_files.iter().map(|f| f.score));

    let mut package_map: HashMap<String, Vec<f64>> = HashMap::new();
    for f in lang_files {
        let package = get_package_name(Path::new(&f.path), base_dir);
        package_map.entry(package).or_default().push(f.score);
    }

    let mut package_scores: Vec<(String, f64)> = package_map
        .into_iter()
        .map(|(p, scores)| (p, average(scores)))
        .collect();
    package_scores.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });

    LanguageSummary {
        score: lang_avg,
        package_scores,
    }
}

impl RepoSummary {
    pub fn from_files(files: &[FileSimplicity], base_dir: &Path) -> Self {
        let avg_score = average(files.iter().map(|f| f.score));

        let mut lang_map: HashMap<Language, Vec<&FileSimplicity>> = HashMap::new();
        for f in files {
            lang_map.entry(f.language).or_default().push(f);
        }

        let language_summaries = lang_map
            .into_iter()
            .map(|(lang, lang_files)| (lang, build_language_summary(&lang_files, base_dir)))
            .collect();

        Self {
            avg_score,
            language_summaries,
        }
    }
}

pub fn generate_report(
    lenses: &MultiLensReport,
    is_workspace: bool,
    limit: usize,
    output_dir: &Path,
    generate_html: bool,
    project_name: &str,
    report_output_dir: &Path,
) -> Result<(), std::io::Error> {
    let md_content = create_report_md(lenses, is_workspace, limit, output_dir, project_name);

    fs::write(report_output_dir.join("CODE_SIMPLICITY.md"), &md_content)?;

    if generate_html {
        let full_html = render_markdown_to_html(&md_content, "Code Simplicity Report");
        fs::write(report_output_dir.join("CODE_SIMPLICITY.html"), full_html)?;
    }
    Ok(())
}

/// Convert markdown report content into a styled HTML document.
pub fn render_markdown_to_html(md_content: &str, title: &str) -> String {
    let mut options = pulldown_cmark::Options::empty();
    options.insert(pulldown_cmark::Options::ENABLE_TABLES);
    options.insert(pulldown_cmark::Options::ENABLE_STRIKETHROUGH);
    let parser = pulldown_cmark::Parser::new_ext(md_content, options);
    let mut html_output = String::new();
    pulldown_cmark::html::push_html(&mut html_output, parser);

    let style = "
            body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Helvetica, Arial, sans-serif; line-height: 1.6; color: #24292e; max-width: 900px; margin: 0 auto; padding: 40px 20px; background-color: #f6f8fa; }
            h1, h2, h3 { color: #1b1f23; border-bottom: 1px solid #eaecef; padding-bottom: 0.3em; margin-top: 1.5em; }
            pre { background-color: #f6f8fa; padding: 16px; border-radius: 6px; overflow: auto; }
            code { font-family: ui-monospace, SFMono-Regular, SF Mono, Menlo, Consolas, Liberation Mono, monospace; background-color: rgba(27,31,35,0.05); padding: 0.2em 0.4em; border-radius: 3px; }
            blockquote { padding: 0 1em; color: #6a737d; border-left: 0.25em solid #dfe2e1; margin: 0; }
            table { border-spacing: 0; border-collapse: collapse; width: 100%; margin: 1em 0; }
            table td, table th { padding: 6px 13px; border: 1px solid #dfe2e1; }
            table tr { background-color: #fff; border-top: 1px solid #c6cbd1; }
            table tr:nth-child(2n) { background-color: #f6f8fa; }
        ";

    format!(
        "<!DOCTYPE html>\n<html>\n<head>\n<meta charset='UTF-8'>\n<title>{}</title>\n<style>\n{}\n</style>\n</head>\n<body>\n{}\n</body>\n</html>",
        title, style, html_output
    )
}

pub fn create_report_md(
    lenses: &MultiLensReport,
    is_workspace: bool,
    limit: usize,
    base_dir: &Path,
    project_name: &str,
) -> String {
    let files = &lenses.complexity_files;
    let summary = RepoSummary::from_files(files, base_dir);
    let mut report = String::new();

    write_header(&mut report, project_name, summary.avg_score);
    write_executive_summary(&mut report, summary.avg_score);
    write_package_simplicity(&mut report, &summary, is_workspace);
    write_emergencies(&mut report, files, limit, base_dir);
    write_duplicates(&mut report, &lenses.duplicates, limit, base_dir);
    write_dead_symbols(&mut report, &lenses.dead_symbols, limit, base_dir);
    write_altitude_chains(&mut report, &lenses.altitude_chains, limit, base_dir);
    write_glossary(&mut report);

    report
}

/// Renders the altitude lens. A forwarding layer is frequently deliberate — a
/// public facade over an internal module, a trait impl delegating to a free
/// function, a platform shim — so a chain is a question about where the logic
/// belongs, not evidence of a mistake.
fn write_altitude_chains(
    report: &mut String,
    chains: &[AltitudeChain],
    limit: usize,
    base_dir: &Path,
) {
    if chains.is_empty() {
        return;
    }

    report.push_str("## Delegation Chains (Altitude Lens)\n\n");
    report.push_str(&format!(
        "{} chain(s) of functions that only forward to the next one. Each layer adds \
         a call frame and a name to learn without adding behaviour — but forwarding \
         is often deliberate, so treat these as questions about where the logic \
         belongs rather than defects.\n\n",
        chains.len()
    ));

    for chain in chains.iter().take(limit) {
        report.push_str(&format!(
            "- **{} hops** — {}\n",
            chain.depth, chain.description
        ));
        for call in &chain.calls {
            report.push_str(&format!(
                "  - `{}` → `{}` at `{}`:{}\n",
                call.caller,
                call.callee,
                get_relative_path(&call.file, base_dir).display(),
                call.line
            ));
        }
    }
    report.push('\n');

    if chains.len() > limit {
        report.push_str(&format!(
            "*{} further chain(s) not shown; raise --limit to see them.*\n\n",
            chains.len() - limit
        ));
    }
}

/// Renders the dead-code lens. Reference counting is textual, so it cannot see a
/// downstream crate calling a library's public API, a macro-generated call site,
/// or dispatch through a trait object — every entry here is a question, not a
/// finding, and deleting one on the report's word alone will eventually delete
/// live code. The caveat is in the output, not just the docs, for that reason.
fn write_dead_symbols(report: &mut String, symbols: &[DeadSymbol], limit: usize, base_dir: &Path) {
    if symbols.is_empty() {
        return;
    }

    report.push_str("## Possibly Unreferenced Exports (Dead Code Lens)\n\n");
    report.push_str(&format!(
        "{} exported symbol(s) have no reference anywhere in the scanned files. \
         Verify each before removing anything: a public API used only by another \
         crate, a call generated by a macro, and a trait method reached through a \
         trait object all look identical to this lens.\n\n",
        symbols.len()
    ));

    report.push_str("| Symbol | Kind | Visibility | Location |\n");
    report.push_str("|--------|------|------------|----------|\n");
    for symbol in symbols.iter().take(limit) {
        report.push_str(&format!(
            "| `{}` | {} | `{}` | `{}`:{} |\n",
            symbol.name,
            symbol.kind.label(),
            symbol.visibility,
            get_relative_path(&symbol.file, base_dir).display(),
            symbol.line
        ));
    }
    report.push('\n');

    if symbols.len() > limit {
        report.push_str(&format!(
            "*{} further candidate(s) not shown; raise --limit to see them.*\n\n",
            symbols.len() - limit
        ));
    }
}

/// Renders the reuse lens. Duplicate blocks are *candidates* for extraction, not
/// defects: identical-looking code can be coincidental, and the reader has to
/// judge whether a shared helper would actually be clearer. The wording says so,
/// because a report that reads as a verdict gets acted on as one.
fn write_duplicates(report: &mut String, groups: &[DuplicateGroup], limit: usize, base_dir: &Path) {
    if groups.is_empty() {
        return;
    }

    let duplicated_lines: usize = groups
        .iter()
        .map(|group| group.line_count * group.locations.len().saturating_sub(1))
        .sum();

    report.push_str("## Duplicate Code (Reuse Lens)\n\n");
    report.push_str(&format!(
        "Found {} duplicated block(s) spanning roughly {} redundant line(s). \
         These are candidates for extraction, not confirmed problems — review each \
         before changing anything.\n\n",
        groups.len(),
        duplicated_lines
    ));

    for (index, group) in groups.iter().take(limit).enumerate() {
        report.push_str(&format!(
            "### Duplicate #{} — {} lines × {} occurrences\n\n",
            index + 1,
            group.line_count,
            group.locations.len()
        ));
        for location in &group.locations {
            report.push_str(&format!(
                "- `{}` lines {}–{}\n",
                get_relative_path(&location.file, base_dir).display(),
                location.start_line,
                location.end_line
            ));
        }
        report.push_str("\n```\n");
        report.push_str(group.sample_text.trim_end());
        report.push_str("\n```\n\n");
    }

    if groups.len() > limit {
        report.push_str(&format!(
            "*{} further duplicate group(s) not shown; raise --limit to see them.*\n\n",
            groups.len() - limit
        ));
    }
}

fn write_header(report: &mut String, project_name: &str, avg_score: f64) {
    report.push_str(&format!("# Code Simplicity Metrics: {}\n\n", project_name));
    report.push_str(&format!(
        "## Overall Repository Simplicity: **{:.0}%**\n\n",
        avg_score
    ));
    let now = chrono::Local::now();
    report.push_str(&format!(
        "*Generated on: {}*\n\n",
        now.format("%Y-%m-%d %H:%M:%S")
    ));
}

fn write_executive_summary(report: &mut String, avg_score: f64) {
    report.push_str("### Executive Summary\n");
    let msg = if avg_score > 80.0 {
        "The repository has good simplicity overall. Focus on isolated high-complexity files.\n\n"
    } else if avg_score > 60.0 {
        "The repository has moderate technical debt. Consider refactoring the top complexity issues.\n\n"
    } else {
        "The repository requires significant architectural review. Multiple areas show high complexity.\n\n"
    };
    report.push_str(msg);
}

fn write_package_simplicity(report: &mut String, summary: &RepoSummary, is_workspace: bool) {
    let languages = sorted_languages(&summary.language_summaries);

    for lang in languages {
        let Some(lang_summary) = summary.language_summaries.get(lang) else {
            continue;
        };

        report.push_str(&format!(
            "## {} Simplicity (Avg: {:.0}%)\n\n",
            lang.display_name(),
            lang_summary.score
        ));

        write_package_scores_list(report, lang, lang_summary, is_workspace);
    }
}

/// Writes the "By Crate/Module/Package" breakdown for a language, when there is
/// more than one package to distinguish.
fn write_package_scores_list(
    report: &mut String,
    lang: &Language,
    lang_summary: &LanguageSummary,
    is_workspace: bool,
) {
    if lang_summary.package_scores.len() <= 1 {
        return;
    }

    let group_label = language_group_label(lang, is_workspace);
    report.push_str(&format!("### By {}\n\n", group_label));

    for (i, (p, score)) in lang_summary.package_scores.iter().enumerate() {
        report.push_str(&format!("{}. **{}**: {:.0}%\n", i + 1, p, score));
    }
    report.push('\n');
}

/// Returns the shortest path suffix (joined by `/`) from `components` that is
/// not shared by any entry in `siblings`, or `None` if no such suffix exists.
fn find_minimal_unique_suffix(components: &[String], siblings: &[Vec<String>]) -> Option<String> {
    let n = components.len();
    for depth in 2..=n {
        let candidate = components[n - depth..].join("/");
        let is_unique = siblings.iter().all(|other_comps| {
            let on = other_comps.len();
            depth > on || other_comps[on - depth..].join("/") != candidate
        });
        if is_unique {
            return Some(candidate);
        }
    }
    None
}

/// Compute display names for a list of files, disambiguating entries that share
/// a basename by adding the minimal parent directory context needed for uniqueness.
fn disambiguate_display_names(files: &[&FileSimplicity], base_dir: &Path) -> Vec<String> {
    let entries: Vec<(PathBuf, String)> = files
        .iter()
        .map(|f| {
            let path = Path::new(&f.path);
            let rel_path = get_relative_path(path, base_dir);
            let basename = path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| rel_path.to_string_lossy().to_string());
            (rel_path, basename)
        })
        .collect();

    let mut basename_counts: HashMap<&str, usize> = HashMap::new();
    for (_, basename) in &entries {
        *basename_counts.entry(basename.as_str()).or_insert(0) += 1;
    }

    entries
        .iter()
        .enumerate()
        .map(|(i, (rel_path, basename))| {
            if basename_counts.get(basename.as_str()).copied().unwrap_or(0) <= 1 {
                return basename.clone();
            }

            let components = path_components(rel_path);
            let siblings: Vec<Vec<String>> = entries
                .iter()
                .enumerate()
                .filter(|(j, (_, b))| *j != i && b == basename)
                .map(|(_, (rp, _))| path_components(rp))
                .collect();

            find_minimal_unique_suffix(&components, &siblings)
                .unwrap_or_else(|| rel_path.to_string_lossy().to_string())
        })
        .collect()
}

fn write_issue_metrics(report: &mut String, file: &FileSimplicity) {
    report.push_str(&format!(
        "    - Metrics: Cog: {:.0}, PeakCog: {:.0}, Cyc: {:.0}, SLOC: {:.0}, MI: {:.1}\n",
        file.cognitive, file.peak_cognitive, file.cyclomatic, file.sloc, file.mi
    ));
}

fn write_hotspot_details(report: &mut String, file: &FileSimplicity) {
    if !file.hotspots.is_empty() {
        report.push_str("    - **Hotspots**:\n");
        for hotspot in &file.hotspots {
            report.push_str(&format!(
                "      - `{}()` lines {}-{}: Cog={}, Cyc={}, SLOC={}\n",
                hotspot.name,
                hotspot.start_line,
                hotspot.end_line,
                hotspot.cognitive as u32,
                hotspot.cyclomatic as u32,
                hotspot.sloc as u32
            ));
        }
    }
}

fn write_detekt_findings(report: &mut String, file: &FileSimplicity) {
    let other_issues: Vec<_> = file
        .external_issues
        .iter()
        .filter(|issue| {
            let rule = issue.rule.to_lowercase();
            !rule.contains("cognitive") && !rule.contains("cyclomatic")
        })
        .take(5)
        .collect();

    if !other_issues.is_empty() {
        report.push_str("    - **Detekt Findings**:\n");
        for issue in other_issues {
            let fn_ctx = issue
                .function_name
                .as_deref()
                .map(|name| format!(" in `{name}()`"))
                .unwrap_or_default();
            report.push_str(&format!(
                "      - [{}{}] {} (line {})\n",
                issue.rule, fn_ctx, issue.message, issue.start_line
            ));
        }
    }
}

fn write_emergency_entry(
    report: &mut String,
    rank: usize,
    file: &FileSimplicity,
    display_name: &str,
    base_dir: &Path,
) {
    let culprit = identify_culprit(file);
    let rel_str = relative_path_string(Path::new(&file.path), base_dir);

    report.push_str(&format!(
        "{}. **{}**: Simplicity: {:.0}% ({})**\n\t{}\n",
        rank, display_name, file.score, culprit, rel_str
    ));
    write_issue_metrics(report, file);
    write_hotspot_details(report, file);
    write_detekt_findings(report, file);
}

fn write_emergencies(report: &mut String, files: &[FileSimplicity], limit: usize, base_dir: &Path) {
    let mut lang_map: HashMap<Language, Vec<&FileSimplicity>> = HashMap::new();
    for f in files {
        lang_map.entry(f.language).or_default().push(f);
    }

    let languages = sorted_languages(&lang_map);

    for lang in languages {
        let lang_files = lang_map.get(lang).unwrap();
        let display_limit = std::cmp::min(limit, lang_files.len());
        let displayed: Vec<&FileSimplicity> =
            lang_files.iter().take(display_limit).copied().collect();
        let display_names = disambiguate_display_names(&displayed, base_dir);

        report.push_str(&format!(
            "## Top {display_limit} {} Code Complexity Issues (Lowest Simplicity)\n\n",
            lang.display_name()
        ));

        for (i, f) in displayed.iter().enumerate() {
            write_emergency_entry(report, i + 1, f, &display_names[i], base_dir);
        }
        report.push('\n');
    }
}

pub fn identify_culprit(f: &FileSimplicity) -> &'static str {
    // Use per-function max complexity when available to distinguish files where
    // the total is driven by many simple functions vs. a few genuinely complex ones.
    let max_fn_cognitive = f
        .hotspots
        .iter()
        .map(|h| h.cognitive)
        .fold(0.0_f64, f64::max);
    let max_fn_cyclomatic = f
        .hotspots
        .iter()
        .map(|h| h.cyclomatic)
        .fold(0.0_f64, f64::max);

    if max_fn_cognitive >= 10.0 {
        "High Cognitive Complexity (concentrated)"
    } else if f.cognitive > 20.0 {
        "Elevated Cognitive Complexity (distributed across many functions)"
    } else if max_fn_cyclomatic >= 15.0 {
        "High Cyclomatic Complexity (concentrated)"
    } else if f.cyclomatic > 20.0 {
        "Elevated Cyclomatic Complexity (distributed across many functions)"
    } else if f.sloc > 500.0 {
        "Mega-file"
    } else if f.mi < 50.0 {
        "Low Maintainability Index"
    } else {
        "General Complexity"
    }
}

/// Builds the hotspot/findings section of the AI fix prompt for `file`.
/// Shows per-function hotspots when available, or falls back to raw Detekt findings.
fn format_hotspots(hotspots: &[super::models::FunctionHotspot]) -> String {
    let mut s = String::from("\nHOTSPOT FUNCTIONS (worst first — focus here):\n");
    for h in hotspots {
        if h.cognitive > 0.0 || h.cyclomatic > 0.0 {
            s.push_str(&format!(
                "  - `{}()` line {}: Cog={:.0}, Cyc={:.0}\n",
                h.name, h.start_line, h.cognitive, h.cyclomatic
            ));
        }
    }
    s
}

fn format_detekt_findings(issues: &[super::analysis::ExternalIssue]) -> String {
    let mut s = String::from("\nDETEKT FINDINGS (for context — fix the listed functions):\n");
    for issue in issues.iter().take(10) {
        let fn_ctx = issue
            .function_name
            .as_deref()
            .map(|n| format!(" in `{n}()`"))
            .unwrap_or_default();
        s.push_str(&format!(
            "  - [{}{}] line {}: {}\n",
            issue.rule, fn_ctx, issue.start_line, issue.message
        ));
    }
    s
}

/// Builds the hotspot/findings section of the AI fix prompt for `file`.
/// Shows per-function hotspots when available, or falls back to raw Detekt findings.
fn build_hotspot_section(file: &FileSimplicity) -> String {
    if !file.hotspots.is_empty() {
        format_hotspots(&file.hotspots)
    } else if !file.external_issues.is_empty() {
        format_detekt_findings(&file.external_issues)
    } else {
        String::new()
    }
}

/// Generates a structured AI prompt instructing the parent AI to evaluate and
/// optionally simplify a specific issue from the complexity report.
///
/// `files` must already be sorted by score ascending (worst first).
/// `issue_number` is 1-indexed (1 = most complex file).
///
/// Returns `None` if `issue_number` is out of bounds or files is empty.
pub fn generate_ai_fix_prompt(
    files: &[FileSimplicity],
    issue_number: usize,
    base_dir: &Path,
) -> Option<String> {
    if issue_number == 0 || files.is_empty() {
        return None;
    }
    let index = issue_number - 1;
    let file = files.get(index)?;

    let rel_str = relative_path_string(Path::new(&file.path), base_dir);
    let culprit = identify_culprit(file);
    let is_test_file = file.path.contains("/tests/")
        || file.path.contains("_test.rs")
        || file.path.ends_with("test.rs");
    let test_context = if is_test_file {
        "\nNOTE: This is a test file. Repetitive setup/assert patterns and many small test \
        functions are expected and desirable — they exist to provide comprehensive coverage. \
        Only recommend extracting shared helpers if the same boilerplate block appears 3+ \
        times verbatim AND extraction genuinely reduces duplication without obscuring intent."
    } else {
        ""
    };

    let hotspot_section = build_hotspot_section(file);

    Some(format!(
        "\
=== EVALUATE COMPLEXITY: ISSUE #{issue_number} ===

TARGET: {rel_str}
SIMPLICITY: {score:.0}% | FLAGGED REASON: {culprit}
METRICS: Cognitive={cog:.0}, PeakCog={peak_cog:.0}, Cyclomatic={cyc:.0} (info only), SLOC={sloc:.0}, MI={mi:.1}{hotspot_section}{test_context}

STEP 1 - READ the target file and the hotspot functions listed above.

STEP 2 - EVALUATE critically: Is this code genuinely hard to understand and maintain?
   Ask yourself:
   - Do the hotspot functions have deep nesting, tangled control flow, or non-obvious logic?
   - Or are they long because they enumerate cases (match arms, config fields, test assertions)?
   - Would splitting them force a reader to jump between more locations to understand one thing?
   - Is the cyclomatic complexity spread across many small functions (file-level sum) or
     concentrated in a few large ones (genuine complexity)?
   If the code is already clear and the metrics are driven by volume or enumeration rather
   than genuine algorithmic complexity, state that and STOP — no changes are needed.

STEP 3 - If and only if genuine complexity was found: IMPLEMENT focused changes.
   - Target ONLY the hotspot functions identified as genuinely complex
   - Prefer early returns and guard clauses to reduce nesting depth
   - Extract helpers only when the extracted piece has a clear, self-contained responsibility
   - Do NOT split a function solely because it exceeds a line-count threshold; locality
     (keeping related logic together) is often more valuable than smaller function count
   - Do NOT refactor surrounding code unless directly needed

STEP 4 - VERIFY by running the project's test suite.

STEP 5 - Report: either (a) the changes made and the measurable metric improvements,
         or (b) a concise explanation of why no changes were warranted.",
        issue_number = issue_number,
        rel_str = rel_str,
        score = file.score,
        culprit = culprit,
        cog = file.cognitive,
        peak_cog = file.peak_cognitive,
        cyc = file.cyclomatic,
        sloc = file.sloc,
        mi = file.mi,
        hotspot_section = hotspot_section,
        test_context = test_context,
    ))
}

fn write_glossary(report: &mut String) {
    report.push_str("\n---\n\n## Metrics Glossary\n\n");
    report.push_str("### Score Formula\n");
    report.push_str("*Scores are calibrated for AI-assisted maintenance: decomposed, focused functions reduce the context an AI agent must hold to make safe changes.*\n\n");
    report.push_str("The composite simplicity score is computed as:\n\n");
    report.push_str("`Score = 0.4 × MI + 0.3 × Cognitive Density + 0.2 × Peak Cognitive + 0.1 × Length Score`\n\n");
    report.push_str("| Component | Weight | What it measures |\n");
    report.push_str("|-----------|--------|------------------|\n");
    report.push_str("| Maintainability Index (MI) | 40% | Function-weighted composite of Halstead volume, cyclomatic, and SLOC |\n");
    report.push_str("| Cognitive Density | 30% | Cognitive complexity normalised by SLOC |\n");
    report.push_str(
        "| Peak Cognitive | 20% | Cognitive complexity of the single most complex function |\n",
    );
    report
        .push_str("| Length Score | 10% | 100% at ≤300 SLOC; scales down linearly above that |\n");
    report.push_str("| Cyclomatic | — | Reported for context only; already embedded in MI |\n\n");
    report.push_str("### Cognitive Complexity\n- **Description**: Measures how hard it is to understand the control flow of the code. [See](https://axify.io/blog/cognitive-complexity)\n- **How to Improve**: Extract complex conditions into well-named functions and reduce nesting levels.\n\n");
    report.push_str("### Cyclomatic Complexity\n- **Description**: Measures the number of linearly independent paths through the source code (info only — not directly scored). [See](https://www.nist.gov/publications/structured-testing-methodology-using-cyclomatic-complexity-metric)\n- **How to Improve**: Use polymorphic abstractions instead of complex switch/if-else chains, and break down large functions into smaller components.\n\n");
    report.push_str("### Source Lines of Code (SLOC)\n- **Description**: A measure of the size of the computer program by counting the number of lines in the text of the source code. [See](https://en.wikipedia.org/wiki/Source_lines_of_code)\n- **How to Improve**: Remove dead code and refactor repetitive logic into reusable helper functions or macros.\n\n");
    report.push_str("### Maintainability Index (MI)\n- **Description**: A composite metric representing the relative ease of maintaining the code; higher is better. [See](https://learn.microsoft.com/en-us/visualstudio/code-quality/code-metrics-maintainability-index-range-and-meaning)\n- **How to Improve**: Simultaneously reduce complexity (both cognitive and cyclomatic) and file size to boost the index.\n");
}

#[cfg(test)]
mod tests {
    use super::super::models::{AnalysisConfidence, FunctionHotspot};
    use super::*;

    fn lens_report(complexity_files: Vec<FileSimplicity>) -> MultiLensReport {
        MultiLensReport {
            complexity_files,
            ..Default::default()
        }
    }

    /// Helper to construct a FileSimplicity for tests without hotspots.
    fn test_file(
        path: &str,
        lang: Language,
        score: f64,
        cog: f64,
        cyc: f64,
        sloc: f64,
        mi: f64,
    ) -> FileSimplicity {
        FileSimplicity {
            path: path.to_string(),
            language: lang,
            score,
            cognitive: cog,
            cyclomatic: cyc,
            sloc,
            mi,
            peak_cognitive: 0.0,
            hotspots: vec![],
            external_issues: vec![],
            analysis_sources: vec!["rust-code-analysis".to_string()],
            confidence: AnalysisConfidence::Full,
        }
    }

    /// Helper to construct a FileSimplicity with hotspots for testing report output.
    #[allow(clippy::too_many_arguments)]
    fn test_file_with_hotspots(
        path: &str,
        lang: Language,
        score: f64,
        cog: f64,
        cyc: f64,
        sloc: f64,
        mi: f64,
        hotspots: Vec<FunctionHotspot>,
    ) -> FileSimplicity {
        FileSimplicity {
            path: path.to_string(),
            language: lang,
            score,
            cognitive: cog,
            cyclomatic: cyc,
            sloc,
            mi,
            peak_cognitive: 0.0,
            hotspots,
            external_issues: vec![],
            analysis_sources: vec!["rust-code-analysis".to_string()],
            confidence: AnalysisConfidence::Full,
        }
    }

    #[test]
    fn test_create_report_structure() {
        let files = vec![
            test_file(
                "pkg1/file1.rs",
                Language::Rust,
                80.0,
                10.0,
                5.0,
                100.0,
                100.0,
            ),
            test_file(
                "pkg2/file2.rs",
                Language::Rust,
                40.0,
                30.0,
                25.0,
                600.0,
                40.0,
            ),
        ];

        let report = create_report_md(
            &lens_report(files),
            false,
            10,
            Path::new("."),
            "test_project",
        );
        assert!(report.contains("# Code Simplicity Metrics: test_project"));
        assert!(report.contains("## Overall Repository Simplicity: **60%**"));
        assert!(report.contains("## Rust Simplicity"));
    }

    #[test]
    fn test_report_multi_language_emergencies() {
        let files = vec![
            test_file("file1.rs", Language::Rust, 50.0, 20.0, 15.0, 100.0, 50.0),
            test_file("file2.py", Language::Python, 40.0, 25.0, 20.0, 150.0, 40.0),
        ];

        let report = create_report_md(&lens_report(files), false, 10, Path::new("."), "test_multi");

        assert!(report.contains("## Top 1 Rust Code Complexity Issues"));
        assert!(report.contains("## Top 1 Python Code Complexity Issues"));
        assert!(report.contains("file1.rs"));
        assert!(report.contains("file2.py"));
        assert!(!report.contains("(Rust)"));
        assert!(!report.contains("(Python)"));
    }

    #[test]
    fn test_repo_summary_orders_packages_by_simplicity_desc_then_name() {
        let files = vec![
            test_file(
                "mod_b/main.py",
                Language::Python,
                40.0,
                20.0,
                10.0,
                100.0,
                60.0,
            ),
            test_file(
                "mod_c/main.py",
                Language::Python,
                60.0,
                15.0,
                8.0,
                90.0,
                70.0,
            ),
            test_file(
                "mod_a/main.py",
                Language::Python,
                80.0,
                10.0,
                5.0,
                80.0,
                80.0,
            ),
            test_file(
                "mod_d/main.py",
                Language::Python,
                60.0,
                12.0,
                7.0,
                85.0,
                72.0,
            ),
        ];

        let summary = RepoSummary::from_files(&files, Path::new("."));
        let python = summary.language_summaries.get(&Language::Python).unwrap();

        assert_eq!(
            python.package_scores,
            vec![
                ("mod_a".to_string(), 80.0),
                ("mod_c".to_string(), 60.0),
                ("mod_d".to_string(), 60.0),
                ("mod_b".to_string(), 40.0),
            ]
        );
    }

    #[test]
    fn test_disambiguate_unique_basenames_unchanged() {
        let files = [
            test_file("src/foo.rs", Language::Rust, 50.0, 20.0, 15.0, 100.0, 50.0),
            test_file("src/bar.rs", Language::Rust, 40.0, 25.0, 20.0, 150.0, 40.0),
        ];
        let refs: Vec<&FileSimplicity> = files.iter().collect();
        let names = disambiguate_display_names(&refs, Path::new("."));
        assert_eq!(names, vec!["foo.rs", "bar.rs"]);
    }

    #[test]
    fn test_disambiguate_duplicate_basenames_adds_parent_dir() {
        let files = [
            test_file(
                "project/src/analysis/translation.rs",
                Language::Rust,
                36.0,
                92.0,
                174.0,
                1191.0,
                0.0,
            ),
            test_file(
                "project/src/views/translation.rs",
                Language::Rust,
                36.0,
                88.0,
                68.0,
                786.0,
                0.0,
            ),
        ];
        let refs: Vec<&FileSimplicity> = files.iter().collect();
        let names = disambiguate_display_names(&refs, Path::new("."));
        assert_eq!(
            names,
            vec!["analysis/translation.rs", "views/translation.rs"]
        );
    }

    #[test]
    fn test_disambiguate_three_files_same_basename() {
        let files = [
            test_file(
                "crate_a/src/lib.rs",
                Language::Rust,
                50.0,
                10.0,
                5.0,
                100.0,
                50.0,
            ),
            test_file(
                "crate_b/src/lib.rs",
                Language::Rust,
                40.0,
                20.0,
                10.0,
                200.0,
                40.0,
            ),
            test_file(
                "crate_c/src/lib.rs",
                Language::Rust,
                30.0,
                30.0,
                15.0,
                300.0,
                30.0,
            ),
        ];
        let refs: Vec<&FileSimplicity> = files.iter().collect();
        let names = disambiguate_display_names(&refs, Path::new("."));
        assert_eq!(
            names,
            vec![
                "crate_a/src/lib.rs",
                "crate_b/src/lib.rs",
                "crate_c/src/lib.rs"
            ]
        );
    }

    #[test]
    fn test_disambiguate_mixed_unique_and_duplicate() {
        let files = [
            test_file(
                "src/analysis/translation.rs",
                Language::Rust,
                36.0,
                92.0,
                174.0,
                1191.0,
                0.0,
            ),
            test_file(
                "src/unique_file.rs",
                Language::Rust,
                50.0,
                10.0,
                5.0,
                100.0,
                50.0,
            ),
            test_file(
                "src/views/translation.rs",
                Language::Rust,
                36.0,
                88.0,
                68.0,
                786.0,
                0.0,
            ),
        ];
        let refs: Vec<&FileSimplicity> = files.iter().collect();
        let names = disambiguate_display_names(&refs, Path::new("."));
        assert_eq!(
            names,
            vec![
                "analysis/translation.rs",
                "unique_file.rs",
                "views/translation.rs"
            ]
        );
    }

    #[test]
    fn test_report_disambiguates_same_basename_files() {
        let files = vec![
            test_file(
                "project/src/analysis/translation.rs",
                Language::Rust,
                36.0,
                92.0,
                174.0,
                1191.0,
                0.0,
            ),
            test_file(
                "project/src/views/translation.rs",
                Language::Rust,
                36.0,
                88.0,
                68.0,
                786.0,
                0.0,
            ),
        ];

        let report = create_report_md(
            &lens_report(files),
            false,
            10,
            Path::new("."),
            "test_disambig",
        );

        assert!(report.contains("**analysis/translation.rs**"));
        assert!(report.contains("**views/translation.rs**"));
        assert!(!report.contains("**translation.rs**"));
    }

    #[test]
    fn test_report_includes_hotspots() {
        let files = vec![test_file_with_hotspots(
            "src/complex.rs",
            Language::Rust,
            35.0,
            45.0,
            30.0,
            800.0,
            0.0,
            vec![
                FunctionHotspot {
                    name: "handle_request".to_string(),
                    start_line: 145,
                    end_line: 210,
                    cognitive: 28.0,
                    cyclomatic: 15.0,
                    sloc: 65.0,
                },
                FunctionHotspot {
                    name: "process_message".to_string(),
                    start_line: 312,
                    end_line: 350,
                    cognitive: 10.0,
                    cyclomatic: 8.0,
                    sloc: 38.0,
                },
            ],
        )];

        let report = create_report_md(
            &lens_report(files),
            false,
            10,
            Path::new("."),
            "test_hotspots",
        );

        assert!(report.contains("**Hotspots**:"));
        assert!(report.contains("`handle_request()` lines 145-210: Cog=28, Cyc=15"));
        assert!(report.contains("`process_message()` lines 312-350: Cog=10, Cyc=8"));
    }

    #[test]
    fn test_report_no_hotspots_section_when_empty() {
        let files = vec![test_file(
            "src/simple.rs",
            Language::Rust,
            80.0,
            5.0,
            3.0,
            50.0,
            90.0,
        )];

        let report = create_report_md(
            &lens_report(files),
            false,
            10,
            Path::new("."),
            "test_no_hotspots",
        );

        assert!(!report.contains("**Hotspots**"));
    }

    #[test]
    fn test_generate_ai_fix_prompt_issue_1() {
        let files = vec![
            test_file(
                "src/complex.rs",
                Language::Rust,
                25.0,
                45.0,
                30.0,
                800.0,
                35.0,
            ),
            test_file(
                "src/moderate.rs",
                Language::Rust,
                65.0,
                12.0,
                8.0,
                200.0,
                70.0,
            ),
        ];

        let prompt = generate_ai_fix_prompt(&files, 1, Path::new(".")).unwrap();

        assert!(prompt.contains("ISSUE #1"));
        assert!(prompt.contains("src/complex.rs"));
        assert!(prompt.contains("25%"));
        // With no per-function hotspot data, elevated file-level cognitive fires
        // the "distributed" culprit label.
        assert!(prompt.contains("Cognitive Complexity"));
        assert!(prompt.contains("Cognitive=45"));
        assert!(prompt.contains("Cyclomatic=30"));
        assert!(prompt.contains("SLOC=800"));
        assert!(prompt.contains("MI=35.0"));
        assert!(prompt.contains("STEP 1"));
        assert!(prompt.contains("STEP 4"));
        assert!(prompt.contains("hotspot functions"));
        assert!(prompt.contains("Target ONLY"));
    }

    #[test]
    fn test_generate_ai_fix_prompt_issue_2() {
        let files = vec![
            test_file(
                "src/worst.rs",
                Language::Rust,
                20.0,
                50.0,
                40.0,
                900.0,
                30.0,
            ),
            test_file(
                "src/second.rs",
                Language::Rust,
                40.0,
                10.0,
                25.0,
                300.0,
                45.0,
            ),
        ];

        let prompt = generate_ai_fix_prompt(&files, 2, Path::new(".")).unwrap();

        assert!(prompt.contains("ISSUE #2"));
        assert!(prompt.contains("src/second.rs"));
        // With no per-function hotspot data, elevated file-level cyclomatic fires
        // the "distributed" culprit label.
        assert!(prompt.contains("Cyclomatic Complexity"));
    }

    #[test]
    fn test_generate_ai_fix_prompt_out_of_bounds() {
        let files = vec![test_file(
            "src/only.rs",
            Language::Rust,
            50.0,
            15.0,
            10.0,
            100.0,
            60.0,
        )];
        assert!(generate_ai_fix_prompt(&files, 2, Path::new(".")).is_none());
    }

    #[test]
    fn test_generate_ai_fix_prompt_zero_issue() {
        let files = vec![test_file(
            "src/file.rs",
            Language::Rust,
            50.0,
            15.0,
            10.0,
            100.0,
            60.0,
        )];
        assert!(generate_ai_fix_prompt(&files, 0, Path::new(".")).is_none());
    }

    #[test]
    fn test_generate_ai_fix_prompt_empty_files() {
        let files: Vec<FileSimplicity> = vec![];
        assert!(generate_ai_fix_prompt(&files, 1, Path::new(".")).is_none());
    }

    #[test]
    fn test_generate_ai_fix_prompt_mega_file() {
        let files = vec![test_file(
            "src/huge.rs",
            Language::Rust,
            45.0,
            15.0,
            15.0,
            800.0,
            55.0,
        )];

        let prompt = generate_ai_fix_prompt(&files, 1, Path::new(".")).unwrap();
        assert!(prompt.contains("Mega-file"));
    }

    #[test]
    fn test_generate_ai_fix_prompt_low_mi() {
        let files = vec![test_file(
            "src/unmaintainable.rs",
            Language::Rust,
            35.0,
            10.0,
            10.0,
            200.0,
            40.0,
        )];

        let prompt = generate_ai_fix_prompt(&files, 1, Path::new(".")).unwrap();
        assert!(prompt.contains("Low Maintainability Index"));
    }

    #[test]
    fn test_identify_culprit_all_variants() {
        // Files with no hotspot data → file-level totals drive the distributed labels.
        let high_cog = test_file("a.rs", Language::Rust, 30.0, 25.0, 10.0, 100.0, 50.0);
        assert_eq!(
            identify_culprit(&high_cog),
            "Elevated Cognitive Complexity (distributed across many functions)"
        );

        let high_cyc = test_file("b.rs", Language::Rust, 30.0, 10.0, 25.0, 100.0, 50.0);
        assert_eq!(
            identify_culprit(&high_cyc),
            "Elevated Cyclomatic Complexity (distributed across many functions)"
        );

        // Files with high-complexity hotspots → concentrated labels.
        let concentrated_cog = test_file_with_hotspots(
            "a2.rs",
            Language::Rust,
            30.0,
            25.0,
            10.0,
            100.0,
            50.0,
            vec![FunctionHotspot {
                name: "complex_fn".to_string(),
                start_line: 1,
                end_line: 50,
                cognitive: 12.0,
                cyclomatic: 8.0,
                sloc: 30.0,
            }],
        );
        assert_eq!(
            identify_culprit(&concentrated_cog),
            "High Cognitive Complexity (concentrated)"
        );

        let concentrated_cyc = test_file_with_hotspots(
            "b2.rs",
            Language::Rust,
            30.0,
            10.0,
            25.0,
            100.0,
            50.0,
            vec![FunctionHotspot {
                name: "complex_fn".to_string(),
                start_line: 1,
                end_line: 50,
                cognitive: 0.0,
                cyclomatic: 18.0,
                sloc: 30.0,
            }],
        );
        assert_eq!(
            identify_culprit(&concentrated_cyc),
            "High Cyclomatic Complexity (concentrated)"
        );

        let mega = test_file("c.rs", Language::Rust, 30.0, 10.0, 10.0, 600.0, 50.0);
        assert_eq!(identify_culprit(&mega), "Mega-file");

        let low_mi = test_file("d.rs", Language::Rust, 30.0, 10.0, 10.0, 100.0, 40.0);
        assert_eq!(identify_culprit(&low_mi), "Low Maintainability Index");

        let general = test_file("e.rs", Language::Rust, 30.0, 10.0, 10.0, 100.0, 60.0);
        assert_eq!(identify_culprit(&general), "General Complexity");
    }
}
