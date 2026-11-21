//! Who approved: the surface a permission audit record names (SPEC R-GUI.7,
//! R-PERM.2.1, R-CFG10.5).
//!
//! Every writer of `~/.ahma/permissions-audit.jsonl` — and of the `surface` of
//! a ledger row — takes an [`ApproverSurface`](crate::approver::ApproverSurface) and renders it through
//! `Display`, so the set of names a record can carry is this enum and nothing
//! else. The readers (`AuditEntry`, `GrantRecord`, the ledger rows) keep a
//! plain `Option<String>`: an old record may hold any string, and reading one
//! back must never fail on it.

use std::fmt;

use crate::hub::Surface;

/// Where a human approved, denied or changed something: the `surface` of an
/// audit record (SPEC R-GUI.7).
///
/// Not [`Surface`]. That one is the interface a session runs on (`tui`, `web`,
/// `desktop`) and decides what the session may do (R-GUI.9). This one is who
/// gave the answer, which includes places that are not a session at all: the
/// agent's own prompt, a command line, `ahma doctor`. Every session surface is
/// an approver surface today (`From<Surface>`); the reverse is not true.
///
/// **Adding an approver** — an extension that answers questions, say — is one
/// variant here and its arm in `Display` (and in
/// [`crate::hub::answer_surface`] if its answers arrive over the hub wire).
/// The writers carry the value through without looking inside it, so no call
/// site changes, and `#[non_exhaustive]` keeps every other crate from matching
/// on the whole set. A variant's rendered name is the audit file's format:
/// once written it never changes spelling.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ApproverSurface {
    /// `ahma tui`. Also what an answer that names no surface on the hub wire
    /// came from (SPEC R24.5).
    Tui,
    /// `ahma web`: the page in a browser.
    Web,
    /// `ahma desktop`: the same page in ahma's own window.
    Desktop,
    /// The agent's own prompt: an MCP elicitation, or the dialog a terminal
    /// hook's `ask` raises. The copies such an answer settles elsewhere are
    /// recorded under it too (SPEC R-GUI.7, R-PERM.3.8).
    Harness,
    /// A command typed at a terminal (`ahma sandbox grant`, `ahma settings
    /// set`, `ahma setup --plan --apply`, …).
    Cli,
    /// A fix `ahma doctor` applied with the human's consent: `doctor`, or
    /// `doctor: <detail>` when the fix acted for a workspace or path.
    Doctor {
        /// What the fix acted for (`from <workspace>`, a path), if anything.
        detail: Option<String>,
    },
    /// The subprocess egress proxy's own question (SPEC R-WEB.16).
    EgressProxy,
    /// `fetch_webpage`'s own question, recorded under the tool's bare name
    /// (it predates the `mcp:<tool>` spelling).
    FetchWebpage,
    /// An MCP tool's own question to a human: `mcp:<tool>`.
    McpTool(&'static str),
    /// An answer from the hub wire naming a surface this build does not know:
    /// never trusted into the record (SPEC R-GUI.7).
    Unknown,
}

impl ApproverSurface {
    /// `ahma doctor` with no detail.
    pub const DOCTOR: Self = Self::Doctor { detail: None };
}

impl fmt::Display for ApproverSurface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tui => f.write_str("tui"),
            Self::Web => f.write_str("web"),
            Self::Desktop => f.write_str("desktop"),
            Self::Harness => f.write_str("harness"),
            Self::Cli => f.write_str("cli"),
            Self::Doctor { detail: None } => f.write_str("doctor"),
            Self::Doctor {
                detail: Some(detail),
            } => write!(f, "doctor: {detail}"),
            Self::EgressProxy => f.write_str("egress-proxy"),
            Self::FetchWebpage => f.write_str("fetch_webpage"),
            Self::McpTool(tool) => write!(f, "mcp:{tool}"),
            Self::Unknown => f.write_str("unknown"),
        }
    }
}

impl From<Surface> for ApproverSurface {
    fn from(surface: Surface) -> Self {
        match surface {
            Surface::Tui => Self::Tui,
            Surface::Web => Self::Web,
            Surface::Desktop => Self::Desktop,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The audit file's bytes: each name is what the record held before the
    /// type existed, and a reader (`doctor`, `jq`, the hub) depends on it.
    #[test]
    fn every_approver_renders_the_name_the_audit_file_has_always_held() {
        let cases = [
            (ApproverSurface::Tui, "tui"),
            (ApproverSurface::Web, "web"),
            (ApproverSurface::Desktop, "desktop"),
            (ApproverSurface::Harness, "harness"),
            (ApproverSurface::Cli, "cli"),
            (ApproverSurface::DOCTOR, "doctor"),
            (
                ApproverSurface::Doctor {
                    detail: Some("from /ws".into()),
                },
                "doctor: from /ws",
            ),
            (ApproverSurface::EgressProxy, "egress-proxy"),
            (ApproverSurface::FetchWebpage, "fetch_webpage"),
            (
                ApproverSurface::McpTool("network_grant"),
                "mcp:network_grant",
            ),
            (ApproverSurface::McpTool("logs_approve"), "mcp:logs_approve"),
            (ApproverSurface::Unknown, "unknown"),
        ];
        for (surface, name) in cases {
            assert_eq!(surface.to_string(), name, "{surface:?}");
        }
    }

    /// A session surface approves under its own name, the one
    /// `Surface::as_str` has always given the audit.
    #[test]
    fn a_session_surface_approves_under_its_own_name() {
        for surface in [Surface::Tui, Surface::Web, Surface::Desktop] {
            assert_eq!(ApproverSurface::from(surface).to_string(), surface.as_str());
        }
    }
}
