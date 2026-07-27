//! Agent Skills discovery and `SKILL.md` parsing (SPEC §15, R-SK8).
//!
//! Implements the [Agent Skills open standard](https://agentskills.io/specification):
//! a skill is a directory whose name matches the required `name` frontmatter
//! field of the `SKILL.md` it contains. Discovery scans the standard locations
//! (workspace `.agents/skills/` and `.claude/skills/`, then the same two under
//! the user's home directory); the first skill found under a given name wins,
//! so workspace skills shadow user-global ones.
//!
//! The frontmatter parser is deliberately a minimal YAML *subset* — top-level
//! `key: value` scalars, quoted scalars, and `>`/`|` block scalars — because
//! that is all the standard requires and it keeps the workspace free of an
//! unmaintained full-YAML dependency. Unknown keys and nested maps (e.g.
//! `metadata:`) are tolerated and ignored.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Maximum `name` length permitted by the Agent Skills specification.
pub const MAX_NAME_LEN: usize = 64;
/// Maximum `description` length permitted by the Agent Skills specification.
pub const MAX_DESCRIPTION_LEN: usize = 1024;

/// A successfully parsed and validated skill.
#[derive(Debug, Clone)]
pub struct Skill {
    /// The validated skill name (matches the parent directory name).
    pub name: String,
    /// The `description` frontmatter field.
    pub description: String,
    /// `user-invocable` frontmatter extension (SPEC R-SK2). Defaults to `true`
    /// when absent so third-party standard skills remain `/name`-invocable.
    pub user_invocable: bool,
    /// Path to the `SKILL.md` file this skill was loaded from.
    pub path: PathBuf,
    /// Markdown instruction body (everything after the frontmatter block).
    pub body: String,
}

/// A skill directory that could not be loaded, and why. Surfaced so callers
/// can disclose the problem instead of silently hiding a broken skill.
#[derive(Debug, Clone)]
pub struct SkillError {
    /// The `SKILL.md` path (or candidate directory) that failed.
    pub path: PathBuf,
    /// Human-readable reason.
    pub reason: String,
}

/// The result of scanning the skill roots.
#[derive(Debug, Clone, Default)]
pub struct SkillSet {
    /// Valid skills, sorted by name. First-found root wins per name.
    pub skills: Vec<Skill>,
    /// Skill directories that failed validation or parsing.
    pub invalid: Vec<SkillError>,
}

impl SkillSet {
    /// Find a skill by exact name.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }
}

/// The standard discovery roots for `workspace`, in precedence order:
/// workspace `.agents/skills`, workspace `.claude/skills`, then the same two
/// under the user's home directory (where `ahma setup --skills` installs).
pub fn skill_roots(workspace: &Path) -> Vec<PathBuf> {
    let mut roots = vec![
        workspace.join(".agents").join("skills"),
        workspace.join(".claude").join("skills"),
    ];
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".agents").join("skills"));
        roots.push(home.join(".claude").join("skills"));
    }
    roots
}

/// Discover skills from the standard roots for `workspace`.
pub fn discover_skills(workspace: &Path) -> SkillSet {
    discover_skills_in(&skill_roots(workspace))
}

/// Discover skills from explicit `roots` (earlier roots shadow later ones).
/// Roots that resolve to the same directory (e.g. `.claude/skills` symlinked
/// to `.agents/skills`) are scanned once.
pub fn discover_skills_in(roots: &[PathBuf]) -> SkillSet {
    let mut seen_roots: Vec<PathBuf> = Vec::new();
    let mut by_name: BTreeMap<String, Skill> = BTreeMap::new();
    let mut invalid: Vec<SkillError> = Vec::new();

    for root in roots {
        let canonical = dunce::canonicalize(root).unwrap_or_else(|_| root.clone());
        if seen_roots.contains(&canonical) {
            continue;
        }
        seen_roots.push(canonical);

        let Ok(entries) = std::fs::read_dir(root) else {
            continue; // Missing root is normal, not an error.
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            let skill_md = dir.join("SKILL.md");
            if !skill_md.is_file() {
                continue; // Not a skill directory; ignore quietly.
            }
            match load_skill(&skill_md) {
                Ok(skill) => {
                    // First root found under a name wins (workspace shadows home).
                    by_name.entry(skill.name.clone()).or_insert(skill);
                }
                Err(reason) => invalid.push(SkillError {
                    path: skill_md,
                    reason,
                }),
            }
        }
    }

    SkillSet {
        skills: by_name.into_values().collect(),
        invalid,
    }
}

/// Load and validate a single `SKILL.md`.
pub fn load_skill(skill_md: &Path) -> Result<Skill, String> {
    let content =
        std::fs::read_to_string(skill_md).map_err(|e| format!("cannot read SKILL.md: {e}"))?;
    let (fields, body) = parse_frontmatter(&content)?;

    let name = fields
        .get("name")
        .ok_or("frontmatter is missing the required `name` field")?
        .clone();
    validate_name(&name)?;

    let dir_name = skill_md
        .parent()
        .and_then(|d| d.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name != dir_name {
        return Err(format!(
            "`name: {name}` must match the skill directory name `{dir_name}`"
        ));
    }

    let description = fields
        .get("description")
        .ok_or("frontmatter is missing the required `description` field")?
        .clone();
    if description.is_empty() {
        return Err("`description` must be non-empty".into());
    }
    if description.chars().count() > MAX_DESCRIPTION_LEN {
        return Err(format!(
            "`description` exceeds {MAX_DESCRIPTION_LEN} characters"
        ));
    }

    let user_invocable = match fields.get("user-invocable").map(String::as_str) {
        None => true,
        Some("true") => true,
        Some("false") => false,
        Some(other) => {
            return Err(format!(
                "`user-invocable` must be true or false, got {other}"
            ));
        }
    };

    Ok(Skill {
        name,
        description,
        user_invocable,
        path: skill_md.to_path_buf(),
        body: body.to_string(),
    })
}

/// Validate the `name` field per the Agent Skills specification: 1–64 chars,
/// lowercase alphanumerics and hyphens only, no leading/trailing/consecutive
/// hyphens.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.chars().count() > MAX_NAME_LEN {
        return Err(format!("`name` must be 1-{MAX_NAME_LEN} characters"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err("`name` may only contain lowercase letters, digits, and hyphens".to_string());
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err("`name` must not start or end with a hyphen".to_string());
    }
    if name.contains("--") {
        return Err("`name` must not contain consecutive hyphens".to_string());
    }
    Ok(())
}

/// Split `SKILL.md` content into parsed frontmatter fields and the Markdown
/// body. The frontmatter must be the first block, delimited by `---` lines.
fn parse_frontmatter(content: &str) -> Result<(BTreeMap<String, String>, &str), String> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let rest = content
        .strip_prefix("---")
        .and_then(|r| r.strip_prefix("\r\n").or_else(|| r.strip_prefix('\n')))
        .ok_or("SKILL.md must start with a `---` YAML frontmatter block")?;

    // Find the closing delimiter line.
    let mut close: Option<(usize, usize)> = None; // (frontmatter_len, delimiter_line_len)
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            close = Some((offset, line.len()));
            break;
        }
        offset += line.len();
    }
    let (fm_len, delim_len) = close.ok_or("unterminated YAML frontmatter (missing `---`)")?;
    let frontmatter = &rest[..fm_len];
    let body = &rest[fm_len + delim_len..];

    Ok((parse_yaml_subset(frontmatter), body))
}

/// Parse the YAML subset used by skill frontmatter: top-level scalar fields,
/// with support for quoted values and `>` / `|` block scalars (with optional
/// `-`/`+` chomping indicator). Nested maps are skipped.
fn parse_yaml_subset(frontmatter: &str) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    let mut lines = frontmatter.lines().peekable();

    while let Some(line) = lines.next() {
        // Top-level keys start at column 0; anything indented here is a stray
        // continuation (nested-map children are consumed below).
        if line.starts_with(' ') || line.starts_with('\t') || line.trim().is_empty() {
            continue;
        }
        let Some((key, raw_value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_string();
        let raw_value = raw_value.trim();

        let value = match raw_value {
            "" => {
                // Nested map (e.g. `metadata:`): consume and ignore children.
                while lines.peek().is_some_and(|l| {
                    l.starts_with(' ') || l.starts_with('\t') || l.trim().is_empty()
                }) {
                    lines.next();
                }
                continue;
            }
            block
                if block == ">" || block == "|" || {
                    let mut c = block.chars();
                    matches!(c.next(), Some('>') | Some('|'))
                        && matches!(c.next(), Some('-') | Some('+'))
                        && c.next().is_none()
                } =>
            {
                let fold = block.starts_with('>');
                let mut parts: Vec<String> = Vec::new();
                while lines
                    .peek()
                    .is_some_and(|l| l.starts_with(' ') || l.trim().is_empty())
                {
                    let l = lines.next().unwrap_or_default();
                    parts.push(l.trim().to_string());
                }
                // Trim trailing blank continuation lines.
                while parts.last().is_some_and(|p| p.is_empty()) {
                    parts.pop();
                }
                if fold {
                    parts.retain(|p| !p.is_empty());
                    parts.join(" ")
                } else {
                    parts.join("\n")
                }
            }
            scalar => unquote(scalar).to_string(),
        };
        fields.insert(key, value);
    }

    fields
}

/// Strip one layer of matching single or double quotes.
fn unquote(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 2
        && (b[0] == b'"' && b[b.len() - 1] == b'"' || b[0] == b'\'' && b[b.len() - 1] == b'\'')
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, dir_name: &str, content: &str) -> PathBuf {
        let dir = root.join(dir_name);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        std::fs::write(&path, content).unwrap();
        path
    }

    const MINIMAL: &str =
        "---\nname: demo\ndescription: A demo skill for tests.\n---\nDo the demo thing.\n";

    #[test]
    fn parses_minimal_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_skill(tmp.path(), "demo", MINIMAL);
        let skill = load_skill(&path).unwrap();
        assert_eq!(skill.name, "demo");
        assert_eq!(skill.description, "A demo skill for tests.");
        assert!(skill.user_invocable, "user-invocable defaults to true");
        assert_eq!(skill.body.trim(), "Do the demo thing.");
    }

    #[test]
    fn parses_folded_description_and_extension_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let content = "---\nname: fold-demo\nversion: 1.0\ndescription: >\n  First line\n  second line.\n\n  Third line.\nmetadata:\n  author: someone\n  version: \"2\"\nuser-invocable: false\n---\nBody here.\n";
        let path = write_skill(tmp.path(), "fold-demo", content);
        let skill = load_skill(&path).unwrap();
        assert_eq!(skill.description, "First line second line. Third line.");
        assert!(!skill.user_invocable);
        assert_eq!(skill.body.trim(), "Body here.");
    }

    #[test]
    fn parses_literal_block_and_quoted_scalars() {
        let tmp = tempfile::tempdir().unwrap();
        let content = "---\nname: lit\ndescription: \"Quoted description.\"\nnotes: |\n  line one\n  line two\n---\nBody.\n";
        let path = write_skill(tmp.path(), "lit", content);
        let skill = load_skill(&path).unwrap();
        assert_eq!(skill.description, "Quoted description.");
    }

    #[test]
    fn rejects_invalid_names() {
        for bad in [
            "PDF-Processing",
            "-pdf",
            "pdf-",
            "pdf--processing",
            "",
            "a b",
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?} must be rejected");
        }
        for good in ["pdf-processing", "a", "code-review-2"] {
            assert!(validate_name(good).is_ok(), "{good:?} must be accepted");
        }
        assert!(validate_name(&"a".repeat(64)).is_ok());
        assert!(validate_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn rejects_name_directory_mismatch_and_missing_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_skill(tmp.path(), "other-dir", MINIMAL);
        let err = load_skill(&path).unwrap_err();
        assert!(err.contains("must match the skill directory name"), "{err}");

        let path = write_skill(tmp.path(), "nodesc", "---\nname: nodesc\n---\nBody\n");
        let err = load_skill(&path).unwrap_err();
        assert!(err.contains("description"), "{err}");

        let path = write_skill(tmp.path(), "nofm", "No frontmatter at all.\n");
        let err = load_skill(&path).unwrap_err();
        assert!(err.contains("frontmatter"), "{err}");
    }

    #[test]
    fn discovery_prefers_earlier_roots_and_reports_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        let ws_root = tmp.path().join("ws");
        let home_root = tmp.path().join("home");
        write_skill(
            &ws_root,
            "demo",
            "---\nname: demo\ndescription: Workspace copy.\n---\nWS body\n",
        );
        write_skill(
            &home_root,
            "demo",
            "---\nname: demo\ndescription: Home copy.\n---\nHome body\n",
        );
        write_skill(
            &home_root,
            "extra",
            "---\nname: extra\ndescription: Home-only skill.\n---\nExtra body\n",
        );
        write_skill(
            &home_root,
            "Broken",
            "---\nname: Broken\ndescription: Bad name.\n---\nX\n",
        );

        let set = discover_skills_in(&[ws_root, home_root]);
        assert_eq!(set.skills.len(), 2);
        assert_eq!(set.get("demo").unwrap().description, "Workspace copy.");
        assert_eq!(set.get("extra").unwrap().description, "Home-only skill.");
        assert_eq!(set.invalid.len(), 1);
        assert!(
            set.invalid[0].reason.contains("lowercase"),
            "{:?}",
            set.invalid
        );
    }

    #[test]
    fn discovery_skips_duplicate_symlinked_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        write_skill(&real, "demo", MINIMAL);

        #[cfg(unix)]
        {
            let link = tmp.path().join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            let set = discover_skills_in(&[real.clone(), link]);
            assert_eq!(set.skills.len(), 1);
        }

        // Same root listed twice is also scanned once.
        let set = discover_skills_in(&[real.clone(), real]);
        assert_eq!(set.skills.len(), 1);
    }

    #[test]
    fn missing_roots_and_non_skill_dirs_are_quietly_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("skills");
        std::fs::create_dir_all(root.join("not-a-skill")).unwrap();
        let set = discover_skills_in(&[root, tmp.path().join("does-not-exist")]);
        assert!(set.skills.is_empty());
        assert!(set.invalid.is_empty());
    }

    #[test]
    fn parses_this_repos_ahma_skill_frontmatter_shape() {
        // Mirror of skills/ahma/SKILL.md's frontmatter style (folded block
        // description + version/author extension fields).
        let tmp = tempfile::tempdir().unwrap();
        let content = "---\nname: ahma\nversion: 0.16.6\nauthor: Someone\ndescription: >\n  Comprehensive guide.\n  Trigger phrases: \"use ahma\".\nuser-invocable: true\n---\n# Ahma\nInstructions.\n";
        let path = write_skill(tmp.path(), "ahma", content);
        let skill = load_skill(&path).unwrap();
        assert_eq!(skill.name, "ahma");
        assert!(skill.user_invocable);
        assert!(skill.description.starts_with("Comprehensive guide."));
        assert!(skill.body.contains("# Ahma"));
    }
}
