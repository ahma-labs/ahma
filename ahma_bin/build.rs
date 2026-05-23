//! Build script for ahma_bin.
//!
//! Compile-time guard: fails the build if any version-bearing file is out of
//! sync with the workspace version in Cargo.toml.
//!
//! Re-triggers automatically when any of the watched files change, so stale
//! versions are caught on the very next `cargo build` or `cargo check`.
//!
//! To fix: run `cargo xtask bump-version X.Y.Z` where X.Y.Z matches Cargo.toml.

use std::path::Path;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let workspace_root = Path::new(&manifest_dir)
        .parent()
        .expect("ahma_bin has no parent directory — unexpected workspace layout");

    // Re-run this build script when any version-bearing file changes.
    for rel in &[
        "scripts/install.sh",
        "scripts/install.ps1",
        "skills/ahma/SKILL.md",
    ] {
        println!(
            "cargo:rerun-if-changed={}",
            workspace_root.join(rel).display()
        );
    }

    let ver = env!("CARGO_PKG_VERSION");

    check_file(
        &workspace_root.join("scripts/install.sh"),
        &format!("AHMA_VERSION=\"{ver}\""),
        "scripts/install.sh",
        ver,
    );

    check_file(
        &workspace_root.join("scripts/install.ps1"),
        &format!("-Version '{ver}'"),
        "scripts/install.ps1",
        ver,
    );

    check_file(
        &workspace_root.join("skills/ahma/SKILL.md"),
        &format!("version: {ver}"),
        "skills/ahma/SKILL.md",
        ver,
    );
}

fn check_file(path: &Path, expected: &str, label: &str, ver: &str) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            // Non-fatal if the file is missing in a stripped source tree.
            println!("cargo:warning=Could not read {label}: {e}");
            return;
        }
    };
    if !content.contains(expected) {
        panic!(
            "\n\nVERSION MISMATCH: {label} is out of sync with Cargo.toml (v{ver}).\n\
             Expected to find:  {expected}\n\
             Fix: cargo xtask bump-version {ver}\n"
        );
    }
}
