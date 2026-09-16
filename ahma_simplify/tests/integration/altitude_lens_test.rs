//! End-to-end coverage for the altitude lens: real Rust source parsed from disk,
//! through chain detection, into the rendered report section.

use ahma_simplify::analysis::altitude::find_altitude_chains;
use ahma_simplify::analysis::source_tree::parse_source_tree;
use ahma_simplify::models::MultiLensReport;
use ahma_simplify::report::create_report_md;
use std::fs;
use tempfile::TempDir;

fn chains_for(source: &str) -> (TempDir, Vec<ahma_simplify::models::AltitudeChain>) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("lib.rs");
    fs::write(&path, source).expect("fixture write");
    let trees: Vec<_> = parse_source_tree(&path).into_iter().collect();
    let chains = find_altitude_chains(&trees);
    (dir, chains)
}

#[test]
fn a_forwarding_chain_reaches_the_report() {
    let (dir, chains) = chains_for(
        "pub fn outer(v: u32) -> u32 {\n    middle(v);\n}\n\
         \npub fn middle(v: u32) -> u32 {\n    inner(v);\n}\n\
         \npub fn inner(v: u32) -> u32 {\n    let doubled = v * 2;\n    let shifted = doubled + 1;\n    shifted\n}\n",
    );

    assert_eq!(chains.len(), 1, "expected one maximal chain: {chains:?}");
    assert_eq!(chains[0].depth, 2);
    assert_eq!(chains[0].calls[0].caller, "outer");
    assert_eq!(chains[0].calls[1].caller, "middle");

    let report = create_report_md(
        &MultiLensReport {
            altitude_chains: chains,
            ..Default::default()
        },
        false,
        50,
        dir.path(),
        "altitude-fixture",
    );
    assert!(report.contains("## Delegation Chains (Altitude Lens)"));
    assert!(report.contains("outer"));
    // Forwarding is frequently a deliberate facade, so the section must not read
    // as a defect list.
    assert!(report.contains("forwarding is often deliberate"));
}

#[test]
fn a_single_forwarding_hop_is_ordinary_delegation() {
    let (_dir, chains) = chains_for(
        "pub fn outer(v: u32) -> u32 {\n    inner(v);\n}\n\
         \npub fn inner(v: u32) -> u32 {\n    let a = v + 1;\n    let b = a * 3;\n    b\n}\n",
    );
    assert!(
        chains.is_empty(),
        "one hop is normal delegation, not an altitude finding: {chains:?}"
    );
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
    assert!(!report.contains("Delegation Chains"));
}
