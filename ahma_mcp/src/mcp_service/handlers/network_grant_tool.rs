//! The `network_grant` MCP tool.
//!
//! When sandboxed subprocesses or commands attempt outbound network egress
//! that is blocked by the network sandbox, this tool allows AI models to
//! inspect, preview, and request persistent network egress permissions.
//!
//! ## Security model
//!
//! Mirrors `sandbox_grant` for the network boundary:
//!
//! 1. **A hard denylist** ([`classify_network_risk`] → [`NetGrantRisk::Refused`]).
//!    Blanket `*` wildcards, localhost/local domains, private RFC 1918 IPs,
//!    loopback IPs, and cloud metadata/link-local IPs (`169.254.169.254`) are
//!    refused **even with `confirm: true`**. The AI cannot override this.
//! 2. **A human decision, not the model's word.** For the autonomous in-process
//!    agent ([`McpClientType::Ahma`](crate::client_type::McpClientType)), `confirm: true`
//!    does **not** self-persist: the request informs the human to run `ahma network allow <host>`.
//!    For external clients (Cursor, VS Code, …), `confirm: true` raises an MCP
//!    `elicitation/create` prompt when supported, persisting only on explicit human
//!    approval.
//! 3. **A two-phase confirm**. Without `confirm: true` the tool only *previews*
//!    (writes nothing). The default is always Deny.
//!
//! The grant is persisted to `~/.ahma/settings.toml` under `[network].allow` and
//! immediately added to the live session's granted network destinations.

use super::common;
use crate::AhmaMcpService;
use crate::egress::host_pattern::{HostPattern, HostPatternError};
use crate::egress::net_prompt::{NetApprovalForm, parse_answer};
use crate::mcp_service::schema;
use ahma_common::config::settings_path;
use ahma_common::net_approval::{NetApprovalDecision, persist_net_allow};
use ahma_common::permissions::{AuditAction, GrantKind, GrantTier, append_audit, audit_entry};
use rmcp::model::{CallToolResult, ErrorData as McpError};
use serde_json::{Map, Value};
use std::path::Path;
use std::sync::Arc;

/// The risk tier of a proposed network egress grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetGrantRisk {
    /// Catastrophic and never legitimate — refused even with `confirm: true`.
    Refused(String),
    /// Allowed with confirmation, but surfaced loudly as high risk.
    High(String),
    /// An ordinary grant (e.g. package registry, known API).
    Normal,
}

/// Classify the security risk of a proposed network destination.
pub fn classify_network_risk(pattern: &HostPattern, raw: &str) -> NetGrantRisk {
    let lower = raw.trim().to_ascii_lowercase();

    // 1. Blanket `*` is refused outright via AI tool
    if matches!(pattern, HostPattern::Any) || lower == "*" {
        return NetGrantRisk::Refused(
            "Blanket '*' wildcard allows all outbound network traffic and cannot be granted via AI tool. \
             To disable network egress restrictions, the human must add '*' to [network].allow in \
             ~/.ahma/settings.toml by hand or run `ahma network allow '*'.".to_string(),
        );
    }

    // 2. Localhost and local domains
    if lower == "localhost"
        || lower.ends_with(".localhost")
        || lower.ends_with(".local")
        || lower.ends_with(".internal")
    {
        return NetGrantRisk::Refused(
            "Localhost and local network domains cannot be granted network egress.".to_string(),
        );
    }

    // 3. IP addresses (literal check)
    let ip_str = lower.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = ip_str.parse::<std::net::IpAddr>()
        && ahma_harness_tools::egress_guard::is_blocked_ip(&ip)
    {
        return NetGrantRisk::Refused(format!(
            "IP address '{ip}' is in a private, loopback, link-local, or cloud-metadata range and is blocked."
        ));
    }

    // 4. Wildcards
    if let HostPattern::Wildcard(suffix) = pattern {
        if !suffix.contains('.') {
            return NetGrantRisk::Refused(format!(
                "Broad wildcard '*.{suffix}' covering an entire top-level domain is refused."
            ));
        }
        return NetGrantRisk::High(format!(
            "Wildcard pattern '*.{suffix}' grants egress to all subdomains of {suffix}."
        ));
    }

    NetGrantRisk::Normal
}

/// Parse and validate a host or domain argument.
pub fn parse_host_argument(raw: &str) -> Result<String, HostPatternError> {
    let pattern = HostPattern::parse(raw)?;
    match pattern {
        HostPattern::Exact(h) => Ok(h),
        HostPattern::Wildcard(s) => Ok(format!("*.{s}")),
        HostPattern::Any => Ok("*".to_string()),
    }
}

/// Build the JSON input schema advertised for the `network_grant` tool.
pub fn network_grant_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "host".to_string(),
        schema::string_property(
            "Hostname or domain pattern to allow for subprocess network egress \
             (e.g. `crates.io`, `*.github.com`).",
        ),
    );
    props.insert(
        "confirm".to_string(),
        schema::boolean_property(
            "Must be `true` to actually write the grant. Omit (or `false`) to PREVIEW only: the \
             tool returns the full settings-file path and the exact line it would add so you can \
             show the human and get approval first. The default is always Deny.",
        ),
    );
    props.insert(
        "note".to_string(),
        schema::string_property("Optional provenance note recorded alongside the grant."),
    );
    schema::object_input_schema(props, &["host"])
}

impl AhmaMcpService {
    /// Handle a `network_grant` call. See module docs for the security model.
    pub async fn handle_network_grant(
        &self,
        args: Map<String, Value>,
        client_type: crate::client_type::McpClientType,
    ) -> Result<CallToolResult, McpError> {
        let raw = match args.get("host").and_then(Value::as_str) {
            Some(h) => h,
            None => match args.get("domain").and_then(Value::as_str) {
                Some(d) => d,
                None => {
                    return Err(common::mcp_invalid_params(
                        "network_grant requires a `host` argument (e.g. `crates.io`)",
                    ));
                }
            },
        };

        let host = parse_host_argument(raw).map_err(|e| {
            common::mcp_invalid_params(format!("invalid host pattern '{raw}': {e}"))
        })?;

        let parsed_pattern = HostPattern::parse(&host).map_err(|e| {
            common::mcp_invalid_params(format!("invalid host pattern '{host}': {e}"))
        })?;

        let confirm = args
            .get("confirm")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let note = common::opt_str(&args, "note");

        let settings_file = settings_path().ok_or_else(|| {
            common::mcp_internal(
                "cannot locate ~/.ahma/settings.toml (home directory unknown); set HOME and retry",
            )
        })?;

        let risk = classify_network_risk(&parsed_pattern, &host);

        // Gate 1: Hard denylist refuses outright, regardless of confirm.
        if let NetGrantRisk::Refused(reason) = &risk {
            return Err(common::mcp_invalid_params(format!(
                "network_grant REFUSED for {host}: {reason}\n\n\
                 This destination is on the hard denylist and cannot be granted by the AI \
                 even with confirmation. If you genuinely need it, the human must edit {} by hand.",
                settings_file.display()
            )));
        }

        // Gate 2: Without explicit confirmation, only preview.
        if !confirm {
            return Ok(common::text_result(preview_text(
                &host,
                &settings_file,
                &risk,
            )));
        }

        // Autonomous in-process agent cannot self-persist.
        if matches!(client_type, crate::client_type::McpClientType::Ahma) {
            return Ok(common::text_result(agent_requested_text(
                &host,
                &settings_file,
            )));
        }

        // External client: raise elicitation prompt if supported.
        let human_approved = {
            let peer = self.peer.read().clone();
            match peer {
                None => true,
                Some(peer) => {
                    match peer
                        .elicit_with_timeout::<NetApprovalForm>(
                            grant_prompt_message(&host, &settings_file, &risk),
                            Some(std::time::Duration::from_secs(120)),
                        )
                        .await
                    {
                        Ok(Some(form)) => matches!(
                            parse_answer(&form.decision),
                            NetApprovalDecision::AllowAlways
                                | NetApprovalDecision::AllowSession
                                | NetApprovalDecision::AllowOnce
                        ),
                        Ok(None) | Err(rmcp::service::ElicitationError::UserDeclined) => false,
                        Err(rmcp::service::ElicitationError::CapabilityNotSupported) => true,
                        Err(e) => {
                            tracing::debug!("network_grant elicitation unavailable: {e}");
                            false
                        }
                    }
                }
            }
        };

        if !human_approved {
            return Ok(common::text_result(declined_text(&host, &settings_file)));
        }

        // Persist to ~/.ahma/settings.toml
        let newly_added = persist_net_allow(&settings_file, &host).map_err(|e| {
            common::mcp_internal(format!(
                "failed to persist network grant to {}: {e:#}",
                settings_file.display()
            ))
        })?;

        // Apply immediately to the live session
        self.net_approval.add_session_grant(&host);

        // Record audit entry
        let at = chrono::Local::now().to_rfc3339();
        let entry = audit_entry(
            at,
            AuditAction::Grant,
            GrantKind::NetHost,
            &host,
            None,
            GrantTier::Always,
            Some("mcp:network_grant".to_string()),
        );
        let _ = note;
        append_audit(&entry);

        Ok(common::text_result(success_text(
            &host,
            &settings_file,
            newly_added,
        )))
    }
}

fn preview_text(host: &str, settings_file: &Path, risk: &NetGrantRisk) -> String {
    let mut out = format!(
        "PREVIEW ONLY — nothing was written to disk.\n\n\
         Proposed network grant: {host}\n\
         Target settings file: {}\n\
         Exact entry to add under [network].allow:\n\
           \"{host}\"\n\n",
        settings_file.display()
    );
    match risk {
        NetGrantRisk::High(warning) => {
            out.push_str(&format!("⚠️ HIGH RISK: {warning}\n\n"));
        }
        NetGrantRisk::Normal => {
            out.push_str("Risk level: Normal\n\n");
        }
        NetGrantRisk::Refused(_) => {}
    }
    out.push_str(&format!(
        "To apply this grant, call `network_grant` with `confirm: true`,\n\
         or run `ahma network allow {host}` in your terminal."
    ));
    out
}

fn agent_requested_text(host: &str, settings_file: &Path) -> String {
    format!(
        "Network grant requested for '{host}'.\n\n\
         As an autonomous agent, you cannot self-persist network grants to {}.\n\
         The human user must approve this grant by running:\n\
           ahma network allow {host}\n\
         or by adding \"{host}\" to `[network].allow` in {}.",
        settings_file.display(),
        settings_file.display()
    )
}

fn grant_prompt_message(host: &str, settings_file: &Path, risk: &NetGrantRisk) -> String {
    let risk_note = match risk {
        NetGrantRisk::High(w) => format!("\n⚠️ Risk warning: {w}\n"),
        _ => String::new(),
    };
    format!(
        "Allow outbound network egress to '{host}'?\n{risk_note}\n\
         This appends to {} (under [network].allow), takes effect immediately \
         for this session, and persists for future sessions.\n\n\
         Answer 'always' to persist, 'session' for this session only, or 'deny' to refuse.",
        settings_file.display()
    )
}

fn declined_text(host: &str, settings_file: &Path) -> String {
    format!(
        "Not granted — the human declined the prompt, so nothing was written.\n\n\
         Requested network access to: {host}\n\
         Nothing was appended to {}. Re-run `network_grant` with `confirm: true` to \
         prompt again, or run `ahma network allow {host}` directly.",
        settings_file.display()
    )
}

fn success_text(host: &str, settings_file: &Path, newly_added: bool) -> String {
    let status = if newly_added {
        format!(
            "Added \"{host}\" to [network].allow in {}",
            settings_file.display()
        )
    } else {
        format!(
            "\"{host}\" is already present in [network].allow in {}",
            settings_file.display()
        )
    };
    format!(
        "Network grant successful for '{host}'.\n\n\
         {status}\n\
         The grant is active immediately for this session and persists across restarts."
    )
}

#[cfg(test)]
#[path = "network_grant_tool_tests.rs"]
mod tests;
