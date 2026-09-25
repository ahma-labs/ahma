//! Tests for prioritized multi-lens auto planning (`ahma simplify --auto`).

use ahma_simplify::analysis::lens::Lens;
use ahma_simplify::auto::{create_auto_report_md, prioritize_fixes};
use ahma_simplify::models::{
    AltitudeCall, AltitudeChain, AnalysisConfidence, DeadSymbol, DuplicateGroup, DuplicateLocation,
    FileSimplicity, FunctionHotspot, Language, MultiLensReport, SymbolKind,
};
use std::path::{Path, PathBuf};

fn make_complexity_file(
    path: &str,
    score: f64,
    peak_cognitive: f64,
    cognitive: f64,
    sloc: f64,
) -> FileSimplicity {
    FileSimplicity {
        path: path.to_string(),
        language: Language::Rust,
        score,
        cognitive,
        cyclomatic: 10.0,
        sloc,
        mi: 65.0,
        peak_cognitive,
        hotspots: vec![FunctionHotspot {
            name: "complex_fn".to_string(),
            start_line: 10,
            end_line: 40,
            cognitive: peak_cognitive,
            cyclomatic: 8.0,
            sloc: 30.0,
        }],
        external_issues: Vec::new(),
        analysis_sources: vec!["rca".to_string()],
        confidence: AnalysisConfidence::Full,
    }
}

#[test]
fn test_prioritize_fixes_empty_report() {
    let report = MultiLensReport::default();
    let fixes = prioritize_fixes(&report, 10, Path::new("."));
    assert!(fixes.is_empty());

    let md = create_auto_report_md(&report, &fixes, 10, Path::new("."), "my_proj");
    assert!(md.contains("No Issues Found"));
    assert!(md.contains("The codebase is clean!"));
}

#[test]
fn test_prioritize_fixes_ranking_order() {
    let mut report = MultiLensReport::default();

    // 1. High-complexity file: score 30%, peak_cog 25 -> value = 70 + 37.5 = 107.5
    report.complexity_files.push(make_complexity_file(
        "src/complex.rs",
        30.0,
        25.0,
        40.0,
        350.0,
    ));

    // 2. Mild complexity file: score 85%, peak_cog 2 -> value = 15 + 3 = 18.0
    report
        .complexity_files
        .push(make_complexity_file("src/mild.rs", 85.0, 2.0, 5.0, 100.0));

    // 3. Duplicate code block: 20 lines × 4 locations = 60 wasted lines -> value = 120.0
    report.duplicates.push(DuplicateGroup {
        hash: 12345,
        line_count: 20,
        locations: vec![
            DuplicateLocation {
                file: PathBuf::from("src/a.rs"),
                start_line: 10,
                end_line: 30,
            },
            DuplicateLocation {
                file: PathBuf::from("src/b.rs"),
                start_line: 50,
                end_line: 70,
            },
            DuplicateLocation {
                file: PathBuf::from("src/c.rs"),
                start_line: 100,
                end_line: 120,
            },
            DuplicateLocation {
                file: PathBuf::from("src/d.rs"),
                start_line: 200,
                end_line: 220,
            },
        ],
        sample_text: "fn duplicate_helper() {\n    // block\n}".to_string(),
    });

    // 4. Altitude chain: 3 hops -> value = 3 * 25.0 = 75.0
    report.altitude_chains.push(AltitudeChain {
        calls: vec![
            AltitudeCall {
                caller: "entry".to_string(),
                callee: "middle".to_string(),
                file: PathBuf::from("src/proxy.rs"),
                line: 15,
            },
            AltitudeCall {
                caller: "middle".to_string(),
                callee: "inner".to_string(),
                file: PathBuf::from("src/proxy.rs"),
                line: 30,
            },
            AltitudeCall {
                caller: "inner".to_string(),
                callee: "target".to_string(),
                file: PathBuf::from("src/proxy.rs"),
                line: 45,
            },
        ],
        depth: 3,
        description: "entry -> middle -> inner -> target (3 hops)".to_string(),
    });

    // 5. Dead function: value = 35.0
    report.dead_symbols.push(DeadSymbol {
        name: "unused_func".to_string(),
        kind: SymbolKind::Function,
        file: PathBuf::from("src/unused.rs"),
        line: 42,
        visibility: "pub".to_string(),
        references_found: 1,
    });

    // 6. Dead struct: value = 25.0
    report.dead_symbols.push(DeadSymbol {
        name: "UnusedType".to_string(),
        kind: SymbolKind::Struct,
        file: PathBuf::from("src/types.rs"),
        line: 88,
        visibility: "pub".to_string(),
        references_found: 1,
    });

    let fixes = prioritize_fixes(&report, 10, Path::new("."));
    assert_eq!(fixes.len(), 6);

    // Verify ranks and lens order
    assert_eq!(fixes[0].rank, 1);
    assert_eq!(fixes[0].lens, Lens::Reuse);
    assert_eq!(fixes[0].value_score, 120.0);

    assert_eq!(fixes[1].rank, 2);
    assert_eq!(fixes[1].lens, Lens::Complexity);
    assert_eq!(fixes[1].value_score, 107.5);
    assert_eq!(fixes[1].target, "src/complex.rs");

    assert_eq!(fixes[2].rank, 3);
    assert_eq!(fixes[2].lens, Lens::Altitude);
    assert_eq!(fixes[2].value_score, 75.0);

    assert_eq!(fixes[3].rank, 4);
    assert_eq!(fixes[3].lens, Lens::DeadCode);
    assert_eq!(fixes[3].value_score, 35.0);
    assert!(fixes[3].title.contains("unused_func"));

    assert_eq!(fixes[4].rank, 5);
    assert_eq!(fixes[4].lens, Lens::DeadCode);
    assert_eq!(fixes[4].value_score, 25.0);
    assert!(fixes[4].title.contains("UnusedType"));

    assert_eq!(fixes[5].rank, 6);
    assert_eq!(fixes[5].lens, Lens::Complexity);
    assert_eq!(fixes[5].value_score, 18.0);
    assert_eq!(fixes[5].target, "src/mild.rs");
}

#[test]
fn test_prioritize_fixes_limit_cutoff() {
    let mut report = MultiLensReport::default();
    report
        .complexity_files
        .push(make_complexity_file("src/a.rs", 30.0, 20.0, 40.0, 200.0));
    report
        .complexity_files
        .push(make_complexity_file("src/b.rs", 40.0, 15.0, 30.0, 200.0));
    report
        .complexity_files
        .push(make_complexity_file("src/c.rs", 50.0, 10.0, 20.0, 200.0));

    let fixes = prioritize_fixes(&report, 2, Path::new("."));
    assert_eq!(fixes.len(), 2);
    assert_eq!(fixes[0].rank, 1);
    assert_eq!(fixes[1].rank, 2);
}

#[test]
fn test_create_auto_report_md_content() {
    let mut report = MultiLensReport::default();
    report
        .complexity_files
        .push(make_complexity_file("src/core.rs", 25.0, 30.0, 50.0, 400.0));

    let fixes = prioritize_fixes(&report, 5, Path::new("."));
    let md = create_auto_report_md(&report, &fixes, 5, Path::new("."), "my_app");

    assert!(md.contains("# Auto Simplification Plan: Top 1 Most Valuable Fixes"));
    assert!(md.contains("## Prioritized Fixes Overview"));
    assert!(md.contains("| #1 | Complexity |"));
    assert!(md.contains("## Actionable Implementation Plans"));
    assert!(md.contains("### Fix #1 [Complexity]"));
    assert!(md.contains("ACTION STEPS:"));
}
