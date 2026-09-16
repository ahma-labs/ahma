//! End-to-end coverage for the dead-code lens: real AST parsing of files on
//! disk, through the detector, into the rendered report section.

use ahma_simplify::analysis::dead_code::find_dead_symbols;
use ahma_simplify::analysis::source_tree::parse_source_tree;
use ahma_simplify::models::MultiLensReport;
use ahma_simplify::report::create_report_md;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

fn project(files: &[(&str, &str)]) -> (TempDir, Vec<(PathBuf, String)>) {
    let dir = TempDir::new().expect("tempdir");
    let mut sources = Vec::new();
    for (name, body) in files {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture dirs");
        }
        fs::write(&path, body).expect("fixture write");
        sources.push((path, (*body).to_string()));
    }
    (dir, sources)
}

fn analyze(sources: &[(PathBuf, String)]) -> Vec<ahma_simplify::models::DeadSymbol> {
    let trees: Vec<_> = sources
        .iter()
        .filter_map(|(path, _)| parse_source_tree(path))
        .collect();
    find_dead_symbols(sources, &trees)
}

#[test]
fn an_unreferenced_export_is_reported_and_a_called_one_is_not() {
    let (dir, sources) = project(&[
        (
            "lib.rs",
            "pub fn used_helper() -> u32 {\n    7\n}\n\npub fn orphaned_helper() -> u32 {\n    9\n}\n",
        ),
        (
            "consumer.rs",
            "pub fn entry() -> u32 {\n    used_helper() + 1\n}\n",
        ),
    ]);

    let dead = analyze(&sources);
    let names: Vec<&str> = dead.iter().map(|s| s.name.as_str()).collect();

    assert!(
        names.contains(&"orphaned_helper"),
        "unreferenced export should be a candidate: {names:?}"
    );
    assert!(
        !names.contains(&"used_helper"),
        "a called export must not be reported: {names:?}"
    );

    let reported = dead
        .iter()
        .find(|s| s.name == "orphaned_helper")
        .expect("candidate present");
    assert_eq!(reported.references_found, 0);

    let report = create_report_md(
        &MultiLensReport {
            dead_symbols: dead,
            ..Default::default()
        },
        false,
        50,
        dir.path(),
        "dead-code-fixture",
    );
    assert!(report.contains("## Possibly Unreferenced Exports (Dead Code Lens)"));
    assert!(report.contains("orphaned_helper"));
    // The caveat is load-bearing: this lens cannot see downstream crates, macro
    // call sites, or trait-object dispatch, and a reader who deletes on its word
    // alone will eventually delete live code.
    assert!(report.contains("Verify each before removing anything"));
}

#[test]
fn private_functions_are_never_candidates() {
    let (_dir, sources) = project(&[(
        "internal.rs",
        "fn hidden_helper() -> u32 {\n    3\n}\n\npub fn api() -> u32 {\n    hidden_helper()\n}\n",
    )]);

    let names: Vec<String> = analyze(&sources).into_iter().map(|s| s.name).collect();
    assert!(
        !names.contains(&"hidden_helper".to_string()),
        "private fns are rustc's job, not this lens's: {names:?}"
    );
}

#[test]
fn results_are_deterministic_across_runs() {
    let (_dir, sources) = project(&[
        (
            "a.rs",
            "pub fn alpha_orphan() {}\npub fn beta_orphan() {}\n",
        ),
        ("b.rs", "pub fn gamma_orphan() {}\n"),
    ]);

    assert_eq!(analyze(&sources), analyze(&sources));
}

#[test]
fn an_empty_result_renders_no_section() {
    let report = create_report_md(
        &MultiLensReport::default(),
        false,
        50,
        std::path::Path::new("."),
        "empty",
    );
    assert!(!report.contains("Possibly Unreferenced Exports"));
}
