//! The interactive network-egress approval prompt: the MCP `elicitation/create`
//! form the server sends a capable client when a sandboxed subprocess reaches
//! an unlisted domain through the guarded egress proxy (SPEC R-NET), and the
//! pure mapping from the client's answer back to a [`NetApprovalDecision`].
//!
//! Structurally identical to [`crate::egress::web_prompt`]; wire I/O (the
//! `elicit` round-trip, the coordinator resolve, persistence) lives in
//! `proxy.rs`, everything here is pure and unit-tested.

use ahma_common::net_approval::NetApprovalDecision;
use rmcp::elicit_safe;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The form the client renders for a network-egress approval. A single choice
/// field; rmcp auto-generates the JSON schema from this type. The value is
/// mapped by [`parse_answer`], which defaults anything unrecognized to a deny
/// so a misbehaving client can never *widen* access.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NetApprovalForm {
    /// One of: `once` (allow this connection only), `session` (allow for the
    /// rest of this session), `always` (allow and save to settings), or
    /// `deny`.
    pub decision: String,
}

elicit_safe!(NetApprovalForm);

/// Map the client's raw choice to a [`NetApprovalDecision`]. Lenient on
/// spelling (case-insensitive, ignores surrounding whitespace, accepts a few
/// synonyms) but **fails safe**: anything unrecognized — including an empty
/// string — is a [`NetApprovalDecision::Deny`], never an allow.
pub fn parse_answer(raw: &str) -> NetApprovalDecision {
    match raw.trim().to_ascii_lowercase().as_str() {
        "once" | "allow_once" | "allow once" => NetApprovalDecision::AllowOnce,
        "session" | "allow_session" | "allow session" => NetApprovalDecision::AllowSession,
        "always" | "allow_always" | "allow always" | "persist" => NetApprovalDecision::AllowAlways,
        _ => NetApprovalDecision::Deny,
    }
}

/// The human-facing prompt message. States the domain and the exact target
/// (`host:port`) so the operator can judge the request, and spells out what
/// each answer does and its duration.
pub fn prompt_message(domain: &str, target: &str) -> String {
    format!(
        "Allow subprocess network access to '{domain}'?\n\n\
         A sandboxed command tried to reach: {target}\n\n\
         Answer with one of:\n\
         • once — allow just this connection\n\
         • session — allow '{domain}' until the server restarts\n\
         • always — allow '{domain}' and save it to [network].allow\n\
         • deny — refuse (the default)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_answer_maps_each_choice() {
        assert_eq!(parse_answer("once"), NetApprovalDecision::AllowOnce);
        assert_eq!(parse_answer("session"), NetApprovalDecision::AllowSession);
        assert_eq!(parse_answer("always"), NetApprovalDecision::AllowAlways);
        assert_eq!(parse_answer("deny"), NetApprovalDecision::Deny);
    }

    #[test]
    fn parse_answer_is_lenient_on_case_whitespace_and_synonyms() {
        assert_eq!(parse_answer("  ALWAYS "), NetApprovalDecision::AllowAlways);
        assert_eq!(
            parse_answer("Allow Session"),
            NetApprovalDecision::AllowSession
        );
        assert_eq!(parse_answer("persist"), NetApprovalDecision::AllowAlways);
    }

    #[test]
    fn parse_answer_fails_safe_to_deny() {
        for bad in ["", "yes", "sure", "allow", "grant everything", "🤷"] {
            assert_eq!(
                parse_answer(bad),
                NetApprovalDecision::Deny,
                "'{bad}' must map to Deny"
            );
        }
    }

    #[test]
    fn prompt_message_states_domain_and_target() {
        let m = prompt_message("crates.io", "crates.io:443");
        assert!(m.contains("crates.io"));
        assert!(m.contains("crates.io:443"));
        for opt in ["once", "session", "always", "deny"] {
            assert!(m.contains(opt), "prompt must offer '{opt}'");
        }
    }
}
