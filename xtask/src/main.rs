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

    // 5. Cargo.lock — every workspace member carries its own version here, and ALL CI
    //    builds with `--locked`.  If we bump Cargo.toml but leave Cargo.lock stale, the
    //    release-bump commit fails to build on CI (and the local pre-push `cargo check
    //    --locked` hook rejects it).  Refresh the lockfile so the member versions match.
    refresh_cargo_lock(root, new_ver);

    println!();
    println!("Done. Version bumped to {new_ver}.");
    println!("Suggested commit:");
    println!(
        "  git add Cargo.toml Cargo.lock skills/ahma/SKILL.md scripts/install.sh scripts/install.ps1"
    );
    println!("  git commit -m \"chore(release): bump version to {new_ver}\"");
}

/// Refresh `Cargo.lock` so every workspace member's `version` entry matches the just-bumped
/// `Cargo.toml`.  Uses `cargo update --workspace`, which re-resolves ONLY the workspace
/// packages and leaves third-party dependency pins untouched (so the bump never perturbs the
/// dependency graph).  Tries `--offline` first (workspace-member resolution needs no network
/// and keeps the bump deterministic); falls back to an online run only if offline fails.
/// After updating, verifies the lockfile actually moved to `new_ver` and warns loudly if not,
/// because a silently-stale Cargo.lock is exactly the regression this guards against.
fn refresh_cargo_lock(root: &Path, new_ver: &str) {
    let updated = run_cargo_update_workspace(root, true) || run_cargo_update_workspace(root, false);
    if !updated {
        eprintln!(
            "WARNING: `cargo update --workspace` did not succeed — Cargo.lock may be stale.\n\
             Run `cargo update --workspace` manually and include Cargo.lock in the release commit,\n\
             or CI's `--locked` build will fail."
        );
        return;
    }
    let lock_path = root.join("Cargo.lock");
    match fs::read_to_string(&lock_path) {
        Ok(lock) => match package_version_in_lock(&lock, "ahma_bin") {
            Some(v) if v == new_ver => println!("  OK Cargo.lock (workspace members → {new_ver})"),
            Some(v) => eprintln!(
                "WARNING: Cargo.lock still pins ahma_bin = {v} after update (expected {new_ver}); \
                 verify the lockfile before committing."
            ),
            None => eprintln!(
                "WARNING: could not find ahma_bin in Cargo.lock to verify the bump; \
                 review Cargo.lock before committing."
            ),
        },
        Err(e) => eprintln!("WARNING: could not read Cargo.lock to verify the bump: {e}"),
    }
}

/// Run `cargo update --workspace` (optionally `--offline`) in `root`.
/// Returns `true` only on a clean exit.
fn run_cargo_update_workspace(root: &Path, offline: bool) -> bool {
    let mut args: Vec<&str> = vec!["update", "--workspace"];
    if offline {
        args.push("--offline");
    }
    match std::process::Command::new("cargo")
        .args(&args)
        .current_dir(root)
        .status()
    {
        Ok(status) => status.success(),
        Err(e) => {
            eprintln!(
                "  (could not spawn `cargo update --workspace{}`: {e})",
                if offline { " --offline" } else { "" }
            );
            false
        }
    }
}

/// Parse a `Cargo.lock` and return the `version` of the `[[package]]` entry named `package`.
///
/// Cargo.lock is line-oriented TOML; each package is a `[[package]]` block with `name = "..."`
/// and `version = "..."` lines.  We scan for the block whose `name` matches and return the
/// first `version` that follows it.  Pure (no I/O) so it is cheaply unit-testable.
fn package_version_in_lock(lock_contents: &str, package: &str) -> Option<String> {
    let target = format!("name = \"{package}\"");
    let mut in_target = false;
    for line in lock_contents.lines() {
        let trimmed = line.trim();
        if trimmed == "[[package]]" {
            in_target = false;
        } else if trimmed == target {
            in_target = true;
        } else if in_target && let Some(rest) = trimmed.strip_prefix("version = \"") {
            return rest.strip_suffix('"').map(str::to_string);
        }
    }
    None
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
    /// Extra human-readable context for non-obvious statuses (e.g. why a
    /// candidate is `skipped:blocked`). Printed beneath the summary table.
    note: Option<String>,
}

/// A planned upgrade carrying both endpoints so the lockfile pin can be
/// disambiguated as `name@<old> --precise <new>`. Threading `old_ver` through is
/// what prevents the "specification `X` is ambiguous" failures when two major
/// versions of a crate coexist in the tree (bitflags 1.x + 2.x, socket2 0.5 +
/// 0.6, …): the bare `-p name` form cannot pick which one to bump.
struct Upgrade {
    name: String,
    old_ver: String,
    new_ver: String,
}

/// Outcome of attempting to apply a single planned upgrade. `error` is `None` on
/// success; on failure it holds the most informative line of cargo's stderr so
/// the final summary can report *why* — instead of silently counting the attempt
/// as a success (the old behavior, which reported "8 upgraded" when 1 applied).
struct ApplyResult {
    name: String,
    old_ver: String,
    new_ver: String,
    error: Option<String>,
}

/// Run a `cargo <args>` invocation, capturing output. Returns `Ok(())` on
/// success, or `Err(reason)` with the most informative stderr line on failure.
fn run_cargo_capture(root: &Path, args: &[&str]) -> Result<(), String> {
    let output = std::process::Command::new("cargo")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("failed to spawn cargo: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(extract_cargo_error(&String::from_utf8_lossy(
        &output.stderr,
    )))
}

/// Pull the most useful single line out of a captured cargo stderr blob: prefer
/// the first `error:` line, else the last non-empty line, else a placeholder.
fn extract_cargo_error(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("error"))
        .or_else(|| stderr.lines().map(str::trim).rfind(|l| !l.is_empty()))
        .unwrap_or("unknown cargo error")
        .to_string()
}

/// Dry-run a precise lockfile pin to check the upgrade is actually resolvable
/// *before* we promise it in the plan. Catches structurally-blocked upgrades
/// (e.g. `lru 0.18` blocked by `ratatui-core` requiring `^0.16`) so the preview
/// shows `skipped:blocked` instead of the apply step surprising the caller.
fn precise_pin_resolvable(
    root: &Path,
    name: &str,
    old_ver: &str,
    new_ver: &str,
) -> Result<(), String> {
    run_cargo_capture(
        root,
        &[
            "update",
            "--dry-run",
            "-p",
            &format!("{name}@{old_ver}"),
            "--precise",
            new_ver,
        ],
    )
}

/// Pin skipped/unsafe candidates to their *current* version in Cargo.lock so a
/// targeted upgrade can't drag them past the age gate as a resolver side effect.
/// Pins use the `name@version` spec to stay unambiguous when multiple majors of
/// the crate are present in the tree.
fn pin_skipped_dependencies(root: &Path, rows: &[Row]) {
    println!("Pinning skipped/unsafe dependencies to current versions in Cargo.lock…");
    let mut args = vec!["update".to_string(), "--offline".to_string()];
    let mut count = 0;
    for row in rows {
        if row.status.starts_with("skipped:") {
            args.push("-p".to_string());
            args.push(format!("{}@{}", row.name, row.old_ver));
            args.push("--precise".to_string());
            args.push(row.old_ver.clone());
            count += 1;
        }
    }
    if count == 0 {
        return;
    }
    println!("  Batch-pinning {count} skipped dependencies offline…");
    let status = std::process::Command::new("cargo")
        .args(&args)
        .current_dir(root)
        .status();

    if status.is_err() || matches!(status.as_ref().map(|s| s.success()), Ok(false)) {
        // Fallback without --offline if offline fails
        let mut online_args = args;
        online_args.remove(1); // remove "--offline"
        let _ = std::process::Command::new("cargo")
            .args(&online_args)
            .current_dir(root)
            .status();
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

    // --- Step 3: evaluate each candidate (includes a resolvability pre-check) ---
    let (rows, to_apply) = evaluate_candidates(&root, &candidates, &vulnerable_pairs, &opts);

    // --- Step 4: print summary table + notes ---
    print_summary_table(&rows);
    print_blocked_notes(&rows);

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

    // --- Step 5: apply upgrades, tracking real per-crate outcomes ---
    println!("Applying {} upgrade(s)…", to_apply.len());
    let results: Vec<ApplyResult> = to_apply
        .iter()
        .map(|up| apply_upgrade(&root, up, direct_upgrades.contains(&up.name)))
        .collect();

    // --- Step 6: verify workspace still compiles ---
    verify_workspace_compiles(&root);

    // --- Step 7: honest summary; exit non-zero if any planned upgrade failed ---
    println!();
    println!("{}", format_apply_summary(&results));
    println!("Review `git diff Cargo.toml Cargo.lock` before committing.");
    if results.iter().any(|r| r.error.is_some()) {
        process::exit(1);
    }
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

/// Print the reason behind every `skipped:blocked` row beneath the table so a
/// structurally-blocked upgrade (e.g. `lru` held by `ratatui-core`) is visible
/// in the preview rather than discovered only at apply time.
fn print_blocked_notes(rows: &[Row]) {
    let blocked: Vec<&Row> = rows.iter().filter(|r| r.note.is_some()).collect();
    if blocked.is_empty() {
        return;
    }
    println!();
    println!("Blocked upgrades (eligible by age/advisory, but not resolvable):");
    for row in blocked {
        println!(
            "  {name} {old} → {new}: {note}",
            name = row.name,
            old = row.old_ver,
            new = row.new_ver,
            note = row.note.as_deref().unwrap_or("")
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
        note: None,
    }
}

fn evaluate_one_candidate(
    root: &Path,
    name: &str,
    old_ver: &str,
    new_ver: &str,
    vulnerable_pairs: &std::collections::HashSet<(String, String)>,
    opts: &SafeUpdateOpts,
) -> (Row, Option<Upgrade>) {
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
        Ok(age) => {
            // Honesty gate: an age- and advisory-clean candidate is only a real
            // "upgrade" if its precise pin actually resolves. Verify now so the
            // preview never promises a structurally-blocked bump (e.g. lru).
            if let Err(reason) = precise_pin_resolvable(root, name, old_ver, new_ver) {
                let mut row = candidate_row(name, old_ver, new_ver, Some(age), "skipped:blocked");
                row.note = Some(reason);
                return (row, None);
            }
            (
                candidate_row(name, old_ver, new_ver, Some(age), "upgrade"),
                Some(Upgrade {
                    name: name.to_string(),
                    old_ver: old_ver.to_string(),
                    new_ver: new_ver.to_string(),
                }),
            )
        }
    }
}

fn evaluate_candidates(
    root: &Path,
    candidates: &[(String, String, String)],
    vulnerable_pairs: &std::collections::HashSet<(String, String)>,
    opts: &SafeUpdateOpts,
) -> (Vec<Row>, Vec<Upgrade>) {
    let mut rows = Vec::with_capacity(candidates.len());
    let mut to_apply = Vec::new();
    for (name, old_ver, new_ver) in candidates {
        let (row, apply) =
            evaluate_one_candidate(root, name, old_ver, new_ver, vulnerable_pairs, opts);
        rows.push(row);
        if let Some(up) = apply {
            to_apply.push(up);
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

    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    let agent = AGENT.get_or_init(|| {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(10)))
            .build();
        ureq::Agent::new_with_config(config)
    });

    // Add a small delay to respect crates.io rate limits
    std::thread::sleep(std::time::Duration::from_millis(100));

    // crates.io requires a User-Agent
    let response = agent
        .get(&url)
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

/// Apply one planned upgrade, returning an [`ApplyResult`] that records whether
/// it actually succeeded. Failures are captured and reported (never silently
/// swallowed) so the caller can tally real successes and exit non-zero on any
/// genuine failure.
fn apply_upgrade(root: &Path, up: &Upgrade, is_direct: bool) -> ApplyResult {
    let kind = if is_direct { "direct" } else { "transitive" };
    let result = (|| {
        if is_direct {
            // `cargo upgrade -p name@new` already carries a version, so it is
            // unambiguous; it rewrites the Cargo.toml requirement.
            run_cargo_capture(
                root,
                &["upgrade", "-p", &format!("{}@{}", up.name, up.new_ver)],
            )?;
        }
        // Pin Cargo.lock. `name@old --precise new` disambiguates when several
        // versions of `name` coexist — the bare `-p name` form errors as
        // "specification is ambiguous" and used to fail silently.
        run_cargo_capture(
            root,
            &[
                "update",
                "-p",
                &format!("{}@{}", up.name, up.old_ver),
                "--precise",
                &up.new_ver,
            ],
        )
    })();

    match &result {
        Ok(()) => println!(
            "  ✓ {kind} {name} {old} → {new}",
            name = up.name,
            old = up.old_ver,
            new = up.new_ver
        ),
        Err(reason) => println!(
            "  ✗ {kind} {name} {old} → {new}: {reason}",
            name = up.name,
            old = up.old_ver,
            new = up.new_ver
        ),
    }

    ApplyResult {
        name: up.name.clone(),
        old_ver: up.old_ver.clone(),
        new_ver: up.new_ver.clone(),
        error: result.err(),
    }
}

/// Build the honest post-apply summary lines from per-crate results: a count of
/// what truly applied vs. was planned, plus an explicit failure block. Pure and
/// unit-tested so the accounting can't drift from reality the way the old
/// `to_apply.len()` count did.
fn format_apply_summary(results: &[ApplyResult]) -> String {
    let failed: Vec<&ApplyResult> = results.iter().filter(|r| r.error.is_some()).collect();
    let applied = results.len() - failed.len();
    let mut out = format!(
        "Applied {applied} of {planned} planned upgrade(s).",
        planned = results.len()
    );
    if !failed.is_empty() {
        out.push_str(&format!("\nFAILED to apply {} upgrade(s):", failed.len()));
        for r in &failed {
            out.push_str(&format!(
                "\n  {name} {old} → {new}: {reason}",
                name = r.name,
                old = r.old_ver,
                new = r.new_ver,
                reason = r.error.as_deref().unwrap_or("unknown error")
            ));
        }
    }
    out
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

    fn apply_result(name: &str, old: &str, new: &str, error: Option<&str>) -> super::ApplyResult {
        super::ApplyResult {
            name: name.to_string(),
            old_ver: old.to_string(),
            new_ver: new.to_string(),
            error: error.map(str::to_string),
        }
    }

    #[test]
    fn test_format_apply_summary_all_success() {
        // Regression: the old code printed `to_apply.len()` (attempt count) as
        // the success count, reporting "8 upgraded" when only 1 applied. The
        // summary must reflect ACTUAL successes and carry no FAILED block.
        let results = vec![
            apply_result("prost", "0.14.3", "0.14.4", None),
            apply_result("socket2", "0.6.3", "0.6.4", None),
        ];
        let summary = super::format_apply_summary(&results);
        assert_eq!(summary, "Applied 2 of 2 planned upgrade(s).");
        assert!(!summary.contains("FAILED"));
    }

    #[test]
    fn test_format_apply_summary_partial_failure_lists_reasons() {
        let results = vec![
            apply_result("prost", "0.14.3", "0.14.4", None),
            apply_result(
                "lru",
                "0.16.4",
                "0.18.0",
                Some("error: failed to select a version"),
            ),
        ];
        let summary = super::format_apply_summary(&results);
        assert!(
            summary.starts_with("Applied 1 of 2 planned upgrade(s)."),
            "must count only real successes, got: {summary}"
        );
        assert!(summary.contains("FAILED to apply 1 upgrade(s):"));
        assert!(
            summary.contains("lru 0.16.4 → 0.18.0: error: failed to select a version"),
            "failure block must name the crate and the reason, got: {summary}"
        );
        assert!(
            !summary.contains("prost 0.14.3"),
            "succeeded crates must not appear in the FAILED block"
        );
    }

    #[test]
    fn test_extract_cargo_error_prefers_error_line() {
        let stderr = "    Updating crates.io index\n\
                      error: specificationm `bitflags` is ambiguous\n\
                      help: re-run this command with one of the following specifications\n";
        assert_eq!(
            super::extract_cargo_error(stderr),
            "error: specificationm `bitflags` is ambiguous"
        );
    }

    #[test]
    fn test_extract_cargo_error_falls_back_to_last_nonempty_line() {
        let stderr = "some warning\nfinal detail line\n\n";
        assert_eq!(super::extract_cargo_error(stderr), "final detail line");
    }

    #[test]
    fn test_extract_cargo_error_empty_input() {
        assert_eq!(super::extract_cargo_error("   \n\n"), "unknown cargo error");
    }

    // Regression (PR #273 fixed this once and it regressed): a version bump must also
    // refresh Cargo.lock, because every workspace member's version lives there and all CI
    // builds with `--locked`.  These tests guard the lockfile-consistency parser used to
    // verify the bump landed.
    #[test]
    fn test_package_version_in_lock_finds_workspace_member() {
        let lock = r#"
# This file is automatically @generated by Cargo.
version = 4

[[package]]
name = "ahma_common"
version = "0.12.19"

[[package]]
name = "ahma_bin"
version = "0.12.19"
dependencies = [
 "ahma_common",
]

[[package]]
name = "anyhow"
version = "1.0.100"
"#;
        assert_eq!(
            super::package_version_in_lock(lock, "ahma_bin").as_deref(),
            Some("0.12.19")
        );
        assert_eq!(
            super::package_version_in_lock(lock, "ahma_common").as_deref(),
            Some("0.12.19")
        );
        // A third-party crate's version must not be confused for a workspace member's.
        assert_eq!(
            super::package_version_in_lock(lock, "anyhow").as_deref(),
            Some("1.0.100")
        );
        assert_eq!(super::package_version_in_lock(lock, "nonexistent"), None);
    }

    #[test]
    fn test_package_version_in_lock_detects_stale_lock() {
        // Simulate the exact bug: Cargo.toml bumped to 0.12.20 but Cargo.lock left at 0.12.19.
        let stale_lock = r#"
[[package]]
name = "ahma_bin"
version = "0.12.19"
"#;
        let bumped = "0.12.20";
        assert_ne!(
            super::package_version_in_lock(stale_lock, "ahma_bin").as_deref(),
            Some(bumped),
            "a stale Cargo.lock must be detectable as not matching the bumped version"
        );
    }
}
