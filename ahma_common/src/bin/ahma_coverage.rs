use anyhow::{Context, Result};
use serde::Deserialize;
use std::cmp::Reverse;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct CoverageData {
    data: Vec<CovData>,
}

#[derive(Debug, Deserialize)]
struct CovData {
    totals: Option<Totals>,
    files: Vec<FileEntry>,
}

#[derive(Debug, Deserialize)]
struct Totals {
    lines: SummaryStats,
    functions: SummaryStats,
    instantiations: SummaryStats,
    regions: SummaryStats,
}

#[derive(Debug, Deserialize)]
struct FileEntry {
    filename: String,
    summary: Summary,
    segments: Vec<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct Summary {
    lines: SummaryStats,
}

#[derive(Debug, Deserialize)]
struct SummaryStats {
    count: usize,
    covered: usize,
    percent: f64,
}

/// One file's row in the coverage table plus its uncovered-line detail —
/// named fields instead of a positional tuple sorted and destructured by
/// index.
struct FileCoverage {
    filename: String,
    line_pct: f64,
    uncovered_count: usize,
    uncovered_display: String,
    uncovered_ranges: Vec<String>,
}

/// Close a run of consecutive uncovered lines `[start, end]` into its display
/// form (`"N"` for a single line, `"N-M"` for a range), appending it to
/// `ranges`. Shared by the mid-loop close (a gap follows) and the final flush
/// (the run ends at the last line) in [`main`].
fn push_range(ranges: &mut Vec<String>, start: u64, end: u64) {
    if start == end {
        ranges.push(format!("{start}"));
    } else {
        ranges.push(format!("{start}-{end}"));
    }
}

fn main() -> Result<()> {
    let input_path = "coverage.json";
    let output_path = "coverage_summary.md";

    if !Path::new(input_path).exists() {
        anyhow::bail!("Error: {} not found.", input_path);
    }

    let file = File::open(input_path).context("Failed to open coverage.json")?;
    let data: CoverageData =
        serde_json::from_reader(file).context("Failed to parse coverage.json")?;

    let cov_data = data
        .data
        .first()
        .context("Error: Invalid coverage.json format.")?;

    let mut output = File::create(output_path).context("Failed to create coverage_summary.md")?;

    writeln!(output, "# Code Coverage Summary\n")?;

    if let Some(totals) = &cov_data.totals {
        writeln!(output, "## Totals\n")?;
        writeln!(output, "| Category | Count | Covered | Percent |")?;
        writeln!(output, "|----------|-------|---------|---------|")?;

        let categories = [
            ("Lines", &totals.lines),
            ("Functions", &totals.functions),
            ("Instantiations", &totals.instantiations),
            ("Regions", &totals.regions),
        ];

        for (name, stats) in categories {
            writeln!(
                output,
                "| {} | {} | {} | {:.2}% |",
                name, stats.count, stats.covered, stats.percent
            )?;
        }
        writeln!(output)?;
    }

    writeln!(output, "## File Coverage\n")?;
    writeln!(output, "| File | Coverage | Uncovered Lines |")?;
    writeln!(output, "|------|----------|-----------------|")?;

    let mut parsed_files = Vec::new();

    for file_entry in &cov_data.files {
        let line_cov = file_entry.summary.lines.percent;
        let mut uncovered_ranges = Vec::new();
        let mut uncovered_count = 0;

        if line_cov < 100.0 {
            let mut uncovered_lines = BTreeSet::new();
            for seg in &file_entry.segments {
                if let Some(line) = seg.first().and_then(|v| v.as_u64())
                    && let Some(count) = seg.get(2).and_then(|v| v.as_u64())
                    && count == 0
                {
                    uncovered_lines.insert(line);
                }
            }

            uncovered_count = uncovered_lines.len();

            if !uncovered_lines.is_empty() {
                let lines: Vec<u64> = uncovered_lines.into_iter().collect();
                let mut current_start = lines[0];
                let mut prev_line = lines[0];

                for &curr_line in lines.iter().skip(1) {
                    if curr_line > prev_line + 1 {
                        push_range(&mut uncovered_ranges, current_start, prev_line);
                        current_start = curr_line;
                    }
                    prev_line = curr_line;
                }

                push_range(&mut uncovered_ranges, current_start, prev_line);
            }
        }

        if uncovered_count > 0 {
            let uncovered_str = uncovered_ranges.join(", ");
            let uncovered_display = if uncovered_str.len() < 50 {
                uncovered_str.clone()
            } else {
                format!("{}...", &uncovered_str[..47])
            };

            parsed_files.push(FileCoverage {
                filename: file_entry.filename.clone(),
                line_pct: line_cov,
                uncovered_count,
                uncovered_display,
                uncovered_ranges,
            });
        }
    }

    parsed_files.sort_by_key(|entry| Reverse(entry.uncovered_count));
    let top_files = parsed_files.into_iter().take(20).collect::<Vec<_>>();

    let mut uncovered_details = Vec::new();

    for entry in &top_files {
        writeln!(
            output,
            "| {} | {:.2}% | {} |",
            entry.filename, entry.line_pct, entry.uncovered_display
        )?;
        uncovered_details.push((
            entry.filename.clone(),
            entry.line_pct,
            entry.uncovered_ranges.clone(),
        ));
    }

    if !uncovered_details.is_empty() {
        writeln!(output, "\n## Uncovered Line Details\n")?;
        for (filename, percent, ranges) in uncovered_details {
            writeln!(output, "### {} ({:.2}%)\n", filename, percent)?;
            writeln!(output, "Uncovered: {}\n", ranges.join(", "))?;
        }
    }

    println!("Condensed coverage report generated at {}", output_path);
    Ok(())
}
