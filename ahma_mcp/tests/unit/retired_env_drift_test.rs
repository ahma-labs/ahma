//! Guard for SPEC R-CFG1.2.1: a retired `AHMA_*` variable is retired *everywhere*.
//!
//! Fixing the surfaces that had drifted (`ahma-tui`'s token prefs and socket path,
//! `ahma update`'s install dir and musl preference, the `restart` handler's socket,
//! `ahma`'s log target) is only half the job. Every one of those was documented as
//! retired *while the code still read it* — the docs and the code disagreed for
//! months and nothing noticed, because the only thing tying them together was
//! someone remembering.
//!
//! This test is that tie. It reads the RETIRED tables out of
//! `docs/environment-variables.md` and asserts no production source reads any of
//! those names except through [`ahma_mcp::warn_retired_env`], the single function
//! that states the verdict. Adding a new retired variable to the docs
//! automatically extends the guard; re-adding a read to any surface fails here
//! rather than in a user's environment a release later.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    // `ahma_mcp/` → workspace root. Not `env::current_dir()`: nextest's working
    // directory is the package dir, but that is a convention, not a guarantee.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("ahma_mcp must have a parent directory")
        .to_path_buf()
}

/// Variable names listed under a `## RETIRED — …` heading in the env-var docs.
///
/// The docs are the source of truth for *what* is retired (R-CFG1.2 names them
/// there, with replacements). Parsing them rather than hardcoding a list here is
/// the point: a second list would be one more thing to keep in sync, which is the
/// failure this test exists to catch.
fn documented_retired_vars(doc: &str) -> Vec<String> {
    let mut retired = Vec::new();
    let mut in_retired_section = false;
    for line in doc.lines() {
        let trimmed = line.trim();
        if let Some(heading) = trimmed.strip_prefix("## ") {
            in_retired_section = heading.starts_with("RETIRED");
            continue;
        }
        if !in_retired_section || !trimmed.starts_with('|') {
            continue;
        }
        // `| `AHMA_FOO` | replacement |` — take the first cell's backticked name.
        let Some(first_cell) = trimmed.trim_matches('|').split('|').next() else {
            continue;
        };
        let name = first_cell.trim().trim_matches('`').trim();
        if name.starts_with("AHMA_") && name.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
            retired.push(name.to_string());
        }
    }
    retired
}

/// Every `.rs` file under the given crate source directories.
fn production_sources(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for crate_dir in std::fs::read_dir(root).expect("workspace root must be readable") {
        let crate_dir = crate_dir.expect("readable dir entry").path();
        let src = crate_dir.join("src");
        if crate_dir.file_name().is_some_and(|n| {
            n.to_string_lossy().starts_with("ahma_") || n.to_string_lossy() == "ahma_bin"
        }) && src.is_dir()
        {
            collect_rs(&src, &mut files);
        }
    }
    files
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Strip the trailing `#[cfg(test)] mod tests { … }`, by the near-universal
/// convention that it is the last item in the file.
///
/// Tests legitimately *set* retired variables — that is how they assert the
/// warn-and-ignore contract — so scanning them would report the guard's own
/// coverage as a violation.
fn production_part(source: &str) -> &str {
    match source.find("#[cfg(test)]") {
        Some(idx) => &source[..idx],
        None => source,
    }
}

#[test]
fn no_production_surface_reads_a_documented_retired_env_var() {
    let root = repo_root();
    let doc = std::fs::read_to_string(root.join("docs/environment-variables.md"))
        .expect("docs/environment-variables.md must exist");
    let retired = documented_retired_vars(&doc);

    assert!(
        retired.len() > 10,
        "parsed only {} retired variables — the docs format probably changed and this \
         guard silently stopped guarding anything: {retired:?}",
        retired.len()
    );

    let mut violations = Vec::new();
    for file in production_sources(&root) {
        let Ok(source) = std::fs::read_to_string(&file) else {
            continue;
        };
        for (lineno, line) in production_part(&source).lines().enumerate() {
            // Only actual reads. `warn_retired_env("AHMA_FOO")` names the variable
            // but deliberately never yields its value, so it is the allowed form.
            if !(line.contains("env::var(") || line.contains("env::var_os(")) {
                continue;
            }
            for name in &retired {
                if line.contains(&format!("\"{name}\"")) {
                    violations.push(format!(
                        "{}:{}: reads retired {name}\n      {}",
                        file.strip_prefix(&root).unwrap_or(&file).display(),
                        lineno + 1,
                        line.trim()
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "SPEC R-CFG1.2.1: these surfaces read an `AHMA_*` variable that \
         docs/environment-variables.md lists as RETIRED. A variable ignored by one \
         binary and honored by another has two meanings in one product. Route the \
         read through `ahma_mcp::warn_retired_env(name)` — which reports only whether \
         it was set, never its value — and use the CLI flag or settings key instead:\
         \n\n  {}\n",
        violations.join("\n  ")
    );
}

/// The guard is only as good as its parse. If the docs stop yielding names the
/// test above passes vacuously, so pin a few known entries.
#[test]
fn the_retired_var_parse_finds_known_entries() {
    let doc = std::fs::read_to_string(repo_root().join("docs/environment-variables.md"))
        .expect("docs/environment-variables.md must exist");
    let retired = documented_retired_vars(&doc);
    for expected in [
        "AHMA_PREFER_MUSL",
        "AHMA_LOG_TARGET",
        "AHMA_UNIX_SOCKET",
        "AHMA_INSTALL_DIR",
        "AHMA_MINIMIZE_TOKENS",
    ] {
        assert!(
            retired.iter().any(|v| v == expected),
            "{expected} is documented as retired but the parser did not find it: {retired:?}"
        );
    }
}

/// The live variables must **not** be swept up — retirement is a specific verdict,
/// not a blanket ban on reading the environment (R-CFG1.3 keeps an allowlist).
#[test]
fn live_variables_are_not_treated_as_retired() {
    let doc = std::fs::read_to_string(repo_root().join("docs/environment-variables.md"))
        .expect("docs/environment-variables.md must exist");
    let retired = documented_retired_vars(&doc);
    for live in [
        "AHMA_HOOKS",
        "AHMA_DISABLE_HOOKS",
        "AHMA_PREFER_OWN_SANDBOX",
    ] {
        assert!(
            !retired.iter().any(|v| v == live),
            "{live} is a LIVE variable (terminal hooks) and must not be parsed as retired"
        );
    }
}
