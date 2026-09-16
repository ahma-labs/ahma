//! Library entry point for the `simplify` analysis.
//!
//! Provides [`run`], the main entry point called by the `ahma` binary. The clap
//! argument struct it consumes, [`SimplifyArgs`], is defined in
//! `ahma_common::simplify_args` (and re-exported from this crate root) so the
//! `ahma` CLI can list the subcommand without depending on this crate.

use super::analysis;
use super::models;
use super::report;

use ahma_common::simplify_args::SimplifyArgs;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use analysis::lens::{Lens, parse_lenses};
use analysis::reuse::{self, ReuseConfig};
use analysis::{
    AnalyzerRegistry, ExternalMetrics, ScanOptions, get_project_name, is_cargo_workspace,
    perform_analysis, run_analysis,
};
use analysis::{dead_code, source_tree};
use models::{FileSimplicity, MetricsResults, MultiLensReport, resolve_extensions};
use report::{create_report_md, generate_ai_fix_prompt, generate_report};

/// Run the simplify analysis with the given arguments.
///
/// This is the main entry point for the `ahma simplify` subcommand.
pub fn run(mut args: SimplifyArgs) -> Result<()> {
    // If --heml is set, it triggers both --html and --open
    if args.heml {
        args.html = true;
        args.open = true;
    }

    if let Some(ref verify_path) = args.verify.clone() {
        let extensions = resolve_extensions(&args.extensions);
        return run_verify(verify_path, &args.output, &args.directory, &extensions);
    }

    let directory =
        dunce::canonicalize(&args.directory).context("Failed to canonicalize directory")?;
    prepare_output_directory(&args.output)?;

    let is_workspace = is_cargo_workspace(&args.directory);
    let extensions = resolve_extensions(&args.extensions);
    let lenses = parse_lenses(&args.lens)?;

    // Build the external analyzer registry unless the user requested rca-only.
    let registry = build_registry(args.no_external);
    let registry_ref = if args.no_external {
        None
    } else {
        Some(&registry)
    };

    let changed = if args.diff {
        Some(analysis::changed_files::changed_files(&directory)?)
    } else {
        None
    };

    let scan = perform_analysis(
        &directory,
        &args.output,
        is_workspace,
        &ScanOptions {
            extensions: &extensions,
            excludes: &args.exclude,
            changed: changed.as_ref(),
            compute_metrics: lenses.contains(&Lens::Complexity),
        },
        registry_ref,
    )?;

    let mut files_simplicity = if lenses.contains(&Lens::Complexity) {
        load_metrics(&args.output, true, &scan.external)?
    } else {
        Vec::new()
    };
    sort_files_by_simplicity(&mut files_simplicity);

    let duplicates = if lenses.contains(&Lens::Reuse) {
        reuse::find_duplicates(&scan.sources, &ReuseConfig::default())
    } else {
        Vec::new()
    };

    let dead_symbols = if lenses.contains(&Lens::DeadCode) {
        let trees: Vec<_> = scan
            .sources
            .iter()
            .filter_map(|(path, _)| source_tree::parse_source_tree(path))
            .collect();
        dead_code::find_dead_symbols(&scan.sources, &trees)
    } else {
        Vec::new()
    };

    let lens_report = MultiLensReport {
        complexity_files: files_simplicity,
        duplicates,
        dead_symbols,
        ..Default::default()
    };
    if lens_report.is_empty() {
        eprintln!("No findings: nothing matched the selected lenses.");
        return Ok(());
    }

    let project_name = get_project_name(&directory);

    // Determine output mode: write to file if --output-path, --html, or --open is set
    let write_to_file = args.output_path.is_some() || args.html || args.open;

    if write_to_file {
        let report_output_dir = determine_report_output_dir(&args.output_path)?;
        fs::create_dir_all(&report_output_dir)
            .context("Failed to create report output directory")?;

        generate_report(
            &lens_report,
            is_workspace,
            args.limit,
            &directory,
            args.html,
            &project_name,
            &report_output_dir,
        )?;

        print_report_locations(&report_output_dir, args.html);

        if let Some(issue_number) = args.ai_fix {
            handle_ai_fix_from_file(
                issue_number,
                &report_output_dir,
                &lens_report.complexity_files,
                &directory,
            )?;
        }

        if args.open
            && let Err(e) = open_report(&report_output_dir, args.html)
        {
            eprintln!("Warning: Failed to open report: {}", e);
        }
    } else {
        // Default: output markdown to stdout
        let md_content = create_report_md(
            &lens_report,
            is_workspace,
            args.limit,
            &directory,
            &project_name,
        );

        if let Some(issue_number) = args.ai_fix {
            handle_ai_fix_to_stdout(
                &md_content,
                issue_number,
                &lens_report.complexity_files,
                &directory,
            );
        } else {
            println!("{}", md_content);
        }
    }

    Ok(())
}

fn handle_ai_fix_from_file(
    issue_number: usize,
    report_output_dir: &Path,
    files_simplicity: &[FileSimplicity],
    directory: &Path,
) -> Result<()> {
    let md_path = report_output_dir.join("CODE_SIMPLICITY.md");
    let report_content = fs::read_to_string(&md_path).context("Failed to read generated report")?;
    handle_ai_fix_to_stdout(&report_content, issue_number, files_simplicity, directory);
    Ok(())
}

fn handle_ai_fix_to_stdout(
    report_content: &str,
    issue_number: usize,
    files_simplicity: &[FileSimplicity],
    directory: &Path,
) {
    println!("{}", report_content);

    match generate_ai_fix_prompt(files_simplicity, issue_number, directory) {
        Some(prompt) => println!("\n{}", prompt),
        None => eprintln!(
            "Warning: Issue #{} is out of range (only {} files analyzed).",
            issue_number,
            files_simplicity.len()
        ),
    }
}

/// Build the default external analyzer registry.
///
/// When `no_external` is true returns an empty registry so the caller can
/// still call `perform_analysis` with `None` (no external analysis runs).
fn build_registry(no_external: bool) -> AnalyzerRegistry {
    let mut registry = AnalyzerRegistry::new();
    if !no_external {
        // Kotlin: try standalone detekt-cli first (no Gradle needed), then Gradle detekt.
        registry.register(Box::new(analysis::detekt_cli::DetektCliAnalyzer));
        registry.register(Box::new(analysis::detekt::DetektAnalyzer));
        // Universal fallback for Kotlin, Swift, Java, Go, C#, ObjC, JS, TS.
        registry.register(Box::new(analysis::lizard::LizardAnalyzer));
        // Swift: dedicated SwiftLint for richer cognitive + cyclomatic metrics.
        registry.register(Box::new(analysis::swiftlint::SwiftLintAnalyzer));
    }
    registry
}

fn prepare_output_directory(output: &Path) -> Result<()> {
    if output.exists() {
        eprintln!(
            "Clearing existing analysis results in {}...",
            output.display()
        );
        let _ = fs::remove_dir_all(output);
    }
    fs::create_dir_all(output).context("Failed to create output directory")
}

fn resolve_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()
        .context("Failed to get current directory")?
        .join(path))
}

fn is_file_path(path: &Path) -> bool {
    path.extension().is_some()
        || path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().contains('.'))
}

fn determine_report_output_dir(output_path: &Option<PathBuf>) -> Result<PathBuf> {
    let path = match output_path {
        Some(p) => resolve_path(p)?,
        None => std::env::current_dir().context("Failed to get current directory")?,
    };

    if !is_file_path(&path) {
        return Ok(path);
    }

    path.parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| anyhow::anyhow!("Invalid output path: cannot determine parent directory"))
}

fn try_parse_metrics_file(
    path: &Path,
    normalized: bool,
    external: &HashMap<PathBuf, ExternalMetrics>,
) -> Option<FileSimplicity> {
    let content = fs::read_to_string(path).ok()?;
    match toml::from_str::<MetricsResults>(&content) {
        Ok(results) => {
            let fs_base = FileSimplicity::calculate(&results, normalized);
            // Look up external metrics by the source file's absolute path.
            let fs_merged = match external.get(Path::new(&results.name)) {
                Some(ext) => fs_base.apply_external(ext),
                None => fs_base,
            };
            Some(fs_merged)
        }
        Err(e) => {
            eprintln!("Error parsing {}: {}", path.display(), e);
            None
        }
    }
}

fn load_metrics(
    output: &Path,
    normalized: bool,
    external: &HashMap<PathBuf, ExternalMetrics>,
) -> Result<Vec<FileSimplicity>> {
    eprintln!("Aggregating metrics from {}...", output.display());

    // Phase 1: load rca-based TOML metrics and merge external where available.
    let mut covered_paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut files_simplicity: Vec<FileSimplicity> = WalkDir::new(output)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "toml"))
        .filter_map(|e| {
            let result = try_parse_metrics_file(e.path(), normalized, external)?;
            // Track the source path so we don't double-count in Phase 2.
            if let Ok(content) = fs::read_to_string(e.path())
                && let Ok(parsed) = toml::from_str::<MetricsResults>(&content)
            {
                covered_paths.insert(PathBuf::from(&parsed.name));
            }
            Some(result)
        })
        .collect();

    // Phase 2: create synthetic entries for files covered only by external
    // analyzers (e.g. Kotlin files where rca produced no metrics).
    for (src_path, ext_metrics) in external {
        if covered_paths.contains(src_path) {
            continue;
        }
        if let Some(fs) = FileSimplicity::from_external(src_path, ext_metrics) {
            files_simplicity.push(fs);
        }
    }

    Ok(files_simplicity)
}

fn sort_files_by_simplicity(files: &mut [FileSimplicity]) {
    files.sort_by(|a, b| {
        a.score
            .partial_cmp(&b.score)
            .unwrap()
            .then_with(|| b.cognitive.partial_cmp(&a.cognitive).unwrap())
    });
}

fn print_report_locations(directory: &Path, html: bool) {
    eprintln!(
        "Report generated: {}",
        directory.join("CODE_SIMPLICITY.md").display()
    );
    if html {
        eprintln!(
            "Report generated: {}",
            directory.join("CODE_SIMPLICITY.html").display()
        );
    }
}

fn open_report(directory: &Path, html: bool) -> Result<()> {
    let open_path = if html {
        directory.join("CODE_SIMPLICITY.html")
    } else {
        directory.join("CODE_SIMPLICITY.md")
    };
    opener::open(&open_path).context("Failed to open report")
}

fn run_verify(
    verify_path: &Path,
    output_dir: &Path,
    base_dir: &Path,
    extensions: &[String],
) -> Result<()> {
    let abs_verify = if verify_path.is_absolute() {
        verify_path.to_path_buf()
    } else {
        std::env::current_dir()?.join(verify_path)
    };
    let canonical_verify = dunce::canonicalize(&abs_verify)
        .with_context(|| format!("File not found: {}", verify_path.display()))?;

    // External-only languages (Kotlin, Swift, Objective-C) have no rca TOML
    // baseline because rust-code-analysis does not support them. Verify would
    // silently fail with a confusing "No baseline metrics found" message. Fail
    // clearly instead with an actionable message.
    let file_ext = canonical_verify
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    if matches!(file_ext.as_str(), "kt" | "kts" | "swift" | "m" | "mm") {
        anyhow::bail!(
            "Verify mode does not yet support .{} files.\n\
             External-analyzer baselines (Detekt, SwiftLint, Lizard) are not \
             persisted to the output directory, so before/after comparison is \
             not possible for this file type.\n\
             \n\
             Workaround: run a full analysis (`ahma simplify --output-dir \
             <DIR> <PROJECT>`) before and after your changes, then compare the \
             two JSON reports manually.",
            file_ext
        );
    }

    let baseline = find_baseline_metrics(output_dir, &canonical_verify)?;
    let baseline_simplicity = FileSimplicity::calculate(&baseline, true);

    let temp_output = tempfile::tempdir().context("Failed to create temp directory")?;
    let parent_dir = canonical_verify
        .parent()
        .context("Cannot determine parent directory")?;
    run_analysis(
        parent_dir,
        temp_output.path(),
        &ScanOptions {
            extensions,
            excludes: &[],
            changed: None,
            compute_metrics: true,
        },
        None,
        &mut Vec::new(),
    )?;

    let current = find_baseline_metrics(temp_output.path(), &canonical_verify)?;
    let current_simplicity = FileSimplicity::calculate(&current, true);

    let rel_path = analysis::get_relative_path(
        &canonical_verify,
        &dunce::canonicalize(base_dir).unwrap_or(base_dir.to_path_buf()),
    );
    print_verification(
        &rel_path.to_string_lossy(),
        &baseline_simplicity,
        &current_simplicity,
    );

    Ok(())
}

fn find_baseline_metrics(output_dir: &Path, target_path: &Path) -> Result<MetricsResults> {
    let target_str = target_path.to_string_lossy();

    WalkDir::new(output_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "toml"))
        .find_map(|entry| {
            let content = fs::read_to_string(entry.path()).ok()?;
            let results: MetricsResults = toml::from_str(&content).ok()?;
            (results.name == target_str).then_some(results)
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No baseline metrics found for {} in {}. Run a full analysis first.",
                target_str,
                output_dir.display()
            )
        })
}

fn print_verification(path: &str, before: &FileSimplicity, after: &FileSimplicity) {
    println!("=== VERIFICATION: {} ===\n", path);
    println!("BEFORE -> AFTER (CHANGE)");

    print_metric_row("Simplicity", before.score, after.score, "%", true);
    print_metric_row("  MI 40%", before.mi, after.mi, "", true);
    print_metric_row(
        "  Cognitive density 30%",
        before.cognitive,
        after.cognitive,
        "",
        false,
    );
    print_metric_row(
        "  Peak cognitive 20%",
        before.peak_cognitive,
        after.peak_cognitive,
        "",
        false,
    );
    print_metric_row("  SLOC / length 10%", before.sloc, after.sloc, "", false);
    print_metric_row(
        "Cyclomatic (info only)",
        before.cyclomatic,
        after.cyclomatic,
        "",
        false,
    );

    println!();
    print_verdict(before.score, after.score);
}

fn print_verdict(before_score: f64, after_score: f64) {
    let improvement = after_score - before_score;
    let msg = if improvement > 5.0 {
        "VERDICT: Significant improvement achieved."
    } else if improvement > 0.0 {
        "VERDICT: Modest improvement. Consider further refactoring."
    } else if improvement == 0.0 {
        "VERDICT: No change detected."
    } else {
        "VERDICT: Regression detected - complexity increased."
    };
    println!("{}", msg);
}

fn get_direction_label(pct: f64, higher_is_better: bool) -> &'static str {
    let is_positive = pct > 0.0;
    match (higher_is_better, is_positive) {
        (true, true) => "improvement",
        (true, false) => "regression",
        (false, true) => "increase",
        (false, false) => "reduction",
    }
}

fn format_metric_change(before: f64, after: f64, suffix: &str, higher_is_better: bool) -> String {
    if before == 0.0 {
        if after == 0.0 {
            return "unchanged".to_string();
        }
        return format!("+{:.0}{}", after, suffix);
    }

    let pct = ((after - before) / before) * 100.0;
    let label = get_direction_label(pct, higher_is_better);
    format!("{:.0}% {}", pct, label)
}

fn print_metric_row(label: &str, before: f64, after: f64, suffix: &str, higher_is_better: bool) {
    let change = format_metric_change(before, after, suffix, higher_is_better);
    println!(
        "  {:12} {:>6.0}{} -> {:>6.0}{} ({})",
        label, before, suffix, after, suffix, change
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    // Minimal wrapper to enable try_parse_from on SimplifyArgs (which derives Args, not Parser).
    #[derive(Parser, Debug)]
    #[command(name = "test")]
    struct TestParser {
        #[command(flatten)]
        inner: SimplifyArgs,
    }

    fn parse(args: &[&str]) -> SimplifyArgs {
        TestParser::try_parse_from(args).unwrap().inner
    }

    #[test]
    fn test_cli_parsing() {
        let args = parse(&["test", ".", "--output", "results"]);
        assert_eq!(args.directory, PathBuf::from("."));
        assert_eq!(args.output, PathBuf::from("results"));
        assert_eq!(args.output_path, None);
    }

    #[test]
    fn test_cli_parsing_with_output_path() {
        let args = parse(&["test", ".", "--output", "results", "--output-path", "/tmp"]);
        assert_eq!(args.directory, PathBuf::from("."));
        assert_eq!(args.output, PathBuf::from("results"));
        assert_eq!(args.output_path, Some(PathBuf::from("/tmp")));
    }

    #[test]
    fn test_cli_parsing_with_ai_fix() {
        let args = parse(&["test", ".", "--ai-fix", "1"]);
        assert_eq!(args.ai_fix, Some(1));
    }

    #[test]
    fn lens_defaults_to_all_and_diff_defaults_off() {
        let args = parse(&["test", "."]);
        assert_eq!(args.lens, vec!["all".to_string()]);
        assert!(!args.diff);
        assert_eq!(
            parse_lenses(&args.lens).unwrap(),
            vec![Lens::Complexity, Lens::Reuse, Lens::DeadCode]
        );
    }

    #[test]
    fn lens_accepts_a_comma_separated_list() {
        let args = parse(&["test", ".", "--lens", "reuse,complexity"]);
        assert_eq!(
            args.lens,
            vec!["reuse".to_string(), "complexity".to_string()]
        );
    }

    #[test]
    fn lens_rejects_an_unknown_value() {
        let args = parse(&["test", ".", "--lens", "nonsense"]);
        let err = parse_lenses(&args.lens).unwrap_err().to_string();
        assert!(
            err.contains("nonsense"),
            "error should name the bad value: {err}"
        );
        assert!(
            err.contains("reuse"),
            "error should list valid values: {err}"
        );
    }

    #[test]
    fn diff_flag_is_parsed() {
        let args = parse(&["test", ".", "--diff"]);
        assert!(args.diff);
    }

    #[test]
    fn test_cli_parsing_without_ai_fix() {
        let args = parse(&["test", "."]);
        assert_eq!(args.ai_fix, None);
    }

    #[test]
    fn test_cli_parsing_with_verify() {
        let args = parse(&["test", ".", "--verify", "src/main.rs"]);
        assert_eq!(args.verify, Some(PathBuf::from("src/main.rs")));
    }

    #[test]
    fn test_cli_parsing_without_verify() {
        let args = parse(&["test", "."]);
        assert_eq!(args.verify, None);
    }
}
