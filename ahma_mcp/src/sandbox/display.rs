//! Canonical sandbox-scope rendering (SPEC R5.4).
//!
//! Every surface that shows the sandbox scope — the startup banner, `ahma
//! status`, the TUI scope panel, the `notifications/sandbox/configured`
//! payload, and scope-related error bodies — **must** go through this one
//! representation so the user always sees the complete scope and its
//! provenance. No scope decision may be communicated only via an internal log
//! line.

use std::path::PathBuf;

/// Where the locked sandbox scope came from (SPEC R5.2 precedence). Rendered as
/// the `source:` attribution so the user knows *why* the scope is what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeSource {
    /// `--sandbox-scope` / `--working-directories` / user settings / task vault.
    Explicit,
    /// Workspace roots reported by the MCP client via `roots/list`.
    RootsList,
    /// A user answered a downgrade elicitation.
    Elicited,
    /// Fell back to the declared default `~/sandbox` (no roots, no explicit).
    Default,
    /// Established by the TUI with no live IDE session; applied to the next
    /// attaching session (SPEC R5.3.6).
    Pending,
}

impl ScopeSource {
    /// Stable machine-readable token used in JSON payloads and the `source:`
    /// line. Matches the vocabulary in SPEC R5.4.
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeSource::Explicit => "explicit",
            ScopeSource::RootsList => "roots/list",
            ScopeSource::Elicited => "elicited",
            ScopeSource::Default => "default",
            ScopeSource::Pending => "pending",
        }
    }
}

/// Which sandbox is actually protecting the user right now (SPEC R5.4 "nothing
/// silent"). ahma must always be able to state this so a user never discovers
/// after the fact that they were not protected the way they assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveSandbox {
    /// ahma's own kernel sandbox is enforcing.
    AhmaEnforcing,
    /// ahma detected a host sandbox and deferred to it; ahma is **not** applying
    /// its own enforcement (used by terminal hooks to avoid the redundant
    /// double-sandbox). Protection now depends on the host.
    DeferredToHost(super::host_detect::HostSandbox),
    /// Nothing is enforcing (e.g. `--no-sandbox` with no detected host).
    Disabled,
}

impl ActiveSandbox {
    /// Stable machine-readable token for JSON payloads / logs.
    pub fn token(self) -> &'static str {
        match self {
            ActiveSandbox::AhmaEnforcing => "ahma",
            ActiveSandbox::DeferredToHost(_) => "deferred_to_host",
            ActiveSandbox::Disabled => "disabled",
        }
    }

    /// A single, loud, honest line stating which sandbox is in effect — suitable
    /// for a hook `systemMessage`, the startup banner, or a tool-result note.
    pub fn disclosure_line(self) -> String {
        match self {
            ActiveSandbox::AhmaEnforcing => {
                "Sandbox: ahma kernel sandbox is ENFORCING (writes confined to the workspace scope)."
                    .to_string()
            }
            ActiveSandbox::DeferredToHost(host) => format!(
                "Sandbox: ahma is DEFERRING to {host}'s sandbox and is NOT applying its own. \
                 Protection now depends on {host}. If you have disabled {host}'s sandbox, this \
                 command runs UNSANDBOXED.",
                host = host.label()
            ),
            ActiveSandbox::Disabled => {
                "Sandbox: NO sandbox is enforcing — commands run UNSANDBOXED.".to_string()
            }
        }
    }
}

/// A borrowed, render-ready view of the complete sandbox scope. Cheap to build
/// from a live `Sandbox` or from raw config at startup.
#[derive(Debug, Clone)]
pub struct ScopeView<'a> {
    /// Directories the AI may write to.
    pub write_scopes: &'a [PathBuf],
    /// Read-only directories granted beyond the write roots (e.g. livelog).
    pub read_scopes: &'a [PathBuf],
    /// Whether the system temp directory was added via `--tmp`.
    pub tmp_access: bool,
    /// Whether kernel enforcement is active (`false` == `--no-sandbox`).
    pub enforced: bool,
    /// Provenance of the scope.
    pub source: ScopeSource,
}

impl ScopeView<'_> {
    /// The canonical multi-line human representation (SPEC R5.4). Stable and
    /// greppable: every surface renders identically.
    pub fn render_text(&self) -> String {
        let enforcement = if self.enforced {
            "ENFORCED"
        } else {
            "DISABLED (no kernel sandbox)"
        };
        let mut out = format!("Sandbox: {enforcement}\n");

        out.push_str("  write: ");
        if self.write_scopes.is_empty() {
            // No scope yet — the sandbox is awaiting a source (see R5.2). Make
            // the "nothing is writable yet" state explicit rather than blank.
            out.push_str("(none — awaiting scope)\n");
        } else {
            for (i, path) in self.write_scopes.iter().enumerate() {
                if i > 0 {
                    out.push_str("         ");
                }
                out.push_str(&path.display().to_string());
                out.push('\n');
            }
        }

        out.push_str("  read : ");
        if self.read_scopes.is_empty() {
            out.push_str("(none beyond write roots)\n");
        } else {
            for (i, path) in self.read_scopes.iter().enumerate() {
                if i > 0 {
                    out.push_str("         ");
                }
                out.push_str(&path.display().to_string());
                out.push('\n');
            }
        }

        out.push_str(if self.tmp_access {
            "  tmp  : ON\n"
        } else {
            "  tmp  : OFF\n"
        });
        out.push_str("  source: ");
        out.push_str(self.source.as_str());
        out.push('\n');
        out
    }

    /// The structured form used in the `notifications/sandbox/configured`
    /// payload and any machine-readable surface.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "enforced": self.enforced,
            "write": display_paths(self.write_scopes),
            "read": display_paths(self.read_scopes),
            "tmp": self.tmp_access,
            "source": self.source.as_str(),
        })
    }
}

fn display_paths(paths: &[PathBuf]) -> Vec<String> {
    paths.iter().map(|p| p.display().to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn render_text_shows_all_write_roots() {
        let writes = vec![p("/home/me/proj"), p("/home/me/other")];
        let reads: Vec<PathBuf> = vec![];
        let view = ScopeView {
            write_scopes: &writes,
            read_scopes: &reads,
            tmp_access: false,
            enforced: true,
            source: ScopeSource::RootsList,
        };
        let text = view.render_text();
        assert!(
            text.contains("/home/me/proj"),
            "missing first write root:\n{text}"
        );
        assert!(
            text.contains("/home/me/other"),
            "missing second write root:\n{text}"
        );
    }

    #[test]
    fn render_text_shows_enforcement_tmp_and_source() {
        let writes = vec![p("/ws")];
        let reads: Vec<PathBuf> = vec![];
        let view = ScopeView {
            write_scopes: &writes,
            read_scopes: &reads,
            tmp_access: true,
            enforced: true,
            source: ScopeSource::Default,
        };
        let text = view.render_text();
        assert!(
            text.contains("ENFORCED"),
            "should show enforcement state:\n{text}"
        );
        // tmp is ON in this view
        assert!(
            text.to_lowercase().contains("tmp") && text.to_uppercase().contains("ON"),
            "should show tmp ON:\n{text}"
        );
        assert!(
            text.contains("default"),
            "should show source attribution:\n{text}"
        );
    }

    #[test]
    fn render_text_shows_disabled_enforcement() {
        let writes = vec![p("/ws")];
        let reads: Vec<PathBuf> = vec![];
        let view = ScopeView {
            write_scopes: &writes,
            read_scopes: &reads,
            tmp_access: false,
            enforced: false,
            source: ScopeSource::Explicit,
        };
        let text = view.render_text();
        assert!(
            text.contains("DISABLED") || text.contains("NOT ENFORCED"),
            "should clearly show enforcement is off:\n{text}"
        );
    }

    #[test]
    fn to_json_round_trips_fields() {
        let writes = vec![p("/a"), p("/b")];
        let reads = vec![p("/r")];
        let view = ScopeView {
            write_scopes: &writes,
            read_scopes: &reads,
            tmp_access: true,
            enforced: true,
            source: ScopeSource::Elicited,
        };
        let json = view.to_json();
        assert_eq!(json["enforced"], serde_json::json!(true));
        assert_eq!(json["tmp"], serde_json::json!(true));
        assert_eq!(json["source"], serde_json::json!("elicited"));
        assert_eq!(json["write"], serde_json::json!(["/a", "/b"]));
        assert_eq!(json["read"], serde_json::json!(["/r"]));
    }

    #[test]
    fn active_sandbox_disclosure_is_loud_and_honest() {
        use super::super::host_detect::HostSandbox;

        let enforcing = ActiveSandbox::AhmaEnforcing.disclosure_line();
        assert!(enforcing.contains("ENFORCING"), "{enforcing}");

        let deferred = ActiveSandbox::DeferredToHost(HostSandbox::Cursor).disclosure_line();
        assert!(deferred.contains("Cursor"), "names the host: {deferred}");
        assert!(
            deferred.contains("NOT applying its own") && deferred.contains("UNSANDBOXED"),
            "must warn that ahma is not enforcing and the risk if host sandbox is off: {deferred}"
        );

        let disabled = ActiveSandbox::Disabled.disclosure_line();
        assert!(disabled.contains("UNSANDBOXED"), "{disabled}");

        assert_eq!(ActiveSandbox::AhmaEnforcing.token(), "ahma");
        assert_eq!(
            ActiveSandbox::DeferredToHost(HostSandbox::Docker).token(),
            "deferred_to_host"
        );
        assert_eq!(ActiveSandbox::Disabled.token(), "disabled");
    }

    #[test]
    fn source_tokens_match_spec_vocabulary() {
        assert_eq!(ScopeSource::Explicit.as_str(), "explicit");
        assert_eq!(ScopeSource::RootsList.as_str(), "roots/list");
        assert_eq!(ScopeSource::Elicited.as_str(), "elicited");
        assert_eq!(ScopeSource::Default.as_str(), "default");
        assert_eq!(ScopeSource::Pending.as_str(), "pending");
    }

    #[test]
    fn no_read_scopes_renders_explicit_none() {
        let writes = vec![p("/ws")];
        let reads: Vec<PathBuf> = vec![];
        let view = ScopeView {
            write_scopes: &writes,
            read_scopes: &reads,
            tmp_access: false,
            enforced: true,
            source: ScopeSource::RootsList,
        };
        let text = view.render_text();
        assert!(
            text.to_lowercase().contains("none"),
            "empty read scopes should render an explicit 'none' marker:\n{text}"
        );
    }
}
