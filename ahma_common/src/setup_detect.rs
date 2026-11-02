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

/// A detector's finding: the edit, and the evidence in this project that
/// suggests it. The question a person is asked, and what they gain and risk,
/// are written from the edit itself in one place (`ahma_mcp::setup_plan::explain`),
/// so a detector, a model and a typed request are worded alike (SPEC R-SETUP.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    /// Which detector (`gpu`, `source-readers`, `profile`).
    pub detector: &'static str,
    /// Which settings file the edit belongs in.
    pub file: SettingsFile,
    /// The edit.
    pub edit: Edit,
    /// Why this project suggests it: what was found, and what goes wrong
    /// without the edit, in plain words.
    pub why: String,
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

/// Run every detector on `workspace` against the current `settings`, judging a
/// script's lines by its name and nothing else. Prefer [`detect_with`] and the
/// edit guard's own classifier.
pub fn detect(workspace: &Path, settings: &AhmaSettings) -> Vec<Detected> {
    detect_with(workspace, settings, &|_| false)
}

/// Run every detector. `rewrites_sources` says whether one command line
/// rewrites source files; it is the edit guard's classifier, passed in because
/// it lives above this crate, so a script setup offers as a reader is judged by
/// the same rule that holds edits back while it runs (SPEC R-SETUP.3.4).
pub fn detect_with(
    workspace: &Path,
    settings: &AhmaSettings,
    rewrites_sources: &dyn Fn(&str) -> bool,
) -> Vec<Detected> {
    let files = project_files(workspace);
    let mut out = Vec::new();
    out.extend(detect_gpu(workspace, &files, settings));
    out.extend(detect_source_readers(workspace, settings, rewrites_sources));
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
        why: format!(
            "This project has {reason}. Without the GPU, a local AI model fails to start \
             (`failed to create command queue`) or runs slowly on the CPU."
        ),
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

/// Words in a script's file name that say it writes: a script so named is
/// never offered as one that "only reads sources". Matched as whole words of
/// the name (`gen-proto.sh`, `format_all`), so `check-generated.sh` is not one
/// unless it says `generate`.
const WRITER_WORDS: &[&str] = &[
    "gen",
    "generate",
    "regen",
    "regenerate",
    "codegen",
    "fmt",
    "format",
    "fix",
    "autofix",
    "migrate",
    "install",
    "update",
    "upgrade",
    "bump",
    "release",
    "publish",
    "deploy",
    "setup",
    "bootstrap",
    "clean",
    "sync",
    "vendor",
];

/// Whether the file name of `script` says it writes (see [`WRITER_WORDS`]).
fn name_says_it_writes(script: &str) -> bool {
    let name = Path::new(script)
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    name.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| WRITER_WORDS.contains(&word))
}

/// How much of a script is read to judge it.
const MAX_SCRIPT_BYTES: u64 = 64 * 1024;

/// Whether a line of `path` is a command `rewrites_sources` calls a rewrite.
/// Comments and blank lines are skipped; an unreadable script is judged by its
/// name alone.
fn body_rewrites(path: &Path, rewrites_sources: &dyn Fn(&str) -> bool) -> bool {
    use std::io::Read;
    let mut text = String::new();
    let read =
        std::fs::File::open(path).and_then(|f| f.take(MAX_SCRIPT_BYTES).read_to_string(&mut text));
    if read.is_err() {
        return false;
    }
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .any(rewrites_sources)
}

/// The most lines of a script shown to a model as evidence.
pub const MAX_EVIDENCE_LINES: usize = 200;

/// The lines of `script`, a path inside `workspace`, numbered so a reader can
/// cite them, bounded to [`MAX_EVIDENCE_LINES`] lines and 64 KiB.
/// `None` for a path that leaves the project (through `..` or a symlink) or
/// that is not a readable file. Reads only.
pub fn script_evidence(workspace: &Path, script: &str) -> Option<String> {
    use std::io::Read;
    let root = dunce::canonicalize(workspace).ok()?;
    let real = dunce::canonicalize(root.join(script)).ok()?;
    if !real.starts_with(&root) || !real.is_file() {
        return None;
    }
    let mut text = String::new();
    std::fs::File::open(&real)
        .and_then(|f| f.take(MAX_SCRIPT_BYTES).read_to_string(&mut text))
        .ok()?;
    let total = text.lines().count();
    let mut out: String = text
        .lines()
        .take(MAX_EVIDENCE_LINES)
        .enumerate()
        .map(|(i, line)| format!("{:>4}| {line}\n", i + 1))
        .collect();
    if total > MAX_EVIDENCE_LINES {
        out.push_str(&format!(
            "({} more lines not shown)\n",
            total - MAX_EVIDENCE_LINES
        ));
    }
    Some(out)
}

/// Project scripts named in a "definition of done" section of AGENTS.md or
/// CLAUDE.md that exist inside the project, are not yet declared readers, and
/// show no sign of writing: neither their name nor any of their lines.
fn detect_source_readers(
    workspace: &Path,
    settings: &AhmaSettings,
    rewrites_sources: &dyn Fn(&str) -> bool,
) -> Vec<Detected> {
    let Ok(root) = dunce::canonicalize(workspace) else {
        return Vec::new();
    };
    // A path inside this project belongs in this project's settings file, so it
    // does not apply to every project with a script at that path. Setup never
    // creates the `.ahma` directory (SPEC R-CFG10.6): without one, it is yours.
    let file = if workspace.join(".ahma").is_dir() {
        SettingsFile::Project
    } else {
        SettingsFile::User
    };
    let mut seen = Vec::<(String, &str)>::new();
    for name in INSTRUCTION_FILES {
        let Ok(text) = std::fs::read_to_string(workspace.join(name)) else {
            continue;
        };
        for command in dod_commands(&text) {
            let Some(script) = script_of(&command, &root) else {
                continue;
            };
            if seen.iter().any(|(s, _)| *s == script)
                || settings.tools.source_readers.contains(&script)
                || name_says_it_writes(&script)
                || body_rewrites(&root.join(&script), rewrites_sources)
            {
                continue;
            }
            seen.push((script, name));
        }
    }
    seen.into_iter()
        .map(|(script, named_in)| Detected {
            detector: "source-readers",
            file,
            edit: Edit::AddToList {
                key: "tools.source_readers",
                item: script.clone(),
            },
            why: format!(
                "Your {named_in} tells AI agents to run `{script}` when they finish work. ahma \
                 cannot tell whether a script changes your code, so while it runs your agent's \
                 edits in that folder wait until it ends."
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
    let real = dunce::canonicalize(root.join(first)).ok()?;
    (real.is_file() && real.starts_with(root) && is_script(&real)).then(|| first.to_string())
}

/// Extensions of files meant to be run by an interpreter.
const SCRIPT_EXTENSIONS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "ps1", "cmd", "bat", "py", "rb", "pl",
];

/// Whether `path` is something a shell runs: it starts with `#!`, or carries a
/// script extension. The same test on every platform, since a Windows checkout
/// has no execute bit to ask. A manifest or a document that a definition of done
/// merely mentions is neither, and is not a "script that only reads sources".
fn is_script(path: &Path) -> bool {
    use std::io::Read;
    if has_extension(path, SCRIPT_EXTENSIONS) {
        return true;
    }
    let mut head = [0u8; 2];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok()
        && head == *b"#!"
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
            why: format!(
                "This project has {evidence}, but ahma's access to that toolchain's shared \
                 folders is switched off, so its builds fail with `Operation not permitted`."
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

    fn readers(dir: &Path) -> Vec<String> {
        detect(dir, &AhmaSettings::default())
            .into_iter()
            .filter(|d| d.detector == "source-readers")
            .map(|d| match d.edit {
                Edit::AddToList { item, .. } => item,
                other => panic!("{other:?}"),
            })
            .collect()
    }

    /// Field case: AGENTS.md names `ahma_http_bridge/Cargo.toml` in prose inside
    /// its definition of done, and setup offered it as a script that "only reads
    /// sources". A manifest is a file the build reads, not a command that runs.
    #[test]
    fn a_data_file_named_in_the_definition_of_done_is_not_a_script() {
        let dir = project();
        write(dir.path(), "crate/Cargo.toml", "[package]\n");
        write(dir.path(), "docs/NOTES.md", "# notes\n");
        write(dir.path(), "scripts/check.sh", "#!/bin/sh\n");
        write(
            dir.path(),
            "AGENTS.md",
            "## Definition of Done\n\nIf you touched `crate/Cargo.toml` or `docs/NOTES.md`, \
             run `scripts/check.sh`.\n",
        );
        assert_eq!(readers(dir.path()), ["scripts/check.sh"]);
    }

    #[test]
    fn a_script_is_a_shebang_file_or_one_with_a_script_extension() {
        let dir = project();
        write(dir.path(), "scripts/check", "#!/usr/bin/env bash\necho\n");
        write(dir.path(), "scripts/lint.SH", "echo\n");
        write(dir.path(), "scripts/plain", "just words\n");
        write(dir.path(), "scripts/empty", "");
        write(
            dir.path(),
            "AGENTS.md",
            "## Definition of Done\n\n```\nscripts/check\nscripts/lint.SH\nscripts/plain\n\
             scripts/empty\n```\n",
        );
        assert_eq!(readers(dir.path()), ["scripts/check", "scripts/lint.SH"]);
    }

    /// Field case: AGENTS.md named `scripts/generate.sh`, and setup asked whether
    /// it "only reads sources". A script whose name says it writes is not offered.
    #[test]
    fn a_script_whose_name_says_it_writes_is_not_offered_as_a_reader() {
        let dir = project();
        for name in [
            "generate.sh",
            "gen-proto.sh",
            "fmt.sh",
            "format_all.sh",
            "fix",
            "install-deps.sh",
            "bump.sh",
            "release.sh",
            "codegen.sh",
            "check.sh",
        ] {
            write(dir.path(), &format!("scripts/{name}"), "#!/bin/sh\n");
        }
        write(
            dir.path(),
            "AGENTS.md",
            "## Definition of Done\n\n```\nscripts/generate.sh\nscripts/gen-proto.sh\n\
             scripts/fmt.sh\nscripts/format_all.sh\nscripts/fix\nscripts/install-deps.sh\n\
             scripts/bump.sh\nscripts/release.sh\nscripts/codegen.sh\nscripts/check.sh\n```\n",
        );
        assert_eq!(readers(dir.path()), ["scripts/check.sh"]);
    }

    /// A script one of whose lines the caller's classifier calls a rewrite is
    /// not offered; the classifier is the edit guard's own (SPEC R-SETUP.3.4).
    #[test]
    fn a_script_with_a_rewriting_line_is_not_offered_as_a_reader() {
        let dir = project();
        write(
            dir.path(),
            "scripts/check.sh",
            "#!/bin/sh\n# cargo fmt is mentioned in a comment only\ncargo fmt --check\ncargo test\n",
        );
        write(
            dir.path(),
            "scripts/verify.sh",
            "#!/bin/sh\nset -e\ncargo fmt\ncargo test\n",
        );
        write(
            dir.path(),
            "AGENTS.md",
            "## Definition of Done\n\n`scripts/check.sh` then `scripts/verify.sh`\n",
        );
        let rewrites = |line: &str| line == "cargo fmt";
        let got: Vec<_> = detect_with(dir.path(), &AhmaSettings::default(), &rewrites)
            .into_iter()
            .filter(|d| d.detector == "source-readers")
            .map(|d| match d.edit {
                Edit::AddToList { item, .. } => item,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(got, ["scripts/check.sh"]);
    }

    /// A project script is a path inside this project, so it goes in this
    /// project's settings file when there is one, not in a file every project
    /// shares; the reason names the file that asked for it.
    #[test]
    fn a_reader_goes_in_the_project_settings_file_when_there_is_one() {
        let dir = project();
        write(dir.path(), "scripts/check.sh", "#!/bin/sh\n");
        write(
            dir.path(),
            "CLAUDE.md",
            "## Definition of Done\n\n`scripts/check.sh`\n",
        );
        let found = |dir: &Path| {
            detect(dir, &AhmaSettings::default())
                .into_iter()
                .find(|d| d.detector == "source-readers")
                .expect("proposed")
        };
        let user = found(dir.path());
        assert_eq!(user.file, SettingsFile::User, "no .ahma directory yet");
        assert!(user.why.contains("CLAUDE.md"), "{}", user.why);
        std::fs::create_dir_all(dir.path().join(".ahma")).unwrap();
        assert_eq!(found(dir.path()).file, SettingsFile::Project);
    }

    /// What a model may read to advise on a reader: the script's own lines,
    /// numbered so it can cite them, bounded, and only from inside the project.
    #[test]
    fn a_scripts_evidence_is_its_numbered_lines_from_inside_the_project_only() {
        let dir = project();
        let body: String = (1..=300).map(|i| format!("echo line {i}\n")).collect();
        write(
            dir.path(),
            "scripts/check.sh",
            &format!("#!/bin/sh\n{body}"),
        );
        let text = script_evidence(dir.path(), "scripts/check.sh").expect("read");
        assert!(text.contains("   2| echo line 1"), "{text}");
        assert!(
            text.lines().count() <= MAX_EVIDENCE_LINES + 1,
            "bounded: {} lines",
            text.lines().count()
        );
        assert!(text.contains("more lines not shown"), "{text}");
        assert!(script_evidence(dir.path(), "../outside.sh").is_none());
        assert!(script_evidence(dir.path(), "scripts/missing.sh").is_none());
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
