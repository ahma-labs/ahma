//! xtask — workspace automation for ahma.
//!
//! Run via: `cargo xtask <subcommand>`
//!
//! Subcommands:
//!   bump-version X.Y.Z   Update the workspace version across all version-bearing files.

use std::fs;
use std::path::{Path, PathBuf};
use std::process;

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("bump-version") => {
            let new_ver = args.next().unwrap_or_else(|| {
                eprintln!("Usage: cargo xtask bump-version X.Y.Z");
                process::exit(1);
            });
            bump_version(&new_ver);
        }
        Some(cmd) => {
            eprintln!("Unknown xtask command: {cmd}");
            eprintln!("Available commands:");
            eprintln!("  bump-version X.Y.Z    Update version across all files");
            process::exit(1);
        }
        None => {
            eprintln!("Usage: cargo xtask <command>");
            eprintln!("  bump-version X.Y.Z    Update version across all files");
            process::exit(1);
        }
    }
}

/// Walk up from the current directory to find the workspace root (the directory
/// containing a `Cargo.toml` with a `[workspace]` table).
fn workspace_root() -> PathBuf {
    let mut dir = std::env::current_dir().expect("Failed to get current directory");
    loop {
        let manifest = dir.join("Cargo.toml");
        if manifest.exists() {
            let text = fs::read_to_string(&manifest).unwrap_or_default();
            if text.contains("[workspace]") {
                return dir;
            }
        }
        dir = dir
            .parent()
            .unwrap_or_else(|| {
                eprintln!("Could not find workspace root (no Cargo.toml with [workspace] found)");
                process::exit(1);
            })
            .to_path_buf();
    }
}

fn bump_version(new_ver: &str) {
    // Validate semver X.Y.Z
    let parts: Vec<&str> = new_ver.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.parse::<u32>().is_err()) {
        eprintln!("ERROR: '{new_ver}' is not a valid semver (expected X.Y.Z)");
        process::exit(1);
    }

    let root = workspace_root();

    // Read current version from workspace Cargo.toml
    let cargo_toml_path = root.join("Cargo.toml");
    let cargo_content = fs::read_to_string(&cargo_toml_path).expect("Failed to read Cargo.toml");
    let cur_ver = cargo_content
        .lines()
        .find(|l| l.starts_with("version = \""))
        .expect("No version = \"...\" line found in Cargo.toml")
        .split('"')
        .nth(1)
        .expect("Unexpected Cargo.toml version format")
        .to_string();

    if cur_ver == new_ver {
        // Cargo.toml is already at the target version, but the scripts and skill file may
        // still be behind (e.g. the workspace version was bumped manually without running
        // this task).  Scan each file for any semver-looking version string and replace it.
        println!("Cargo.toml is already at {new_ver}; checking other files for stale versions…");
        let other_files: &[(&str, &str)] = &[
            ("skills/ahma/SKILL.md", "skills/ahma/SKILL.md"),
            ("scripts/install.sh", "scripts/install.sh"),
            ("scripts/install.ps1", "scripts/install.ps1"),
        ];
        let mut any_updated = false;
        for (rel_path, label) in other_files {
            let path = root.join(rel_path);
            if let Some(stale) = find_stale_version(&path, new_ver) {
                println!("  Updating {label}: {stale} → {new_ver}");
                replace_all_version_occurrences(&path, &stale, new_ver, label);
                any_updated = true;
            } else {
                println!("  {label}: already at {new_ver} ✓");
            }
        }
        if !any_updated {
            println!("All files already at {new_ver} — nothing to do.");
        }
        return;
    }

    println!("Bumping {cur_ver} → {new_ver}");
    println!();

    // 1. Cargo.toml — only replace lines that start with `version = "` (workspace package line)
    replace_anchored_line(
        &cargo_toml_path,
        "version = \"",
        &cur_ver,
        new_ver,
        "Cargo.toml",
    );

    // 2. skills/ahma/SKILL.md — YAML frontmatter and HTML comment
    let skill_path = root.join("skills/ahma/SKILL.md");
    replace_anchored_line(
        &skill_path,
        "version: ",
        &cur_ver,
        new_ver,
        "skills/ahma/SKILL.md (YAML)",
    );
    replace_substring(
        &skill_path,
        &format!("<!-- version: {cur_ver} |"),
        &format!("<!-- version: {new_ver} |"),
        "skills/ahma/SKILL.md (HTML comment)",
    );

    // 3. scripts/install.sh — AHMA_VERSION="X.Y.Z"
    let install_sh_path = root.join("scripts/install.sh");
    replace_substring(
        &install_sh_path,
        &format!("AHMA_VERSION=\"{cur_ver}\""),
        &format!("AHMA_VERSION=\"{new_ver}\""),
        "scripts/install.sh",
    );

    // 4. scripts/install.ps1 — Install-OneSkill ... -Version 'X.Y.Z'
    let install_ps1_path = root.join("scripts/install.ps1");
    replace_substring(
        &install_ps1_path,
        &format!("-Version '{cur_ver}'"),
        &format!("-Version '{new_ver}'"),
        "scripts/install.ps1",
    );

    println!();
    println!("Done. Version bumped to {new_ver}.");
    println!("Suggested commit:");
    println!("  git add Cargo.toml skills/ahma/SKILL.md scripts/install.sh scripts/install.ps1");
    println!("  git commit -m \"chore(release): bump version to {new_ver}\"");
}

/// Replace the first occurrence of `old` with `new` within lines that start with `prefix`.
/// Preserves the original file's trailing newline.
fn replace_anchored_line(path: &Path, prefix: &str, old: &str, new: &str, label: &str) {
    let content = fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("ERROR: Failed to read {}: {e}", path.display());
        process::exit(1);
    });
    let mut replaced = false;
    let new_content: String = content
        .lines()
        .map(|line| {
            if !replaced && line.starts_with(prefix) && line.contains(old) {
                replaced = true;
                line.replacen(old, new, 1)
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    // Preserve trailing newline
    let new_content = if content.ends_with('\n') {
        new_content + "\n"
    } else {
        new_content
    };
    if !replaced {
        eprintln!("WARNING: No line starting with '{prefix}' containing '{old}' found in {label}");
        return;
    }
    fs::write(path, new_content).unwrap_or_else(|e| {
        eprintln!("ERROR: Failed to write {}: {e}", path.display());
        process::exit(1);
    });
    println!("  OK {label}");
}

/// Replace the first exact substring occurrence of `old` with `new`.
fn replace_substring(path: &Path, old: &str, new: &str, label: &str) {
    let content = fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("ERROR: Failed to read {}: {e}", path.display());
        process::exit(1);
    });
    if !content.contains(old) {
        eprintln!("WARNING: Pattern '{old}' not found in {label} — skipping");
        return;
    }
    let new_content = content.replacen(old, new, 1);
    fs::write(path, new_content).unwrap_or_else(|e| {
        eprintln!("ERROR: Failed to write {}: {e}", path.display());
        process::exit(1);
    });
    println!("  OK {label}");
}

/// Scan `path` for a semver string that is not equal to `target`.
/// Returns the first stale version found, or `None` if the file is already current.
fn find_stale_version(path: &Path, target: &str) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    // Match bare semver patterns, e.g. 0.7.2 or 1.12.3, that differ from `target`.
    let re = regex::Regex::new(r"\b(\d+\.\d+\.\d+)\b").expect("static regex");
    for cap in re.captures_iter(&content) {
        let ver = cap[1].to_string();
        if ver != target {
            return Some(ver);
        }
    }
    None
}

/// Replace ALL occurrences of `old` with `new` in `path`.
fn replace_all_version_occurrences(path: &Path, old: &str, new: &str, label: &str) {
    let content = fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("ERROR: Failed to read {}: {e}", path.display());
        process::exit(1);
    });
    let new_content = content.replace(old, new);
    fs::write(path, new_content).unwrap_or_else(|e| {
        eprintln!("ERROR: Failed to write {}: {e}", path.display());
        process::exit(1);
    });
    println!("  OK {label}");
}
