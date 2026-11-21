//! Sites whose web links commands ahma runs may open in the human's default
//! browser (SPEC R-LINK): the `[open_url]` table of `settings.toml`, the one
//! writer that grants and revokes it (audited, R-LINK.9), and the guidance
//! every decision about it carries (R-LINK.7).
//!
//! A grant names a site pattern — exact (`github.com`), one-label wildcard
//! (`*.github.io`), or `*` for any public site — and is either for **all
//! repos** (`allow`) or for **one repo** (`repos`). The patterns are checked
//! and matched by the egress `HostPattern` in `ahma_mcp`; this module stores
//! the normalised text and never interprets it, so the settings file reads
//! exactly like `[network] allow`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::approver::ApproverSurface;
use crate::permissions::{AuditAction, AuditEntry, GrantKind, GrantTier};

/// The words every decision about opening links carries (SPEC R-LINK.7): the
/// question at every surface, `ahma permissions grant open-url`'s preview and
/// help, and `docs/open-links.md`. One text, so no surface says less.
pub const GUIDANCE: &str = "Opening a link sends its whole address to that website and opens it \
     in your everyday browser, where you are signed in. An address can carry data from your \
     project, so allow only sites you trust. One site is safer than any site, and one repo is \
     safer than all repos. ahma opens only http and https links, in your default browser, never \
     files or apps. Undo with `ahma permissions revoke open-url`.";

/// Said above [`GUIDANCE`] when a grant names no site: any public site.
pub const ANY_SITE_WARNING: &str = "This lets commands ahma runs open any public website in \
     your browser without asking (never localhost or a private address). Naming one site is \
     safer.";

/// The `[open_url]` table of `settings.toml` (SPEC R-LINK.4).
///
/// A table of its own, as `[log_targets]` is: the top level of the file
/// tolerates a table an older build does not know, so an older build simply
/// does not see these grants (fail-closed), where a new key in an existing
/// `deny_unknown_fields` table would make it refuse the whole file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenUrlSettings {
    /// Sites allowed in every repo.
    pub allow: Vec<String>,
    /// Sites allowed in one repo only.
    pub repos: Vec<OpenUrlRepo>,
}

/// The sites allowed in one repo (SPEC R-LINK.4).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenUrlRepo {
    /// Canonical repo root (see [`crate::permissions::workspace_key`]).
    pub workspace: PathBuf,
    /// Site patterns, as in [`OpenUrlSettings::allow`].
    pub allow: Vec<String>,
}

/// Which repos a grant applies in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenUrlBinding {
    /// Every repo: `[open_url] allow`.
    AllRepos,
    /// One repo, by its canonical root: `[open_url] repos`.
    Repo(PathBuf),
}

impl OpenUrlBinding {
    /// How a preview, a listing and the audit trail name it.
    pub fn describe(&self) -> String {
        match self {
            Self::AllRepos => "all repos".to_string(),
            Self::Repo(ws) => format!("repo {}", ws.display()),
        }
    }
}

impl OpenUrlSettings {
    /// The site patterns that apply in `workspace`: every all-repos entry, then
    /// that repo's own.
    pub fn allowed_in(&self, workspace: &Path) -> Vec<&str> {
        self.allow
            .iter()
            .chain(
                self.repos
                    .iter()
                    .filter(|r| r.workspace == workspace)
                    .flat_map(|r| r.allow.iter()),
            )
            .map(String::as_str)
            .collect()
    }

    /// Add `pattern` where `binding` says; `false` when it was already there.
    fn add(&mut self, pattern: &str, binding: &OpenUrlBinding) -> bool {
        let list = match binding {
            OpenUrlBinding::AllRepos => &mut self.allow,
            OpenUrlBinding::Repo(ws) => {
                let i = match self.repos.iter().position(|r| &r.workspace == ws) {
                    Some(i) => i,
                    None => {
                        self.repos.push(OpenUrlRepo {
                            workspace: ws.clone(),
                            allow: Vec::new(),
                        });
                        self.repos.len() - 1
                    }
                };
                &mut self.repos[i].allow
            }
        };
        if list.iter().any(|p| p.eq_ignore_ascii_case(pattern)) {
            return false;
        }
        list.push(pattern.to_string());
        true
    }

    /// Every grant [`revoke`](Self::revoke) would remove: `pattern` (or every
    /// pattern when `None`) where `binding` says (or everywhere when `None`).
    pub fn matching(
        &self,
        pattern: Option<&str>,
        binding: Option<&OpenUrlBinding>,
    ) -> Vec<(String, OpenUrlBinding)> {
        let hit = |p: &String| pattern.is_none_or(|want| p.eq_ignore_ascii_case(want));
        let mut out = Vec::new();
        if matches!(binding, None | Some(OpenUrlBinding::AllRepos)) {
            out.extend(
                self.allow
                    .iter()
                    .filter(|p| hit(p))
                    .map(|p| (p.clone(), OpenUrlBinding::AllRepos)),
            );
        }
        for repo in &self.repos {
            if applies_to(binding, &repo.workspace) {
                out.extend(
                    repo.allow
                        .iter()
                        .filter(|p| hit(p))
                        .map(|p| (p.clone(), OpenUrlBinding::Repo(repo.workspace.clone()))),
                );
            }
        }
        out
    }

    /// Remove what [`matching`](Self::matching) names; how many were removed.
    pub fn revoke(&mut self, pattern: Option<&str>, binding: Option<&OpenUrlBinding>) -> usize {
        let hit = |p: &String| pattern.is_none_or(|want| p.eq_ignore_ascii_case(want));
        let mut removed = 0;
        if matches!(binding, None | Some(OpenUrlBinding::AllRepos)) {
            let before = self.allow.len();
            self.allow.retain(|p| !hit(p));
            removed += before - self.allow.len();
        }
        for repo in &mut self.repos {
            if applies_to(binding, &repo.workspace) {
                let before = repo.allow.len();
                repo.allow.retain(|p| !hit(p));
                removed += before - repo.allow.len();
            }
        }
        self.repos.retain(|r| !r.allow.is_empty());
        removed
    }
}

/// Whether a revoke naming `binding` (`None`: everywhere) reaches the grants
/// of the repo `workspace`.
fn applies_to(binding: Option<&OpenUrlBinding>, workspace: &Path) -> bool {
    match binding {
        None => true,
        Some(OpenUrlBinding::Repo(ws)) => ws == workspace,
        Some(OpenUrlBinding::AllRepos) => false,
    }
}

/// The audit subject of a grant: the pattern, and the repo when it has one.
pub fn audit_subject(pattern: &str, binding: &OpenUrlBinding) -> String {
    match binding {
        OpenUrlBinding::AllRepos => pattern.to_string(),
        OpenUrlBinding::Repo(ws) => format!("{pattern} in {}", ws.display()),
    }
}

/// Save `pattern` (already checked and normalised by the caller) where
/// `binding` says, and audit it with the `surface` that answered (SPEC
/// R-LINK.9): the one writer, so no caller can grant unrecorded. `false` when
/// it was already granted there (nothing written, nothing audited).
pub fn persist_open_url_allow(
    settings_file: &Path,
    pattern: &str,
    binding: &OpenUrlBinding,
    surface: &ApproverSurface,
) -> Result<bool> {
    let _settings_lock = crate::config::lock_settings_for_write(settings_file)?;
    let mut settings = crate::config::AhmaSettings::load_from_result(settings_file)
        .map_err(|e| anyhow::anyhow!(e))
        .with_context(|| {
            format!(
                "refusing to overwrite unparseable {}",
                settings_file.display()
            )
        })?;
    if !settings.open_url.add(pattern, binding) {
        return Ok(false);
    }
    settings
        .save_to(settings_file)
        .with_context(|| format!("failed to write {}", settings_file.display()))?;
    crate::permissions::append_audit(
        &AuditEntry::new(
            crate::config::fmt_utc_datetime(crate::config::unix_now()),
            AuditAction::Grant,
            GrantKind::OpenUrl,
            audit_subject(pattern, binding),
            GrantTier::Always,
        )
        .with_surface(surface),
    );
    Ok(true)
}

/// Remove what [`OpenUrlSettings::matching`] names from the settings file and
/// audit each removal; what was removed.
pub fn revoke_open_url(
    settings_file: &Path,
    pattern: Option<&str>,
    binding: Option<&OpenUrlBinding>,
    surface: &ApproverSurface,
) -> Result<Vec<(String, OpenUrlBinding)>> {
    let _settings_lock = crate::config::lock_settings_for_write(settings_file)?;
    let mut settings = crate::config::AhmaSettings::load_from_result(settings_file)
        .map_err(|e| anyhow::anyhow!(e))
        .with_context(|| {
            format!(
                "refusing to overwrite unparseable {}",
                settings_file.display()
            )
        })?;
    let removed = settings.open_url.matching(pattern, binding);
    if removed.is_empty() {
        return Ok(removed);
    }
    settings.open_url.revoke(pattern, binding);
    settings
        .save_to(settings_file)
        .with_context(|| format!("failed to write {}", settings_file.display()))?;
    let at = crate::config::fmt_utc_datetime(crate::config::unix_now());
    for (p, b) in &removed {
        crate::permissions::append_audit(
            &AuditEntry::new(
                at.clone(),
                AuditAction::Revoke,
                GrantKind::OpenUrl,
                audit_subject(p, b),
                GrantTier::Always,
            )
            .with_surface(surface),
        );
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(p: &str) -> OpenUrlBinding {
        OpenUrlBinding::Repo(PathBuf::from(p))
    }

    #[test]
    fn a_site_for_all_repos_applies_in_every_repo_and_one_for_a_repo_only_there() {
        let mut s = OpenUrlSettings::default();
        assert!(s.add("github.com", &OpenUrlBinding::AllRepos));
        assert!(s.add("example.com", &repo("/w/a")));
        assert_eq!(
            s.allowed_in(Path::new("/w/a")),
            ["github.com", "example.com"]
        );
        assert_eq!(s.allowed_in(Path::new("/w/b")), ["github.com"]);
    }

    #[test]
    fn granting_the_same_site_twice_in_the_same_place_adds_it_once() {
        let mut s = OpenUrlSettings::default();
        assert!(s.add("github.com", &repo("/w/a")));
        assert!(!s.add("GitHub.com", &repo("/w/a")));
        assert!(s.add("github.com", &OpenUrlBinding::AllRepos));
        assert_eq!(s.repos.len(), 1);
        assert_eq!(s.repos[0].allow, ["github.com"]);
    }

    #[test]
    fn revoke_without_a_site_or_repo_removes_every_grant_and_empty_repos_go() {
        let mut s = OpenUrlSettings::default();
        s.add("github.com", &OpenUrlBinding::AllRepos);
        s.add("example.com", &repo("/w/a"));
        s.add("*", &repo("/w/b"));
        assert_eq!(s.matching(None, None).len(), 3);
        assert_eq!(s.revoke(None, None), 3);
        assert_eq!(s, OpenUrlSettings::default());
    }

    #[test]
    fn revoke_of_one_site_in_one_repo_leaves_the_rest() {
        let mut s = OpenUrlSettings::default();
        s.add("github.com", &OpenUrlBinding::AllRepos);
        s.add("github.com", &repo("/w/a"));
        s.add("example.com", &repo("/w/a"));
        assert_eq!(s.revoke(Some("GITHUB.com"), Some(&repo("/w/a"))), 1);
        assert_eq!(s.allow, ["github.com"]);
        assert_eq!(s.repos[0].allow, ["example.com"]);
        // A site with no repo named goes from everywhere.
        assert_eq!(s.revoke(Some("github.com"), None), 1);
        assert!(s.allow.is_empty());
    }

    #[test]
    fn the_guidance_says_what_a_link_carries_and_what_is_narrower() {
        for must in [
            "whole address",
            "signed in",
            "data from your project",
            "only sites you trust",
            "one repo is safer than all repos",
            "never files or apps",
            "ahma permissions revoke open-url",
        ] {
            assert!(GUIDANCE.contains(must), "{must}: {GUIDANCE}");
        }
    }

    #[test]
    fn a_grant_and_its_revoke_are_written_and_audited() {
        let home = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
        let file = home.path().join(".ahma").join("settings.toml");
        let ws = repo("/w/a");
        assert!(persist_open_url_allow(&file, "github.com", &ws, &ApproverSurface::Cli).unwrap());
        assert!(!persist_open_url_allow(&file, "github.com", &ws, &ApproverSurface::Cli).unwrap());
        let saved = crate::config::AhmaSettings::load_from_result(&file).unwrap();
        assert_eq!(saved.open_url.allowed_in(Path::new("/w/a")), ["github.com"]);

        let removed =
            revoke_open_url(&file, Some("github.com"), None, &ApproverSurface::Cli).unwrap();
        assert_eq!(removed, [("github.com".to_string(), ws)]);
        let saved = crate::config::AhmaSettings::load_from_result(&file).unwrap();
        assert_eq!(saved.open_url, OpenUrlSettings::default());

        let audit = crate::permissions::read_audit_entries(
            &home.path().join(".ahma").join("permissions-audit.jsonl"),
        );
        let ours: Vec<_> = audit
            .iter()
            .filter(|e| e.kind == GrantKind::OpenUrl)
            .map(|e| (e.action, e.subject.as_str(), e.surface.as_deref()))
            .collect();
        assert_eq!(
            ours,
            [
                (AuditAction::Grant, "github.com in /w/a", Some("cli")),
                (AuditAction::Revoke, "github.com in /w/a", Some("cli")),
            ]
        );
    }
}
