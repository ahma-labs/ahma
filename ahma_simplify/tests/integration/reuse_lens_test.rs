//! End-to-end coverage for the reuse lens: a real directory scan feeding the
//! duplicate detector, and the report section it renders.

use ahma_simplify::analysis::reuse::{ReuseConfig, find_duplicates};
use ahma_simplify::models::MultiLensReport;
use ahma_simplify::report::create_report_md;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

const SHARED_BLOCK: &str = r#"    let severity = match raw.as_str() {
        "error" => Severity::Error,
        "info" => Severity::Info,
        _ => Severity::Warning,
    };
    Some(severity)
"#;

fn write(dir: &TempDir, name: &str, body: &str) -> PathBuf {
    let path = dir.path().join(name);
    fs::write(&path, body).expect("fixture write");
    path
}

fn read_sources(paths: &[PathBuf]) -> Vec<(PathBuf, String)> {
    paths
        .iter()
        .map(|p| (p.clone(), fs::read_to_string(p).expect("fixture read")))
        .collect()
}

#[test]
fn duplicate_block_across_two_files_reaches_the_report() {
    let dir = TempDir::new().expect("tempdir");
    let a = write(
        &dir,
        "parser_a.rs",
        &format!("pub fn parse_a(raw: String) -> Option<Severity> {{\n{SHARED_BLOCK}}}\n"),
    );
    let b = write(
        &dir,
        "parser_b.rs",
        &format!("pub fn parse_b(raw: String) -> Option<Severity> {{\n{SHARED_BLOCK}}}\n"),
    );

    let groups = find_duplicates(
        &read_sources(&[a.clone(), b.clone()]),
        &ReuseConfig::default(),
    );
    assert_eq!(groups.len(), 1, "expected exactly one duplicate group");

    let group = &groups[0];
    assert_eq!(group.locations.len(), 2);
    assert!(group.sample_text.contains("Severity::Error"));

    let located: Vec<&PathBuf> = group.locations.iter().map(|l| &l.file).collect();
    assert!(located.contains(&&a) && located.contains(&&b));

    let report = create_report_md(
        &MultiLensReport {
            duplicates: groups,
            ..Default::default()
        },
        false,
        50,
        dir.path(),
        "reuse-fixture",
    );
    assert!(report.contains("## Duplicate Code (Reuse Lens)"));
    assert!(report.contains("parser_a.rs"));
    // The framing is load-bearing: a report that reads as a verdict gets acted
    // on as one, and duplicate blocks are only ever extraction candidates.
    assert!(report.contains("candidates for extraction, not confirmed problems"));
}

#[test]
fn distinct_files_produce_no_duplicate_section() {
    let dir = TempDir::new().expect("tempdir");
    let a = write(
        &dir,
        "alpha.rs",
        "pub fn alpha() -> u32 {\n    let a = 1;\n    let b = 2;\n    a + b\n}\n",
    );
    let b = write(
        &dir,
        "beta.rs",
        "pub fn beta(v: &[u32]) -> u32 {\n    v.iter().copied().max().unwrap_or_default()\n}\n",
    );

    let groups = find_duplicates(&read_sources(&[a, b]), &ReuseConfig::default());
    assert!(
        groups.is_empty(),
        "unrelated files must not match: {groups:?}"
    );

    let report = create_report_md(
        &MultiLensReport::default(),
        false,
        50,
        dir.path(),
        "no-duplicates",
    );
    assert!(!report.contains("Duplicate Code"));
}

#[test]
fn comments_and_formatting_do_not_hide_a_duplicate() {
    let dir = TempDir::new().expect("tempdir");
    let plain = write(
        &dir,
        "plain.rs",
        "fn run() {\n    let a = collect();\n    let b = transform(a);\n    let c = persist(b);\n    finish(c);\n}\n",
    );
    let decorated = write(
        &dir,
        "decorated.rs",
        "fn run_again() {\n    let a   =  collect(); // gather\n    /* step two */\n    let b = transform(a);\n    let c = persist(b);\n    finish(c);\n}\n",
    );

    let groups = find_duplicates(&read_sources(&[plain, decorated]), &ReuseConfig::default());
    assert_eq!(
        groups.len(),
        1,
        "comments and spacing must not defeat matching: {groups:?}"
    );
}
