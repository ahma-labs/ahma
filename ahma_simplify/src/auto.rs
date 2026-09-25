//! Prioritized multi-lens fix planning (`ahma simplify --auto`).
//!
//! Implements `--auto [COUNT]`: runs all analysis lenses (complexity, reuse,
//! dead-code, altitude), prioritizes candidate fixes across all lenses by
//! estimated impact, and generates actionable, step-by-step fix instructions
//! for the top N most valuable fixes (defaulting to 10).

use crate::analysis::lens::Lens;
use crate::analysis::paths::get_relative_path;
use crate::models::{MultiLensReport, SymbolKind};
use std::cmp::Ordering;
use std::path::Path;

/// A prioritized fix candidate from any analysis lens.
#[derive(Debug, Clone, PartialEq)]
pub struct PrioritizedFix {
    /// 1-indexed overall rank (1 = highest impact/value).
    pub rank: usize,
    /// Originating analysis lens.
    pub lens: Lens,
    /// Target location (e.g. file path, symbol, line range, or delegation edge).
    pub target: String,
    /// Calibrated impact score (higher = more valuable to fix).
    pub value_score: f64,
    /// Short human-readable headline.
    pub title: String,
    /// One-line metric or findings summary.
    pub summary: String,
    /// Concrete, step-by-step actionable instructions for implementing the fix.
    pub action_plan: String,
}

/// Collect and prioritize candidate fixes across all lenses from a [`MultiLensReport`].
///
/// Evaluates:
/// - Complexity: files with low simplicity scores or high peak cognitive complexity
/// - Reuse: duplicate code blocks, weighted by duplicated lines of code wasted
/// - Altitude: delegation/forwarding chains, weighted by hop depth
/// - Dead Code: unreferenced exported functions and types
///
/// Sorts descending by `value_score` and returns the top `limit` fixes with 1-based ranks.
pub fn prioritize_fixes(
    report: &MultiLensReport,
    limit: usize,
    base_dir: &Path,
) -> Vec<PrioritizedFix> {
    if limit == 0 {
        return Vec::new();
    }

    let mut candidates: Vec<PrioritizedFix> = Vec::new();

    // 1. Complexity Lens candidates
    for file in &report.complexity_files {
        // Only prioritize files that have meaningful complexity or hotspots
        if file.score < 90.0 || file.peak_cognitive >= 10.0 || !file.hotspots.is_empty() {
            let base_val = (100.0 - file.score).max(5.0);
            let peak_val = file.peak_cognitive * 1.5;
            let value_score = base_val + peak_val;

            let rel_path = get_relative_path(Path::new(&file.path), base_dir);
            let rel_str = rel_path.display().to_string();

            let mut hotspots_desc = String::new();
            if !file.hotspots.is_empty() {
                hotspots_desc.push_str("\nHOTSPOT FUNCTIONS TO REFACTOR:\n");
                for (h_idx, h) in file.hotspots.iter().enumerate() {
                    hotspots_desc.push_str(&format!(
                        "  {}. `{}` (lines {}-{}, cognitive: {:.0}, cyclomatic: {:.0}, SLOC: {:.0})\n",
                        h_idx + 1,
                        h.name,
                        h.start_line,
                        h.end_line,
                        h.cognitive,
                        h.cyclomatic,
                        h.sloc
                    ));
                }
            }

            let culprit = crate::report::identify_culprit(file);
            let action_plan = format!(
                "TARGET: {}\n\
                 SIMPLICITY: {:.0}% | CULPRIT: {}\n\
                 METRICS: Cognitive={:.0}, PeakCognitive={:.0}, MI={:.1}, SLOC={:.0}{}\n\n\
                 ACTION STEPS:\n\
                 1. Read the target file and the listed hotspot functions.\n\
                 2. Evaluate critically: Is complexity volume-driven (many match arms, config fields)\n\
                    or tangled control flow / deep nesting?\n\
                 3. If genuinely complex, edit ONLY the listed hotspot functions:\n\
                    - Introduce early returns and guard clauses to reduce nesting depth\n\
                    - Extract well-scoped helper functions with clear, single responsibilities\n\
                    - Preserve public signatures and observable behaviors\n\
                 4. Verify with compiler checks and test suite (`cargo nextest run`).\n\
                 5. Re-check improvement with `ahma simplify . --verify {}`.",
                rel_str,
                file.score,
                culprit,
                file.cognitive,
                file.peak_cognitive,
                file.mi,
                file.sloc,
                hotspots_desc,
                rel_str
            );

            candidates.push(PrioritizedFix {
                rank: 0,
                lens: Lens::Complexity,
                target: rel_str.clone(),
                value_score,
                title: format!("Refactor complexity hotspots in {}", rel_str),
                summary: format!(
                    "Simplicity: {:.0}%, Peak Cognitive: {:.0}, MI: {:.1}, SLOC: {:.0}",
                    file.score, file.peak_cognitive, file.mi, file.sloc
                ),
                action_plan,
            });
        }
    }

    // 2. Reuse Lens candidates (Duplicate Code Blocks)
    for group in &report.duplicates {
        let copies = group.locations.len();
        let wasted_lines = copies.saturating_sub(1) * group.line_count;
        let value_score = (wasted_lines as f64) * 2.0;

        let mut loc_strings = Vec::new();
        for loc in &group.locations {
            let rel = get_relative_path(&loc.file, base_dir);
            loc_strings.push(format!(
                "{}:{}-{}",
                rel.display(),
                loc.start_line,
                loc.end_line
            ));
        }
        let target_str = loc_strings.join(", ");

        let mut loc_list_md = String::new();
        for loc_str in &loc_strings {
            loc_list_md.push_str(&format!("  - `{}`\n", loc_str));
        }

        let sample_preview = if group.sample_text.lines().count() > 10 {
            let first_lines: Vec<&str> = group.sample_text.lines().take(10).collect();
            format!(
                "{}\n    ... ({} more lines)",
                first_lines.join("\n"),
                group.line_count.saturating_sub(10)
            )
        } else {
            group.sample_text.clone()
        };

        let action_plan = format!(
            "TARGET LOCATIONS ({} copies, {} lines each, {} lines wasted):\n{}\n\
             DUPLICATED BLOCK SAMPLE:\n```\n{}\n```\n\n\
             ACTION STEPS:\n\
             1. Examine the duplicated code across all {} locations.\n\
             2. Verify if this is boilerplate test setup or shared business/helper logic.\n\
             3. If shared logic, extract a single helper function into a common utility or parent module.\n\
             4. Replace each duplicate occurrence with a call to the extracted helper.\n\
             5. Verify all tests pass with `cargo nextest run`.",
            copies, group.line_count, wasted_lines, loc_list_md, sample_preview, copies
        );

        candidates.push(PrioritizedFix {
            rank: 0,
            lens: Lens::Reuse,
            target: target_str,
            value_score,
            title: format!(
                "Extract duplicate code block ({} lines × {} copies)",
                group.line_count, copies
            ),
            summary: format!(
                "{} lines duplicated across {} locations ({} lines wasted)",
                group.line_count, copies, wasted_lines
            ),
            action_plan,
        });
    }

    // 3. Altitude Lens candidates (Thin-Wrapper Delegation Chains)
    for chain in &report.altitude_chains {
        let value_score = (chain.depth as f64) * 25.0;

        let first_caller = chain
            .calls
            .first()
            .map(|c| c.caller.as_str())
            .unwrap_or("unknown");
        let last_callee = chain
            .calls
            .last()
            .map(|c| c.callee.as_str())
            .unwrap_or("unknown");
        let target_str = format!("{} -> ... -> {}", first_caller, last_callee);

        let mut chain_steps = String::new();
        for (hop_idx, call) in chain.calls.iter().enumerate() {
            let rel = get_relative_path(&call.file, base_dir);
            chain_steps.push_str(&format!(
                "  Hop {}: `{}` → `{}` at `{}:{}`\n",
                hop_idx + 1,
                call.caller,
                call.callee,
                rel.display(),
                call.line
            ));
        }

        let action_plan = format!(
            "DELEGATION CHAIN ({} hops):\n{}\n\
             SUMMARY: {}\n\n\
             ACTION STEPS:\n\
             1. Verify if the intermediate forwarding wrappers provide intentional abstractions\n\
                (e.g. public API facade, trait delegation, or platform shim).\n\
             2. If they are redundant thin forwarding layers, update callers to invoke\n\
                the destination function `{}` directly.\n\
             3. Remove or inline the unnecessary intermediate forwarding functions.\n\
             4. Verify that compiler checks and tests pass with `cargo nextest run`.",
            chain.depth, chain_steps, chain.description, last_callee
        );

        candidates.push(PrioritizedFix {
            rank: 0,
            lens: Lens::Altitude,
            target: target_str,
            value_score,
            title: format!(
                "Collapse {}-hop delegation chain: {} -> {}",
                chain.depth, first_caller, last_callee
            ),
            summary: chain.description.clone(),
            action_plan,
        });
    }

    // 4. Dead Code Lens candidates (Unreferenced Exports)
    for sym in &report.dead_symbols {
        let value_score = match sym.kind {
            SymbolKind::Function | SymbolKind::Method => 35.0,
            SymbolKind::Struct
            | SymbolKind::Enum
            | SymbolKind::Trait
            | SymbolKind::Class
            | SymbolKind::Interface => 25.0,
            _ => 20.0,
        };

        let rel_file = get_relative_path(&sym.file, base_dir);
        let target_str = format!("{}:{}", rel_file.display(), sym.line);

        let action_plan = format!(
            "SYMBOL: {} `{}` at {}\n\
             VISIBILITY: {}\n\n\
             ACTION STEPS:\n\
             1. Confirm whether `{}` is used by downstream crates, reflection,\n\
                macro-generated code, trait-object dispatch, or integration tests.\n\
             2. If it is internal dead code, delete the symbol or reduce its visibility\n\
                (e.g. `pub(crate)` or private).\n\
             3. Verify with `cargo check` and `cargo nextest run`.",
            sym.kind.label(),
            sym.name,
            target_str,
            sym.visibility,
            sym.name
        );

        candidates.push(PrioritizedFix {
            rank: 0,
            lens: Lens::DeadCode,
            target: target_str,
            value_score,
            title: format!("Remove unreferenced {} `{}`", sym.kind.label(), sym.name),
            summary: format!(
                "Exported {} `{}` has no references in scanned files",
                sym.kind.label(),
                sym.name
            ),
            action_plan,
        });
    }

    // Sort descending by value_score; tie-break on target for deterministic order
    candidates.sort_by(|a, b| {
        b.value_score
            .partial_cmp(&a.value_score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.target.cmp(&b.target))
    });

    let mut top_fixes: Vec<PrioritizedFix> = candidates.into_iter().take(limit).collect();
    for (idx, fix) in top_fixes.iter_mut().enumerate() {
        fix.rank = idx + 1;
    }

    top_fixes
}

/// Render the prioritized auto-simplification report as Markdown.
pub fn create_auto_report_md(
    report: &MultiLensReport,
    fixes: &[PrioritizedFix],
    limit: usize,
    _base_dir: &Path,
    project_name: &str,
) -> String {
    let mut out = String::new();

    if fixes.is_empty() {
        out.push_str(&format!(
            "# Auto Simplification Plan: No Issues Found\n\n\
             Ran all analysis lenses (**complexity**, **reuse**, **dead-code**, **altitude**) \
             across project `{}`.\n\n\
             No complexity hotspots, duplicate code blocks, delegation chains, or \
             unreferenced exports were found.\n\n\
             The codebase is clean!\n",
            project_name
        ));
        return out;
    }

    let total_candidates = report.total_issues();

    out.push_str(&format!(
        "# Auto Simplification Plan: Top {} Most Valuable Fixes\n\n\
         Ran all analysis lenses (**complexity**, **reuse**, **dead-code**, **altitude**).\n\
         Identified **{}** total candidate finding(s) across project `{}`.\n\
         Prioritized the top **{}** most valuable fix(es) based on maintenance impact and code simplicity:\n\n",
        fixes.len(),
        total_candidates,
        project_name,
        fixes.len()
    ));

    // Summary table
    out.push_str("## Prioritized Fixes Overview\n\n");
    out.push_str("| Rank | Lens | Impact | Target | Summary |\n");
    out.push_str("|:---:|:---:|:---:|:---|:---|\n");

    for fix in fixes {
        let lens_str = match fix.lens {
            Lens::Complexity => "Complexity",
            Lens::Reuse => "Reuse",
            Lens::DeadCode => "DeadCode",
            Lens::Altitude => "Altitude",
        };
        out.push_str(&format!(
            "| #{} | {} | {:.1} | `{}` | {} |\n",
            fix.rank, lens_str, fix.value_score, fix.target, fix.summary
        ));
    }
    out.push('\n');

    // Detailed action plans
    out.push_str("## Actionable Implementation Plans\n\n");

    for fix in fixes {
        let lens_str = match fix.lens {
            Lens::Complexity => "Complexity",
            Lens::Reuse => "Reuse",
            Lens::DeadCode => "DeadCode",
            Lens::Altitude => "Altitude",
        };

        out.push_str(&format!(
            "### Fix #{} [{}] — {} (Impact: {:.1})\n\n\
             {}\n\n\
             ---\n\n",
            fix.rank, lens_str, fix.title, fix.value_score, fix.action_plan
        ));
    }

    if total_candidates > limit {
        out.push_str(&format!(
            "*Note: {} additional candidate issue(s) not included in this plan. \
             Rerun with `--auto {}` to view more fixes.*\n",
            total_candidates - limit,
            total_candidates
        ));
    }

    out
}
