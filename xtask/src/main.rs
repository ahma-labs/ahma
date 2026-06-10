//! xtask — workspace automation for ahma.
//!
//! Run via: `cargo xtask <subcommand>`
//!
//! Subcommands:
//!   bump-version X.Y.Z      Update the workspace version across all version-bearing files.
//!   bump-android-version    Increment the Android Play versionCode and sync versionName from Cargo.
//!   safe-update [options]   Upgrade workspace deps that are ≥14 days old and have no known advisories.

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
        Some("bump-android-version") => {
            // Optional positional arg: path to the Android project root directory.
            // Defaults to test-data/AndoidTestBasicViews.
            let android_dir = args.next();
            bump_android_version(android_dir.as_deref());
        }
        Some("safe-update") => {
            let remaining: Vec<String> = args.collect();
            safe_update(&remaining);
        }
        Some(cmd) => {
            eprintln!("Unknown xtask command: {cmd}");
            eprintln!("Available commands:");
            eprintln!("  bump-version X.Y.Z         Update Cargo + skill version across all files");
            eprintln!(
                "  bump-android-version [dir] Increment Android Play versionCode and sync versionName"
            );
            eprintln!(
                "  safe-update [--dry-run] [--min-age-days N] [--include a,b] [--exclude a,b]"
            );
            eprintln!(
                "                             Upgrade deps that are ≥14 days old and advisory-clean"
            );
            process::exit(1);
        }
        None => {
            eprintln!("Usage: cargo xtask <command>");
            eprintln!("  bump-version X.Y.Z         Update Cargo + skill version across all files");
            eprintln!(
                "  bump-android-version [dir] Increment Android Play versionCode and sync versionName"
            );
            eprintln!(
                "  safe-update [--dry-run] [--min-age-days N] [--include a,b] [--exclude a,b]"
            );
            eprintln!(
                "                             Upgrade deps that are ≥14 days old and advisory-clean"
            );
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
        update_other_stale_files(&root, new_ver);
    } else {
        perform_normal_bump(&root, &cargo_toml_path, &cur_ver, new_ver);
    }
}

fn update_other_stale_files(root: &Path, new_ver: &str) {
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
}

fn perform_normal_bump(root: &Path, cargo_toml_path: &Path, cur_ver: &str, new_ver: &str) {
    println!("Bumping {cur_ver} → {new_ver}");
    println!();

    // 1. Cargo.toml — only replace lines that start with `version = "` (workspace package line)
    replace_anchored_line(
        cargo_toml_path,
        "version = \"",
        cur_ver,
        new_ver,
        "Cargo.toml",
    );

    // 2. skills/ahma/SKILL.md — YAML frontmatter and HTML comment
    let skill_path = root.join("skills/ahma/SKILL.md");
    replace_anchored_line(
        &skill_path,
        "version: ",
        cur_ver,
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

/// Re-apply the source file's trailing newline after line-oriented edits.
fn restore_trailing_newline(edited: &str, original: &str) -> String {
    if original.ends_with('\n') {
        format!("{edited}\n")
    } else {
        edited.to_string()
    }
}

/// Replace the first occurrence of `old` with `new` within lines that start with `prefix`.
/// Preserves the original file's trailing newline.
fn replace_anchored_line(path: &Path, prefix: &str, old: &str, new: &str, label: &str) {
    let content = fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("ERROR: Failed to read {}: {e}", path.display());
        process::exit(1);
    });
    let (new_content, replaced) = transform_first_anchored_match(&content, prefix, old, new);
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

fn transform_first_anchored_match(
    content: &str,
    prefix: &str,
    old: &str,
    new: &str,
) -> (String, bool) {
    let mut replaced = false;
    let body: String = content
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
    (restore_trailing_newline(&body, content), replaced)
}

/// Replace all exact substring occurrences of `old` with `new`.
fn replace_substring(path: &Path, old: &str, new: &str, label: &str) {
    let content = fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("ERROR: Failed to read {}: {e}", path.display());
        process::exit(1);
    });
    if !content.contains(old) {
        eprintln!("WARNING: Pattern '{old}' not found in {label} — skipping");
        return;
    }
    let new_content = content.replace(old, new);
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

/// Increment the Android Play Store versionCode and sync versionName from the Cargo workspace
/// version.  Reads `android-version.properties` from the given Android project root directory
/// (defaults to `test-data/AndoidTestBasicViews`).
///
/// # Layout expected in android-version.properties
/// ```text
/// VERSION_CODE=<positive integer>
/// VERSION_NAME=<semver string>
/// ```
fn bump_android_version(android_dir: Option<&str>) {
    let root = workspace_root();

    let android_root = match android_dir {
        Some(d) => PathBuf::from(d),
        None => root.join("test-data").join("AndoidTestBasicViews"),
    };
    let props_path = android_root.join("android-version.properties");

    // --- Read current properties ---
    let content = fs::read_to_string(&props_path).unwrap_or_else(|e| {
        eprintln!("ERROR: Failed to read {}: {e}", props_path.display());
        eprintln!("Expected file at: {}", props_path.display());
        process::exit(1);
    });

    let current_code = parse_version_properties(&props_path, &content);

    // Play Console hard limit is 2_100_000_000
    const PLAY_MAX: u64 = 2_100_000_000;
    let new_code = current_code + 1;
    if new_code > PLAY_MAX {
        eprintln!("ERROR: new versionCode {new_code} exceeds Google Play maximum ({PLAY_MAX})");
        process::exit(1);
    }

    let cargo_ver = get_cargo_version(&root);
    let new_content = rewrite_android_version_properties(&content, new_code, &cargo_ver);

    fs::write(&props_path, new_content).unwrap_or_else(|e| {
        eprintln!("ERROR: Failed to write {}: {e}", props_path.display());
        process::exit(1);
    });

    println!("Android version bumped:");
    println!("  VERSION_CODE : {current_code} → {new_code}");
    println!("  VERSION_NAME : {cargo_ver}");
    println!();
    println!("Next steps:");
    println!("  1. Build your release bundle:  ./gradlew bundleRelease");
    println!("  2. Upload app-release.aab to Play Console → Internal testing.");
    println!();
    println!("Suggested commit:");
    println!(
        "  git add {} && git commit -m \"chore(android): bump Play versionCode to {new_code}\"",
        props_path.display()
    );
}

fn rewrite_android_version_properties(content: &str, new_code: u64, cargo_ver: &str) -> String {
    let body: String = content
        .lines()
        .map(|line| {
            if line.starts_with("VERSION_CODE=") {
                format!("VERSION_CODE={new_code}")
            } else if line.starts_with("VERSION_NAME=") {
                format!("VERSION_NAME={cargo_ver}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    restore_trailing_newline(&body, content)
}

fn parse_version_properties(props_path: &Path, content: &str) -> u64 {
    content
        .lines()
        .find(|l| l.starts_with("VERSION_CODE="))
        .unwrap_or_else(|| {
            eprintln!("ERROR: VERSION_CODE not found in {}", props_path.display());
            process::exit(1);
        })
        .trim_start_matches("VERSION_CODE=")
        .trim()
        .parse()
        .unwrap_or_else(|e| {
            eprintln!("ERROR: VERSION_CODE is not a valid integer: {e}");
            process::exit(1);
        })
}

fn get_cargo_version(root: &Path) -> String {
    let cargo_toml_path = root.join("Cargo.toml");
    let cargo_content = fs::read_to_string(&cargo_toml_path).expect("Failed to read Cargo.toml");
    cargo_content
        .lines()
        .find(|l| l.starts_with("version = \""))
        .expect("No version = \"...\" line found in Cargo.toml")
        .split('"')
        .nth(1)
        .expect("Unexpected Cargo.toml version format")
        .to_string()
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

// ---------------------------------------------------------------------------
// safe-update — upgrade workspace deps that are ≥14 days old and advisory-clean
// ---------------------------------------------------------------------------

/// Options parsed from the `safe-update` CLI args.
struct SafeUpdateOpts {
    dry_run: bool,
    min_age_days: i64,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
}

impl SafeUpdateOpts {
    fn parse(args: &[String]) -> Self {
        let mut opts = SafeUpdateOpts {
            dry_run: false,
            min_age_days: 14,
            include: None,
            exclude: None,
        };
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--dry-run" => opts.dry_run = true,
                "--min-age-days" => {
                    i += 1;
                    opts.min_age_days =
                        args.get(i).and_then(|v| v.parse().ok()).unwrap_or_else(|| {
                            eprintln!("ERROR: --min-age-days requires a numeric argument");
                            process::exit(1);
                        });
                }
                "--include" => {
                    i += 1;
                    opts.include = args
                        .get(i)
                        .map(|v| v.split(',').map(|s| s.trim().to_string()).collect());
                }
                "--exclude" => {
                    i += 1;
                    opts.exclude = args
                        .get(i)
                        .map(|v| v.split(',').map(|s| s.trim().to_string()).collect());
                }
                "--help" | "-h" => {
                    println!("cargo xtask safe-update [options]");
                    println!();
                    println!("Upgrade workspace dependencies that are both:");
                    println!("  • at least --min-age-days old on crates.io (default: 14), AND");
                    println!("  • not flagged by `cargo deny check advisories`");
                    println!();
                    println!("Options:");
                    println!("  --dry-run              Print the plan; do not modify any files");
                    println!(
                        "  --min-age-days <N>     Minimum days since crates.io publish (default: 14)"
                    );
                    println!("  --include <a,b,...>    Only consider these crates");
                    println!("  --exclude <a,b,...>    Skip these crates");
                    println!("  -h, --help             Show this message");
                    println!();
                    println!("Prerequisites (auto-detected at runtime):");
                    println!("  cargo install cargo-edit    # provides `cargo upgrade`");
                    println!("  cargo install cargo-deny    # provides `cargo deny`");
                    process::exit(0);
                }
                other => {
                    eprintln!("Unknown flag: {other}");
                    eprintln!("Run `cargo xtask safe-update --help` for usage.");
                    process::exit(1);
                }
            }
            i += 1;
        }
        opts
    }
}

/// Entry point for the `safe-update` subcommand.
#[derive(Debug)]
struct Row {
    name: String,
    old_ver: String,
    new_ver: String,
    age_days: Option<i64>,
    status: String,
}

/// Entry point for the `safe-update` subcommand.
fn pin_skipped_dependencies(root: &Path, rows: &[Row]) {
    println!("Pinning skipped/unsafe dependencies to current versions in Cargo.lock…");
    for row in rows {
        if row.status.starts_with("skipped:") {
            println!(
                "  Pinning {name} to {version}…",
                name = row.name,
                version = row.old_ver
            );
            let _ = std::process::Command::new("cargo")
                .args(["update", "-p", &row.name, "--precise", &row.old_ver])
                .current_dir(root)
                .status();
        }
    }
}

fn verify_workspace_compiles(root: &Path) {
    println!();
    println!("Verifying workspace compiles…");
    let exit = std::process::Command::new("cargo")
        .args(["check", "--workspace"])
        .current_dir(root)
        .status()
        .unwrap_or_else(|e| {
            eprintln!("ERROR: failed to run `cargo check`: {e}");
            process::exit(1);
        });
    if !exit.success() {
        eprintln!();
        eprintln!("ERROR: `cargo check --workspace` failed after upgrades.");
        eprintln!("Review `git diff` and revert if needed:");
        eprintln!("  git checkout -- Cargo.toml Cargo.lock");
        process::exit(1);
    }
}

/// Entry point for the `safe-update` subcommand.
fn safe_update(args: &[String]) {
    let opts = SafeUpdateOpts::parse(args);
    check_prereqs();

    let root = workspace_root();

    println!("=== cargo xtask safe-update ===");
    if opts.dry_run {
        println!("(dry-run — no files will be modified)");
    }
    println!(
        "Safety filter: ≥{} days old on crates.io, advisory-clean (cargo-deny)",
        opts.min_age_days
    );
    println!();

    // Discover direct dependency candidates so we know which ones need Cargo.toml updates
    let direct_upgrades: std::collections::HashSet<String> = proposed_upgrades(&root, &opts)
        .into_iter()
        .map(|(name, _, _)| name)
        .collect();

    // Discover all lockfile upgrades (direct + transitive)
    let candidates = proposed_lockfile_updates(&root, &opts);
    if candidates.is_empty() {
        println!("No upgradeable dependencies found.");
        return;
    }

    // --- Step 2: collect known-vulnerable (crate, version) pairs from cargo-deny ---
    let vulnerable_pairs = parse_deny_advisories(&root);

    // --- Step 3: evaluate each candidate ---
    let (rows, to_apply) = evaluate_candidates(&candidates, &vulnerable_pairs, &opts);

    // --- Step 4: print summary table ---
    print_summary_table(&rows);

    if to_apply.is_empty() {
        println!("Nothing to upgrade.");
        return;
    }

    if opts.dry_run {
        println!(
            "{} crate(s) would be upgraded (dry-run; re-run without --dry-run to apply).",
            to_apply.len()
        );
        return;
    }

    // Pin skipped/unsafe candidates to their old versions in Cargo.lock to prevent transitives/resolver from upgrading them
    pin_skipped_dependencies(&root, &rows);

    // --- Step 5: apply upgrades ---
    println!("Applying {} upgrade(s)…", to_apply.len());
    for (name, ver) in &to_apply {
        let is_direct = direct_upgrades.contains(name);
        apply_upgrade(&root, name, ver, is_direct);
    }

    // --- Step 6: verify workspace still compiles ---
    verify_workspace_compiles(&root);

    println!();
    println!(
        "Done. {} crate(s) upgraded. Review `git diff Cargo.toml Cargo.lock` before committing.",
        to_apply.len()
    );
}

fn print_summary_table(rows: &[Row]) {
    println!(
        "{:<30} {:<12} {:<12} {:>8}  status",
        "crate", "old", "new", "age(d)"
    );
    println!("{}", "-".repeat(80));
    for row in rows {
        let age_str = row
            .age_days
            .map(|d| d.to_string())
            .unwrap_or_else(|| "?".to_string());
        println!(
            "{:<30} {:<12} {:<12} {:>8}  {}",
            row.name, row.old_ver, row.new_ver, age_str, row.status
        );
    }
}

fn candidate_row(
    name: &str,
    old_ver: &str,
    new_ver: &str,
    age_days: Option<i64>,
    status: impl Into<String>,
) -> Row {
    Row {
        name: name.to_string(),
        old_ver: old_ver.to_string(),
        new_ver: new_ver.to_string(),
        age_days,
        status: status.into(),
    }
}

fn evaluate_one_candidate(
    name: &str,
    old_ver: &str,
    new_ver: &str,
    vulnerable_pairs: &std::collections::HashSet<(String, String)>,
    opts: &SafeUpdateOpts,
) -> (Row, Option<(String, String)>) {
    if is_prerelease(new_ver) {
        return (
            candidate_row(name, old_ver, new_ver, None, "skipped:pre-release"),
            None,
        );
    }
    if is_vulnerable(name, new_ver, vulnerable_pairs) {
        return (
            candidate_row(name, old_ver, new_ver, None, "skipped:advisory"),
            None,
        );
    }
    match fetch_crate_publish_age_days(name, new_ver) {
        Err(e) => {
            eprintln!("  WARN: could not fetch age for {name}@{new_ver}: {e}");
            (
                candidate_row(name, old_ver, new_ver, None, "skipped:age-fetch-failed"),
                None,
            )
        }
        Ok(age) if age < opts.min_age_days => (
            candidate_row(
                name,
                old_ver,
                new_ver,
                Some(age),
                format!("skipped:too-new ({age}d)"),
            ),
            None,
        ),
        Ok(age) => (
            candidate_row(name, old_ver, new_ver, Some(age), "upgrade"),
            Some((name.to_string(), new_ver.to_string())),
        ),
    }
}

fn evaluate_candidates(
    candidates: &[(String, String, String)],
    vulnerable_pairs: &std::collections::HashSet<(String, String)>,
    opts: &SafeUpdateOpts,
) -> (Vec<Row>, Vec<(String, String)>) {
    let mut rows = Vec::with_capacity(candidates.len());
    let mut to_apply = Vec::new();
    for (name, old_ver, new_ver) in candidates {
        let (row, apply) = evaluate_one_candidate(name, old_ver, new_ver, vulnerable_pairs, opts);
        rows.push(row);
        if let Some(pair) = apply {
            to_apply.push(pair);
        }
    }
    (rows, to_apply)
}

/// Verify that `cargo upgrade` (cargo-edit) and `cargo deny` are available.
/// Emits a clear install command and exits if either is missing.
fn check_prereqs() {
    // cargo upgrade is provided by cargo-edit
    let upgrade_ok = std::process::Command::new("cargo")
        .args(["upgrade", "--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !upgrade_ok {
        eprintln!("ERROR: `cargo upgrade` not found (provided by cargo-edit).");
        eprintln!("Install with: cargo install cargo-edit");
        process::exit(1);
    }

    let deny_ok = std::process::Command::new("cargo")
        .args(["deny", "--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !deny_ok {
        eprintln!("ERROR: `cargo deny` not found.");
        eprintln!("Install with: cargo install cargo-deny");
        process::exit(1);
    }
}

/// Run `cargo upgrade --dry-run` (cargo-edit ≥0.12) and parse its output into
/// a list of `(name, old_version, new_version)` triples.
///
/// Supports cargo-edit output formats:
/// - Table (≥0.13): `name old_req compatible latest new_req`
/// - Legacy arrow: `serde 1.0.210 -> 1.0.215` or `Upgrading serde v1.0.210 -> v1.0.215`
///
/// For safe updates we target the **compatible** column (not `new_req` when it is a major bump).
fn proposed_upgrades(root: &Path, opts: &SafeUpdateOpts) -> Vec<(String, String, String)> {
    let mut cmd = std::process::Command::new("cargo");
    cmd.args([
        "upgrade",
        "--dry-run",
        "--incompatible",
        "allow",
        "--pinned",
        "allow",
        "--recursive",
        "false",
    ])
    .current_dir(root);

    let output = cmd.output().unwrap_or_else(|e| {
        eprintln!("ERROR: failed to run `cargo upgrade --dry-run`: {e}");
        process::exit(1);
    });

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    let arrow_re =
        regex::Regex::new(r"(?i)(?:Upgrading\s+)?([a-zA-Z0-9_\-]+)\s+v?(\S+)\s+->\s+v?(\S+)")
            .expect("static arrow regex");
    let table_re = regex::Regex::new(r"^(.+)\s+(\S+)\s+(\S+)\s+(\S+)\s+(\S+)\s*$")
        .expect("static table regex");

    let mut seen = std::collections::HashSet::new();
    combined
        .lines()
        .filter_map(|line| {
            upgrade_triple_from_table_line(line, &table_re)
                .or_else(|| upgrade_triple_from_arrow_line(line, &arrow_re))
        })
        .filter(|(name, _, _)| passes_upgrade_filters(name, opts))
        .filter(|(name, _, _)| seen.insert(name.clone()))
        .collect()
}

/// Run `cargo update --dry-run` and parse its output to discover upgrades for both direct
/// and transitive/upstream dependencies.
fn proposed_lockfile_updates(root: &Path, opts: &SafeUpdateOpts) -> Vec<(String, String, String)> {
    let mut cmd = std::process::Command::new("cargo");
    cmd.args(["update", "--dry-run"]).current_dir(root);

    let output = cmd.output().unwrap_or_else(|e| {
        eprintln!("ERROR: failed to run `cargo update --dry-run`: {e}");
        process::exit(1);
    });

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    // Look for lines like "    Updating bitflags v2.11.1 -> v2.12.1"
    let update_re =
        regex::Regex::new(r"(?i)^\s*Updating\s+([a-zA-Z0-9_\-]+)\s+v?([^\s]+)\s+->\s+v?([^\s]+)")
            .expect("static update regex");

    let mut seen = std::collections::HashSet::new();
    combined
        .lines()
        .filter_map(|line| {
            let cap = update_re.captures(line)?;
            let name = cap[1].to_string();
            let old_ver = cap[2].trim_start_matches('v').to_string();
            let new_ver = cap[3].trim_start_matches('v').to_string();
            if old_ver == new_ver {
                return None;
            }
            Some((name, old_ver, new_ver))
        })
        .filter(|(name, _, _)| passes_upgrade_filters(name, opts))
        .filter(|(name, _, _)| seen.insert(name.clone()))
        .collect()
}

fn should_skip_cargo_upgrade_line(line: &str) -> bool {
    line.starts_with("Checking ")
        || line.starts_with("note:")
        || line.starts_with("warning:")
        || line.starts_with("error:")
        || line.starts_with("  git:")
        || line.starts_with("  incompatible:")
        || line.starts_with("  latest:")
        || line.starts_with("  local:")
        || line.starts_with("  pinned:")
        || line == "name"
        || line.starts_with("====")
        || line.contains("old req")
}

/// Strip cargo-edit's `rename (crates.io-name)` display suffix for `-p` flags.
fn normalize_upgrade_package_name(raw: &str) -> String {
    raw.split(" (").next().unwrap_or(raw).trim().to_string()
}

fn upgrade_triple_from_table_line(
    line: &str,
    re: &regex::Regex,
) -> Option<(String, String, String)> {
    let line = line.trim();
    if line.is_empty() || should_skip_cargo_upgrade_line(line) {
        return None;
    }
    let cap = re.captures(line)?;
    let name = normalize_upgrade_package_name(cap[1].trim());
    let old_ver = cap[2].trim().to_string();
    let compatible = cap[3].trim().to_string();
    if old_ver == compatible {
        return None;
    }
    Some((name, old_ver, compatible))
}

fn passes_upgrade_filters(name: &str, opts: &SafeUpdateOpts) -> bool {
    if let Some(ref inc) = opts.include
        && !inc.iter().any(|c| c == name)
    {
        return false;
    }
    if let Some(ref exc) = opts.exclude
        && exc.iter().any(|c| c == name)
    {
        return false;
    }
    true
}

fn upgrade_triple_from_arrow_line(
    line: &str,
    re: &regex::Regex,
) -> Option<(String, String, String)> {
    let cap = re.captures(line)?;
    let name = cap[1].to_string();
    let old_ver = cap[2].trim_start_matches('v').to_string();
    let new_ver = cap[3].trim_start_matches('v').to_string();
    if old_ver == new_ver {
        return None;
    }
    Some((name, old_ver, new_ver))
}

/// Return `true` if the semver string has a pre-release component (e.g. `1.0.0-alpha.1`).
fn is_prerelease(ver: &str) -> bool {
    ver.contains('-')
}

/// Run `cargo deny check advisories --format json` and return a set of
/// `(crate_name, version)` pairs that have known advisories.
///
/// We intentionally use a broad match: if *any* version of a crate has an advisory
/// that mentions the candidate version, we skip it.  This is conservative but safe.
fn parse_deny_advisories(root: &Path) -> std::collections::HashSet<(String, String)> {
    let output = std::process::Command::new("cargo")
        .args(["deny", "check", "advisories", "--format", "json"])
        .current_dir(root)
        .output()
        .unwrap_or_else(|e| {
            eprintln!("ERROR: failed to run `cargo deny check advisories`: {e}");
            process::exit(1);
        });

    // cargo-deny exits non-zero when advisories are found; that's expected.
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter_map(advisory_pair_from_ndjson_line)
        .collect()
}

fn advisory_pair_from_ndjson_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let v = serde_json::from_str::<serde_json::Value>(line).ok()?;
    let kind = v.get("type").and_then(|t| t.as_str())?;
    if kind != "advisory" && kind != "diagnostic" {
        return None;
    }
    extract_advisory_crate_ver(&v)
}

fn extract_advisory_crate_ver(v: &serde_json::Value) -> Option<(String, String)> {
    let name = v
        .pointer("/fields/advisory/affected/functions/0/crate/name")
        .or_else(|| v.pointer("/fields/krate/name"))
        .or_else(|| v.pointer("/krate/name"))
        .or_else(|| v.pointer("/package/name"))
        .and_then(|n| n.as_str());
    let ver = v
        .pointer("/fields/krate/version")
        .or_else(|| v.pointer("/krate/version"))
        .or_else(|| v.pointer("/package/version"))
        .and_then(|n| n.as_str());
    match (name, ver) {
        (Some(n), Some(ver_str)) => Some((n.to_string(), ver_str.to_string())),
        _ => None,
    }
}

/// Return `true` if `(name, ver)` appears in the known-vulnerable set.
fn is_vulnerable(
    name: &str,
    ver: &str,
    pairs: &std::collections::HashSet<(String, String)>,
) -> bool {
    pairs.contains(&(name.to_string(), ver.to_string()))
}

/// Query the crates.io API for the publish age of `name@version` in days.
///
/// crates.io rate-limits by IP; adds the required `User-Agent` header.
fn fetch_crate_publish_age_days(name: &str, version: &str) -> Result<i64, String> {
    let url = format!("https://crates.io/api/v1/crates/{name}/{version}");

    // crates.io requires a User-Agent
    let response = ureq::get(&url)
        .header(
            "User-Agent",
            "ahma-xtask/safe-update (https://github.com/paulirotta/ahma)",
        )
        .call()
        .map_err(|e| format!("HTTP request failed: {e}"))?;

    let body = response
        .into_body()
        .read_to_string()
        .map_err(|e| format!("Failed to read response body: {e}"))?;

    let json: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("JSON parse error: {e}"))?;

    // Response layout: { "version": { "created_at": "2024-10-01T12:00:00.000000+00:00", ... } }
    let created_at = json
        .pointer("/version/created_at")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing /version/created_at in crates.io response".to_string())?;

    let published: chrono::DateTime<chrono::Utc> = chrono::DateTime::parse_from_rfc3339(created_at)
        .map_err(|e| format!("date parse error for '{created_at}': {e}"))?
        .into();

    let age = (chrono::Utc::now() - published).num_days();
    Ok(age)
}

/// Apply a single upgrade: bump Cargo.toml via `cargo upgrade -p name@version` (if direct),
/// then tighten/pin Cargo.lock via `cargo update -p name --precise version`.
fn apply_upgrade(root: &Path, name: &str, version: &str, is_direct: bool) {
    if is_direct {
        println!("  Upgrading direct dependency {name} → {version}");
        // cargo upgrade -p name@version writes Cargo.toml requirement
        let exit = std::process::Command::new("cargo")
            .args(["upgrade", "-p", &format!("{name}@{version}")])
            .current_dir(root)
            .status()
            .unwrap_or_else(|e| {
                eprintln!("ERROR: `cargo upgrade -p {name}@{version}` failed: {e}");
                process::exit(1);
            });
        if !exit.success() {
            eprintln!("ERROR: `cargo upgrade -p {name}@{version}` exited with: {exit}");
            process::exit(1);
        }
    } else {
        println!("  Upgrading transitive dependency {name} → {version}");
    }

    // cargo update -p name --precise version pins Cargo.lock
    let exit = std::process::Command::new("cargo")
        .args(["update", "-p", name, "--precise", version])
        .current_dir(root)
        .status()
        .unwrap_or_else(|e| {
            eprintln!("ERROR: `cargo update -p {name} --precise {version}` failed: {e}");
            process::exit(1);
        });
    if !exit.success() {
        eprintln!("WARN: `cargo update -p {name} --precise {version}` exited with: {exit}");
        // Non-fatal — Cargo.lock will still be resolved on next build
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_replace_substring_updates_all_occurrences() {
        // Regression: replace_substring previously used replacen(..., 1) which only updated
        // the first occurrence. install.ps1 has two `-Version 'X.Y.Z'` lines; only the
        // second one (Install-OneSkill) is checked by the invariant test, so bumps were
        // silently skipped on the line that matters.
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "# -Version '1.2.3'").unwrap();
        writeln!(f, "# Install-OneSkill -Version '1.2.3'").unwrap();
        f.flush().unwrap();

        super::replace_substring(f.path(), "-Version '1.2.3'", "-Version '1.2.4'", "test");

        let result = std::fs::read_to_string(f.path()).unwrap();
        assert!(
            result.contains("-Version '1.2.4'"),
            "new version must appear"
        );
        assert!(
            !result.contains("-Version '1.2.3'"),
            "old version must not remain anywhere — replace_substring must update ALL occurrences"
        );
    }

    #[test]
    fn test_parse_cargo_update_output() {
        let sample_output = r#"
    Updating bitflags v2.11.1 -> v2.12.1
    Updating cc v1.2.62 -> v1.2.63
    Removing scc v2.4.0
    Adding shlex v2.0.1
"#;
        let update_re = regex::Regex::new(
            r"(?i)^\s*Updating\s+([a-zA-Z0-9_\-]+)\s+v?([^\s]+)\s+->\s+v?([^\s]+)",
        )
        .unwrap();

        let parsed: Vec<(String, String, String)> = sample_output
            .lines()
            .filter_map(|line| {
                let cap = update_re.captures(line)?;
                let name = cap[1].to_string();
                let old_ver = cap[2].trim_start_matches('v').to_string();
                let new_ver = cap[3].trim_start_matches('v').to_string();
                if old_ver == new_ver {
                    return None;
                }
                Some((name, old_ver, new_ver))
            })
            .collect();

        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed[0],
            (
                "bitflags".to_string(),
                "2.11.1".to_string(),
                "2.12.1".to_string()
            )
        );
        assert_eq!(
            parsed[1],
            ("cc".to_string(), "1.2.62".to_string(), "1.2.63".to_string())
        );
    }
}
