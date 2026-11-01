//! Deterministic detectors for `ahma setup` (SPEC R-SETUP.3).
//!
//! Each looks at a project folder and the current settings and says what a
//! human would probably want to turn on *before* an agent trips over it: a
//! local model that needs the GPU, a project script that only reads sources, a
//! toolchain whose profile was switched off. They are read-only and shared by
//! the CLI and the TUI, so the two cannot propose different things (R-DOCTOR.1).
//!
//! A detector proposes; it never writes. The proposal goes through the same
//! validator as any other source and is applied only on the human's `y`.

use std::path::{Path, PathBuf};

use crate::config::{AhmaSettings, SettingsFile};

/// One edit a detector proposes to a string-list or scalar setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// Set `key` to this TOML value, exactly as `ahma settings set` reads it.
    Set { key: &'static str, value: String },
    /// Add one entry to a list of strings (merged at apply time, so two
    /// proposals to the same list never overwrite each other).
    AddToList { key: &'static str, item: String },
}

/// A detector's finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    /// Which detector (`gpu`, `source-readers`, `profile`).
    pub detector: &'static str,
    /// Which settings file the edit belongs in.
    pub file: SettingsFile,
    /// The edit.
    pub edit: Edit,
    /// One line: what is proposed.
    pub title: String,
    /// Why this project suggests it.
    pub why: String,
    /// What it allows, in plain words.
    pub allows: String,
}

/// Directories never searched: dependency and build output, and VCS internals.
const SKIP_DIRS: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    "build",
    "dist",
    "vendor",
    ".venv",
    "venv",
    "__pycache__",
    ".gradle",
    "Pods",
    "DerivedData",
];

/// How deep a project is searched, and how many entries at most. A monorepo
/// must not make `ahma setup` slow, and a model file is rarely deeper.
const MAX_DEPTH: usize = 3;
const MAX_ENTRIES: usize = 20_000;

/// Run every detector on `workspace` against the current `settings`.
pub fn detect(workspace: &Path, settings: &AhmaSettings) -> Vec<Detected> {
    let files = project_files(workspace);
    let mut out = Vec::new();
    out.extend(detect_gpu(workspace, &files, settings));
    out.extend(detect_source_readers(workspace, settings));
    out.extend(detect_profiles(&files, settings));
    out
}

/// Paths under `workspace` (relative), to a bounded depth and count, without
/// descending into dependency or build directories or hidden ones.
fn project_files(workspace: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![(workspace.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if out.len() >= MAX_ENTRIES {
                return out;
            }
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            let path = entry.path();
            if let Ok(rel) = path.strip_prefix(workspace) {
                out.push(rel.to_path_buf());
            }
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir
                && depth < MAX_DEPTH
                && !name_str.starts_with('.')
                && !SKIP_DIRS.contains(&name_str.as_ref())
            {
                stack.push((path, depth + 1));
            }
        }
    }
    out
}

fn has_extension(path: &Path, exts: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| exts.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

fn file_name_is(path: &Path, names: &[&str]) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| names.contains(&n))
}

/// Manifests whose text can name a local-inference library.
const MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "pyproject.toml",
    "requirements.txt",
    "CMakeLists.txt",
    "Package.swift",
    "package.json",
    "Makefile",
];

/// A local model file, or a dependency on one of the usual local-inference
/// libraries, with the GPU switch still off.
fn detect_gpu(workspace: &Path, files: &[PathBuf], settings: &AhmaSettings) -> Option<Detected> {
    if settings.sandbox.allow_gpu {
        return None;
    }
    let model = files
        .iter()
        .find(|p| has_extension(p, &["gguf", "ggml"]))
        .map(|p| format!("a model file ({})", p.display()));
    let library = || {
        files
            .iter()
            .filter(|p| file_name_is(p, MANIFESTS))
            .find_map(|p| {
                let text = std::fs::read_to_string(workspace.join(p)).ok()?;
                let lower = text.to_ascii_lowercase();
                [
                    "llama-cpp",
                    "llama_cpp",
                    "llama.cpp",
                    "ggml",
                    "whisper-rs",
                    "candle-metal",
                ]
                .iter()
                .find(|needle| lower.contains(*needle))
                .map(|needle| format!("{} mentions `{needle}`", p.display()))
            })
    };
    let reason = model.or_else(library)?;
    Some(Detected {
        detector: "gpu",
        file: SettingsFile::User,
        edit: Edit::Set {
            key: "sandbox.allow_gpu",
            value: "true".into(),
        },
        title: "Let sandboxed commands use the GPU (Metal)".into(),
        why: format!(
            "This project has {reason}. Without the GPU, a local model fails with \
             `failed to create command queue` or silently runs on the CPU."
        ),
        allows: "Commands can open the GPU. It is a capability, not a path, so no folder \
                 access changes; the GPU driver stack becomes reachable from sandboxed code."
            .into(),
    })
}

/// The files an agent-instructions file names as its definition of done.
const INSTRUCTION_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

/// Heading words that open a "definition of done" section.
const DOD_HEADINGS: &[&str] = &[
    "definition of done",
    "done criteria",
    "before you claim",
    "before claiming",
    "before committing",
    "before you commit",
    "before you stop",
    "verification",
];

/// Project scripts named in a "definition of done" section of AGENTS.md or
/// CLAUDE.md that exist inside the project and are not yet declared readers.
fn detect_source_readers(workspace: &Path, settings: &AhmaSettings) -> Vec<Detected> {
    let Ok(root) = workspace.canonicalize() else {
        return Vec::new();
    };
    let mut seen = Vec::<String>::new();
    for name in INSTRUCTION_FILES {
        let Ok(text) = std::fs::read_to_string(workspace.join(name)) else {
            continue;
        };
        for command in dod_commands(&text) {
            let Some(script) = script_of(&command, &root) else {
                continue;
            };
            if !seen.contains(&script) && !settings.tools.source_readers.contains(&script) {
                seen.push(script);
            }
        }
    }
    seen.into_iter()
        .map(|script| Detected {
            detector: "source-readers",
            file: SettingsFile::User,
            edit: Edit::AddToList {
                key: "tools.source_readers",
                item: script.clone(),
            },
            title: format!("Let edits continue while `{script}` runs"),
            why: format!(
                "AGENTS.md or CLAUDE.md names `{script}` as part of finishing work. Ahma cannot \
                 tell that a project script only reads sources, so while it runs every edit is \
                 refused."
            ),
            allows: format!(
                "Edits to your sources are no longer held while a command starting with `{script}` \
                 runs. Only declare a script that does not rewrite sources."
            ),
        })
        .collect()
}

/// Commands written inside the "definition of done" sections of `text`: fenced
/// code lines and inline code spans.
fn dod_commands(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_section = false;
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence && trimmed.starts_with('#') {
            let heading = trimmed.trim_start_matches('#').trim().to_ascii_lowercase();
            in_section = DOD_HEADINGS.iter().any(|h| heading.contains(h));
            continue;
        }
        if !in_section {
            continue;
        }
        if in_fence {
            let cmd = trimmed.trim_start_matches('$').trim();
            if !cmd.is_empty() && !cmd.starts_with('#') {
                out.push(cmd.to_string());
            }
        } else {
            for (i, span) in trimmed.split('`').enumerate() {
                if i % 2 == 1 && !span.trim().is_empty() {
                    out.push(span.trim().to_string());
                }
            }
        }
    }
    out
}

/// The script a command starts with: a path-like first word (it contains a
/// `/`, the same rule the edit-guard refusal uses), made only of plain path
/// characters, naming a regular file that stays inside the project.
fn script_of(command: &str, root: &Path) -> Option<String> {
    let first = command.split_whitespace().next()?;
    let first = first.strip_prefix("./").unwrap_or(first);
    let plain = first
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'));
    if !first.contains('/') || !plain || first.starts_with('/') || first.starts_with('-') {
        return None;
    }
    let real = root.join(first).canonicalize().ok()?;
    (real.is_file() && real.starts_with(root)).then(|| first.to_string())
}

/// A toolchain the project uses whose profile is switched off.
fn detect_profiles(files: &[PathBuf], settings: &AhmaSettings) -> Vec<Detected> {
    let uses = |names: &[&str], exts: &[&str]| {
        files
            .iter()
            .any(|p| file_name_is(p, names) || has_extension(p, exts))
    };
    let wanted: [(&str, bool, &str); 5] = [
        ("rust", uses(&["Cargo.toml"], &[]), "Cargo.toml"),
        ("node", uses(&["package.json"], &[]), "package.json"),
        ("go", uses(&["go.mod"], &[]), "go.mod"),
        (
            "android",
            uses(
                &[
                    "build.gradle",
                    "build.gradle.kts",
                    "settings.gradle",
                    "settings.gradle.kts",
                    "AndroidManifest.xml",
                ],
                &[],
            ),
            "a Gradle or Android build",
        ),
        (
            "apple",
            uses(&["Package.swift"], &["xcodeproj", "xcworkspace"]),
            "an Xcode or Swift package",
        ),
    ];
    wanted
        .into_iter()
        .filter(|(profile, used, _)| {
            *used && !settings.sandbox.profiles.iter().any(|p| p == profile)
        })
        .map(|(profile, _, evidence)| Detected {
            detector: "profile",
            file: SettingsFile::User,
            edit: Edit::AddToList {
                key: "sandbox.profiles",
                item: profile.to_string(),
            },
            title: format!("Turn the `{profile}` toolchain profile back on"),
            why: format!(
                "This project has {evidence}, but the `{profile}` profile is off, so its caches \
                 and SDK folders are denied and builds fail with `Operation not permitted`."
            ),
            allows: format!(
                "The `{profile}` profile's folders (see `ahma permissions list`): the toolchain's \
                 caches and SDK, shared by every project on this machine."
            ),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn a_model_file_proposes_the_gpu_and_only_while_it_is_off() {
        let dir = project();
        write(dir.path(), "models/tiny.gguf", "x");
        let off = AhmaSettings::default();
        let found = detect(dir.path(), &off);
        let gpu = found.iter().find(|d| d.detector == "gpu").expect("gpu");
        assert_eq!(
            gpu.edit,
            Edit::Set {
                key: "sandbox.allow_gpu",
                value: "true".into()
            }
        );
        assert!(gpu.why.contains("tiny.gguf"), "{}", gpu.why);
        let mut on = AhmaSettings::default();
        on.sandbox.allow_gpu = true;
        assert!(detect(dir.path(), &on).iter().all(|d| d.detector != "gpu"));
    }

    #[test]
    fn a_local_inference_dependency_proposes_the_gpu_without_a_model_file() {
        let dir = project();
        write(
            dir.path(),
            "Cargo.toml",
            "[dependencies]\nllama-cpp-2 = \"0.1\"\n",
        );
        let found = detect(dir.path(), &AhmaSettings::default());
        let gpu = found.iter().find(|d| d.detector == "gpu").expect("gpu");
        assert!(
            gpu.why.contains("Cargo.toml") && gpu.why.contains("llama-cpp"),
            "{}",
            gpu.why
        );
    }

    #[test]
    fn nothing_is_proposed_for_an_unrelated_project_or_from_dependency_folders() {
        let dir = project();
        write(dir.path(), "src/main.rs", "fn main() {}");
        // A model inside a dependency or build folder does not count.
        write(dir.path(), "node_modules/x/m.gguf", "x");
        write(dir.path(), "target/m.gguf", "x");
        write(dir.path(), ".hidden/m.gguf", "x");
        assert!(detect(dir.path(), &AhmaSettings::default()).is_empty());
    }

    #[test]
    fn definition_of_done_scripts_become_one_proposal_each() {
        let dir = project();
        write(dir.path(), "scripts/check.sh", "#!/bin/sh\n");
        write(dir.path(), "scripts/lint.sh", "#!/bin/sh\n");
        write(
            dir.path(),
            "AGENTS.md",
            "# Project\n\nUse `scripts/other.sh` for something else.\n\n\
             ## Definition of Done\n\n```bash\n./scripts/check.sh --all\ncargo test\n```\n\n\
             Also run `scripts/lint.sh` and `scripts/missing.sh`.\n\n## Style\n\nRun `scripts/style.sh`.\n",
        );
        write(dir.path(), "scripts/other.sh", "x");
        write(dir.path(), "scripts/style.sh", "x");
        let found: Vec<_> = detect(dir.path(), &AhmaSettings::default())
            .into_iter()
            .filter(|d| d.detector == "source-readers")
            .collect();
        let items: Vec<_> = found
            .iter()
            .map(|d| match &d.edit {
                Edit::AddToList { item, .. } => item.as_str(),
                other => panic!("{other:?}"),
            })
            .collect();
        // Only the section's own scripts, only the ones that exist; `cargo test`
        // has no path, `scripts/other.sh` and `style.sh` are outside the section.
        assert_eq!(items, ["scripts/check.sh", "scripts/lint.sh"]);

        let mut declared = AhmaSettings::default();
        declared.tools.source_readers = vec!["scripts/check.sh".into()];
        let rest: Vec<_> = detect(dir.path(), &declared)
            .into_iter()
            .filter(|d| d.detector == "source-readers")
            .collect();
        assert_eq!(
            rest.len(),
            1,
            "an already-declared script is not proposed again"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_script_that_is_a_symlink_out_of_the_project_is_never_proposed() {
        let dir = project();
        let outside = project();
        write(outside.path(), "evil.sh", "x");
        std::fs::create_dir_all(dir.path().join("scripts")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("evil.sh"),
            dir.path().join("scripts/check.sh"),
        )
        .unwrap();
        write(
            dir.path(),
            "AGENTS.md",
            "## Definition of Done\n\n`scripts/check.sh`\n",
        );
        assert!(detect(dir.path(), &AhmaSettings::default()).is_empty());
    }

    #[test]
    fn an_off_profile_is_proposed_only_when_the_project_needs_it() {
        let dir = project();
        write(dir.path(), "app/build.gradle.kts", "");
        write(dir.path(), "Cargo.toml", "");
        let mut s = AhmaSettings::default();
        s.sandbox.profiles = vec!["rust".into()];
        let found = detect(dir.path(), &s);
        let items: Vec<_> = found
            .iter()
            .filter(|d| d.detector == "profile")
            .map(|d| match &d.edit {
                Edit::AddToList { key, item } => (*key, item.as_str()),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(items, [("sandbox.profiles", "android")]);
        // With every default profile on, there is nothing to propose.
        assert!(detect(dir.path(), &AhmaSettings::default()).is_empty());
    }
}
