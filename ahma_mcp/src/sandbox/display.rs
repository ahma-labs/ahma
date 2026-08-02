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
    /// Derived from the user's `[sandbox] container_root` — the directory that
    /// holds their projects — because the client reported no usable roots and no
    /// explicit scope was configured (SPEC R5.2.3). Always subject to
    /// auto-narrowing (R5.2.6).
    ///
    /// This replaced a `Default` variant that meant "fell back to `~/sandbox`", a
    /// directory ahma invented rather than the user choosing. R5.2.3 now forbids
    /// inventing one at all, so there is no longer any such thing as a default
    /// scope — only a container the user named.
    Container,
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
            ScopeSource::Container => "container",
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
    /// ahma's own sandbox is enforcing, but an **active probe** proved ahma is
    /// itself running inside an outer host sandbox (Cursor/Claude Code/…). Both
    /// apply, so the effective policy for tool subprocesses is the *intersection*
    /// of the two — access ahma grants (e.g. the macOS keychain) can still be
    /// blocked by the outer sandbox. Not a false positive: this is only reported
    /// when a write outside every scope was actually blocked.
    AhmaEnforcingNestedInHost(super::host_detect::HostSandbox),
    /// ahma detected a host sandbox and deferred to it; ahma is **not** applying
    /// its own enforcement (used by terminal hooks to avoid the redundant
    /// double-sandbox, and when ahma cannot nest its own sandbox inside the host).
    /// Protection now depends on the host.
    DeferredToHost(super::host_detect::HostSandbox),
    /// Nothing is enforcing (e.g. `--no-sandbox` with no detected host).
    Disabled,
}

impl ActiveSandbox {
    /// Observe which sandbox is currently in effect, from whether ahma is
    /// enforcing plus the host-detection / active-confinement probes. Shared by
    /// the startup disclosure and the `sandbox/configured` notification so every
    /// surface (logs, MCP clients, the TUI) reports the same state.
    pub fn observe(enforced: bool) -> ActiveSandbox {
        if enforced {
            match super::confinement::outer_confinement() {
                Some(host) => ActiveSandbox::AhmaEnforcingNestedInHost(host),
                None => ActiveSandbox::AhmaEnforcing,
            }
        } else {
            match super::host_detect::detect_host_sandbox() {
                Some(host) => ActiveSandbox::DeferredToHost(host),
                None => ActiveSandbox::Disabled,
            }
        }
    }

    /// The host label when a host sandbox is involved (nested or deferred), for a
    /// compact status indicator; `None` when ahma is the sole authority or nothing
    /// is enforcing.
    pub fn host_label(self) -> Option<&'static str> {
        match self {
            ActiveSandbox::AhmaEnforcingNestedInHost(h) | ActiveSandbox::DeferredToHost(h) => {
                Some(h.label())
            }
            ActiveSandbox::AhmaEnforcing | ActiveSandbox::Disabled => None,
        }
    }

    /// Stable machine-readable token for JSON payloads / logs.
    pub fn token(self) -> &'static str {
        match self {
            ActiveSandbox::AhmaEnforcing => "ahma",
            ActiveSandbox::AhmaEnforcingNestedInHost(_) => "ahma_nested_in_host",
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
            ActiveSandbox::AhmaEnforcingNestedInHost(host) => format!(
                "Sandbox: ahma kernel sandbox is ENFORCING, but ahma is running INSIDE {host}'s \
                 sandbox — both apply, so the effective policy is the INTERSECTION of the two. \
                 Access ahma grants (e.g. the macOS keychain) may still be BLOCKED by {host}. \
                 {remediation}",
                host = host.label(),
                remediation = host.remediation()
            ),
            ActiveSandbox::DeferredToHost(host) => format!(
                "Sandbox: ahma is DEFERRING to {host}'s sandbox and is NOT applying its own. \
                 Protection now depends on {host}. If you have disabled {host}'s sandbox, this \
                 command runs UNSANDBOXED. {remediation}",
                host = host.label(),
                remediation = host.remediation()
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

        // The platform limitation ahma cannot fix, and therefore must not hide
        // (SPEC R-PERM.5.1). On macOS, reads are not kernel-scoped — a user who
        // believes otherwise will make worse decisions about what to keep on this
        // machine than one who knows. Same honesty R7.5 demands when deferring to
        // a host sandbox.
        if let Some(note) = super::profiles::macos_read_disclosure() {
            out.push_str("  note  : ");
            out.push_str(note);
            out.push('\n');
        }
        out
    }

    /// The structured form used in the `notifications/sandbox/configured`
    /// payload and any machine-readable surface.
    pub fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "enforced": self.enforced,
            "write": display_paths(self.write_scopes),
            "read": display_paths(self.read_scopes),
            "tmp": self.tmp_access,
            "source": self.source.as_str(),
        });
        // Machine-readable surfaces get the disclosure too — a TUI or IDE
        // rendering this JSON must be able to show what the text form shows.
        if let Some(note) = super::profiles::macos_read_disclosure()
            && let Some(obj) = v.as_object_mut()
        {
            obj.insert(
                "reads_unrestricted".to_string(),
                serde_json::Value::Bool(true),
            );
            obj.insert(
                "platform_note".to_string(),
                serde_json::Value::String(note.to_string()),
            );
        }
        v
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
            source: ScopeSource::Container,
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
            text.contains("container"),
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
        assert!(
            deferred.contains("insecure_none"),
            "deferred disclosure must include actionable remediation: {deferred}"
        );

        // Enforcing-nested (intersection): names the host, warns access may still
        // be blocked, and tells the user how to make ahma authoritative.
        let nested =
            ActiveSandbox::AhmaEnforcingNestedInHost(HostSandbox::ClaudeCode).disclosure_line();
        assert!(nested.contains("Claude Code"), "names the host: {nested}");
        assert!(
            nested.contains("INTERSECTION") && nested.contains("BLOCKED"),
            "must explain the intersection and residual blocking: {nested}"
        );
        assert!(
            nested.contains("MCP server"),
            "must include actionable remediation: {nested}"
        );

        let disabled = ActiveSandbox::Disabled.disclosure_line();
        assert!(disabled.contains("UNSANDBOXED"), "{disabled}");

        assert_eq!(ActiveSandbox::AhmaEnforcing.token(), "ahma");
        assert_eq!(
            ActiveSandbox::AhmaEnforcingNestedInHost(HostSandbox::ClaudeCode).token(),
            "ahma_nested_in_host"
        );
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
        assert_eq!(ScopeSource::Container.as_str(), "container");
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
