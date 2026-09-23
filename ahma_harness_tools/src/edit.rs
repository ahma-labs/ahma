//! Text edits: exact-match replacement, several edits in one call, and
//! Codex-style `apply_patch`, all written atomically.
//!
//! The rules every edit follows:
//!
//! * **An edit names one place.** `old_str` must occur exactly once unless the
//!   caller asks for `replace_all`. Replacing every match silently was how a
//!   one-line fix turned into a file-wide rewrite.
//! * **All or nothing.** Every edit in a call is applied in memory first; if any
//!   fails, nothing is written. The file is then replaced atomically (temp file
//!   in the same directory, then rename), keeping its permissions.
//! * **A miss says what is there.** "Not found" carries the nearest thing that
//!   *is* in the file, so the next attempt can be right.
//! * **Line endings are the file's.** Models write `\n`; a CRLF file is matched
//!   and written back as CRLF.

use anyhow::{Context, Result, anyhow, bail};
use std::path::{Path, PathBuf};

/// One exact-match replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub old_str: String,
    pub new_str: String,
    /// Replace every occurrence instead of requiring exactly one.
    pub replace_all: bool,
}

/// What an edit call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditOutcome {
    /// Occurrences replaced across all edits.
    pub replacements: usize,
    /// The edited region with line numbers, so the caller sees the result
    /// without reading the file again.
    pub snippet: String,
}

/// Lines of context around an edit in [`EditOutcome::snippet`].
const SNIPPET_CONTEXT: usize = 4;

/// Apply `edits` in order to `content`. Returns the new content, the number of
/// replacements, and the byte offset of the first change.
pub fn apply_edits(content: &str, edits: &[Edit]) -> Result<(String, usize, usize)> {
    if edits.is_empty() {
        bail!("no edits given");
    }
    let mut text = content.to_string();
    let mut total = 0;
    let mut first_change = usize::MAX;
    for (i, edit) in edits.iter().enumerate() {
        let which = if edits.len() > 1 {
            format!("edit {} of {}: ", i + 1, edits.len())
        } else {
            String::new()
        };
        let (next, n, at) = apply_one(&text, edit).map_err(|e| anyhow!("{which}{e}"))?;
        text = next;
        total += n;
        first_change = first_change.min(at);
    }
    Ok((text, total, first_change))
}

fn apply_one(content: &str, edit: &Edit) -> Result<(String, usize, usize)> {
    if edit.old_str.is_empty() {
        bail!("old_str is empty — to create or overwrite a whole file use write_file");
    }
    if edit.old_str == edit.new_str {
        bail!("old_str and new_str are identical — there is nothing to change");
    }
    let count = content.matches(edit.old_str.as_str()).count();
    match count {
        0 => bail!("old_str not found. {}", near_miss(content, &edit.old_str)),
        1 => {}
        n if !edit.replace_all => bail!(
            "old_str matches {n} places. Include more of the surrounding lines so it \
             names exactly one, or set replace_all to change all {n}."
        ),
        _ => {}
    }
    let at = content.find(edit.old_str.as_str()).unwrap_or(0);
    let next = if edit.replace_all {
        content.replace(edit.old_str.as_str(), &edit.new_str)
    } else {
        content.replacen(edit.old_str.as_str(), &edit.new_str, 1)
    };
    Ok((next, if edit.replace_all { count } else { 1 }, at))
}

/// The closest thing to `needle` that is in `content`, as advice.
fn near_miss(content: &str, needle: &str) -> String {
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    if squash(content).contains(&squash(needle)) {
        return "It does match if whitespace is ignored: re-read the file and copy the \
                exact indentation and line breaks."
            .to_string();
    }
    let first = needle.lines().map(str::trim).find(|l| !l.is_empty());
    if let Some(first) = first
        && let Some((n, line)) = content.lines().enumerate().find(|(_, l)| l.trim() == first)
    {
        return format!(
            "Its first line is at line {}: `{}` — the lines after it differ. Re-read \
             from there.",
            n + 1,
            line.trim()
        );
    }
    "Nothing similar is in the file; re-read it before editing.".to_string()
}

/// Numbered lines around the change at byte `at` in `content`.
pub fn snippet_around(content: &str, at: usize, changed_lines: usize) -> String {
    let at = at.min(content.len());
    let line = content[..at].matches('\n').count();
    let start = line.saturating_sub(SNIPPET_CONTEXT);
    let end = line + changed_lines.max(1) + SNIPPET_CONTEXT;
    content
        .lines()
        .enumerate()
        .skip(start)
        .take(end - start)
        .map(|(i, l)| format!("{:>6}\t{l}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Normalise to `\n` for matching; remember whether to write CRLF back.
fn split_line_endings(content: &str) -> (String, bool) {
    if content.contains("\r\n") {
        (content.replace("\r\n", "\n"), true)
    } else {
        (content.to_string(), false)
    }
}

fn restore_line_endings(content: String, crlf: bool) -> String {
    if crlf {
        content.replace('\n', "\r\n")
    } else {
        content
    }
}

/// Apply `edits` to the file at `path` (already scope-validated) and write it
/// atomically. Nothing is written unless every edit applies.
pub async fn edit_file(path: &Path, edits: &[Edit]) -> Result<EditOutcome> {
    let raw = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("Failed to read file: {}", path.display()))?;
    let (content, crlf) = split_line_endings(&raw);
    let edits: Vec<Edit> = edits
        .iter()
        .map(|e| Edit {
            old_str: e.old_str.replace("\r\n", "\n"),
            new_str: e.new_str.replace("\r\n", "\n"),
            replace_all: e.replace_all,
        })
        .collect();
    let (updated, replacements, first) =
        apply_edits(&content, &edits).map_err(|e| anyhow!("{}: {e}", path.display()))?;
    let changed_lines = edits
        .first()
        .map(|e| e.new_str.lines().count())
        .unwrap_or(1);
    let snippet = snippet_around(&updated, first, changed_lines);
    atomic_write(path, &restore_line_endings(updated, crlf)).await?;
    Ok(EditOutcome {
        replacements,
        snippet,
    })
}

/// Replace `path`'s contents atomically: write a temp file beside it, copy the
/// original's permissions, then rename over it. A crash leaves the old file or
/// the new one, never half of each.
pub async fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("Path has no parent: {}", path.display()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.ahma-tmp-{}", std::process::id()));
    tokio::fs::write(&tmp, content)
        .await
        .with_context(|| format!("Failed to write {}", tmp.display()))?;
    if let Ok(meta) = tokio::fs::metadata(path).await {
        let _ = tokio::fs::set_permissions(&tmp, meta.permissions()).await;
    }
    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e).with_context(|| format!("Failed to replace {}", path.display()));
    }
    Ok(())
}

// ─── apply_patch ──────────────────────────────────────────────────────────────

/// One file operation in a patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchOp {
    Add {
        path: PathBuf,
        content: String,
    },
    Delete {
        path: PathBuf,
    },
    Update {
        path: PathBuf,
        move_to: Option<PathBuf>,
        hunks: Vec<Hunk>,
    },
}

impl PatchOp {
    /// Every path this op writes or removes (the source, and a move target).
    pub fn paths(&self) -> Vec<&Path> {
        match self {
            PatchOp::Add { path, .. } | PatchOp::Delete { path } => vec![path],
            PatchOp::Update { path, move_to, .. } => {
                let mut v = vec![path.as_path()];
                if let Some(m) = move_to {
                    v.push(m);
                }
                v
            }
        }
    }
}

/// A changed region: optional `@@` anchor, then context/removed/added lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub anchor: Option<String>,
    /// `(' ' | '-' | '+', text)`.
    pub lines: Vec<(char, String)>,
}

/// Parse the `*** Begin Patch` … `*** End Patch` format (the one Codex models
/// are trained on).
pub fn parse_patch(patch: &str) -> Result<Vec<PatchOp>> {
    let mut lines = patch
        .lines()
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .peekable();
    while lines.peek().is_some_and(|l| l.trim().is_empty()) {
        lines.next();
    }
    if lines.next().map(str::trim) != Some("*** Begin Patch") {
        bail!("a patch starts with `*** Begin Patch`");
    }
    let mut ops = Vec::new();
    loop {
        let Some(line) = lines.next() else {
            bail!("the patch ends without `*** End Patch`");
        };
        let line = line.trim_end();
        if line == "*** End Patch" {
            break;
        }
        if let Some(p) = line.strip_prefix("*** Add File: ") {
            let mut content = String::new();
            while let Some(l) = lines.next_if(|l| !l.starts_with("*** ")) {
                let body = l.strip_prefix('+').ok_or_else(|| {
                    anyhow!("in Add File {p}: every line must start with `+`, got `{l}`")
                })?;
                content.push_str(body);
                content.push('\n');
            }
            ops.push(PatchOp::Add {
                path: PathBuf::from(p.trim()),
                content,
            });
        } else if let Some(p) = line.strip_prefix("*** Delete File: ") {
            ops.push(PatchOp::Delete {
                path: PathBuf::from(p.trim()),
            });
        } else if let Some(p) = line.strip_prefix("*** Update File: ") {
            let move_to = lines
                .next_if(|l| l.starts_with("*** Move to: "))
                .map(|l| PathBuf::from(l["*** Move to: ".len()..].trim()));
            let mut hunks = Vec::new();
            let mut current: Option<Hunk> = None;
            while let Some(l) =
                lines.next_if(|l| !l.starts_with("*** ") || l.trim_end() == "*** End of File")
            {
                if l.trim_end() == "*** End of File" {
                    continue;
                }
                if let Some(anchor) = l.strip_prefix("@@") {
                    if let Some(h) = current.take()
                        && !h.lines.is_empty()
                    {
                        hunks.push(h);
                    }
                    let anchor = anchor.trim();
                    current = Some(Hunk {
                        anchor: (!anchor.is_empty()).then(|| anchor.to_string()),
                        lines: Vec::new(),
                    });
                    continue;
                }
                let (kind, text) = match l.chars().next() {
                    Some(c @ (' ' | '-' | '+')) => (c, &l[1..]),
                    // A bare empty line is an empty context line.
                    None => (' ', ""),
                    Some(_) => bail!(
                        "in Update File {p}: a hunk line must start with ' ', '-' or '+', got `{l}`"
                    ),
                };
                current
                    .get_or_insert_with(|| Hunk {
                        anchor: None,
                        lines: Vec::new(),
                    })
                    .lines
                    .push((kind, text.to_string()));
            }
            if let Some(h) = current
                && !h.lines.is_empty()
            {
                hunks.push(h);
            }
            if hunks.is_empty() && move_to.is_none() {
                bail!("Update File {p} has no changes");
            }
            ops.push(PatchOp::Update {
                path: PathBuf::from(p.trim()),
                move_to,
                hunks,
            });
        } else if !line.trim().is_empty() {
            bail!("unexpected line in patch: `{line}`");
        }
    }
    if ops.is_empty() {
        bail!("the patch has no file operations");
    }
    Ok(ops)
}

/// Find `needle` in `hay` at or after `from`: exactly, then ignoring trailing
/// whitespace, then ignoring surrounding whitespace.
fn seek(hay: &[&str], needle: &[&str], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(hay.len()));
    }
    let fits = |eq: &dyn Fn(&str, &str) -> bool| {
        (from..=hay.len().saturating_sub(needle.len()))
            .find(|&i| needle.iter().enumerate().all(|(j, n)| eq(hay[i + j], n)))
    };
    fits(&|a, b| a == b)
        .or_else(|| fits(&|a, b| a.trim_end() == b.trim_end()))
        .or_else(|| fits(&|a, b| a.trim() == b.trim()))
}

/// Apply `hunks` to `content`, in order.
pub fn apply_hunks(content: &str, hunks: &[Hunk], path: &Path) -> Result<String> {
    let (normalized, crlf) = split_line_endings(content);
    let lines: Vec<&str> = normalized.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut cursor = 0;
    for (n, hunk) in hunks.iter().enumerate() {
        let mut from = cursor;
        if let Some(anchor) = &hunk.anchor {
            match seek(&lines, &[anchor.as_str()], from) {
                Some(i) => from = i,
                None => bail!(
                    "{}: hunk {} anchor `@@ {anchor}` not found",
                    path.display(),
                    n + 1
                ),
            }
        }
        let old: Vec<&str> = hunk
            .lines
            .iter()
            .filter(|(k, _)| *k != '+')
            .map(|(_, t)| t.as_str())
            .collect();
        let Some(at) = seek(&lines, &old, from) else {
            bail!(
                "{}: hunk {} does not match the file — its context/removed lines \
                 were not found{}. Re-read the file and regenerate the patch.",
                path.display(),
                n + 1,
                old.first()
                    .map(|l| format!(" (first: `{}`)", l.trim()))
                    .unwrap_or_default()
            );
        };
        out.extend(lines[cursor..at].iter().map(|s| s.to_string()));
        out.extend(
            hunk.lines
                .iter()
                .filter(|(k, _)| *k != '-')
                .map(|(_, t)| t.clone()),
        );
        cursor = at + old.len();
    }
    out.extend(lines[cursor..].iter().map(|s| s.to_string()));
    let mut text = out.join("\n");
    if normalized.ends_with('\n') || normalized.is_empty() {
        text.push('\n');
    }
    Ok(restore_line_endings(text, crlf))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old: &str, new: &str) -> Edit {
        Edit {
            old_str: old.into(),
            new_str: new.into(),
            replace_all: false,
        }
    }

    #[test]
    fn an_edit_must_name_one_place() {
        let err = apply_edits("a x a", &[edit("a", "b")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("matches 2 places"), "{err}");
        let (out, n, _) = apply_edits(
            "a x a",
            &[Edit {
                replace_all: true,
                ..edit("a", "b")
            }],
        )
        .unwrap();
        assert_eq!((out.as_str(), n), ("b x b", 2));
    }

    #[test]
    fn edits_are_all_or_nothing() {
        let err = apply_edits("one two", &[edit("one", "1"), edit("three", "3")])
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("edit 2 of 2:"), "{err}");
    }

    #[test]
    fn a_miss_points_at_what_is_there() {
        let file = "fn main() {\n    run();\n}\n";
        let err = apply_edits(file, &[edit("fn main() {\n  run();", "x")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("whitespace is ignored"), "{err}");
        let err = apply_edits(file, &[edit("fn main() {\n    walk();", "x")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("first line is at line 1"), "{err}");
    }

    #[test]
    fn identical_and_empty_edits_are_refused() {
        assert!(apply_edits("a", &[edit("a", "a")]).is_err());
        assert!(apply_edits("a", &[edit("", "b")]).is_err());
    }

    #[tokio::test]
    async fn crlf_files_stay_crlf_and_writes_are_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.txt");
        std::fs::write(&path, "one\r\ntwo\r\nthree\r\n").unwrap();
        let out = edit_file(&path, &[edit("one\ntwo", "1\n2")]).await.unwrap();
        assert_eq!(out.replacements, 1);
        assert!(out.snippet.contains("     1\t1"), "{}", out.snippet);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "1\r\n2\r\nthree\r\n"
        );
        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no temp file left behind");
    }

    #[test]
    fn a_patch_parses_every_operation() {
        let patch = "*** Begin Patch\n\
                     *** Add File: new.txt\n+hello\n\
                     *** Delete File: old.txt\n\
                     *** Update File: src/a.rs\n*** Move to: src/b.rs\n@@ fn main\n-    old();\n+    new();\n\
                     *** End Patch\n";
        let ops = parse_patch(patch).unwrap();
        assert_eq!(ops.len(), 3);
        assert_eq!(
            ops[0],
            PatchOp::Add {
                path: "new.txt".into(),
                content: "hello\n".into()
            }
        );
        let PatchOp::Update { move_to, hunks, .. } = &ops[2] else {
            panic!("update")
        };
        assert_eq!(move_to.as_deref(), Some(Path::new("src/b.rs")));
        assert_eq!(hunks[0].anchor.as_deref(), Some("fn main"));
    }

    #[test]
    fn hunks_apply_in_order_with_whitespace_tolerance() {
        let file = "fn a() {\n    one();\n}\n\nfn b() {\n    one();\n}\n";
        let hunks = vec![Hunk {
            anchor: Some("fn b() {".into()),
            lines: vec![('-', "    one();  ".into()), ('+', "    two();".into())],
        }];
        let out = apply_hunks(file, &hunks, Path::new("f")).unwrap();
        assert_eq!(out, "fn a() {\n    one();\n}\n\nfn b() {\n    two();\n}\n");
    }

    #[test]
    fn a_hunk_that_does_not_match_is_an_error_not_a_guess() {
        let hunks = vec![Hunk {
            anchor: None,
            lines: vec![('-', "missing".into()), ('+', "x".into())],
        }];
        let err = apply_hunks("a\nb\n", &hunks, Path::new("f"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not match"), "{err}");
    }

    #[test]
    fn a_malformed_patch_is_refused() {
        assert!(parse_patch("no header").is_err());
        assert!(parse_patch("*** Begin Patch\n*** Add File: a\nnot-plus\n*** End Patch").is_err());
        assert!(parse_patch("*** Begin Patch\n*** Update File: a\n").is_err());
    }
}
