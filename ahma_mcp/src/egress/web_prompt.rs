//! The interactive web-approval prompt: the MCP `elicitation/create` form the
//! server sends a capable client when the `[web]` policy yields
//! [`ahma_common::web_policy::WebDecision::Prompt`], and the pure mapping from the
//! client's answer back to a [`WebApprovalDecision`].
//!
//! Wire I/O (the `elicit` round-trip, the coordinator resolve, persistence) lives
//! in the fetch handler; everything here is pure and unit-tested so the answer
//! mapping and prompt text are verified without a live client.

use ahma_common::web_approval::WebApprovalDecision;
use rmcp::elicit_safe;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The form the client renders for a web-egress approval. A single choice field;
/// rmcp auto-generates the JSON schema from this type. The value is mapped by
/// [`parse_answer`], which defaults anything unrecognized to a deny so a
/// misbehaving client can never *widen* access.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WebApprovalForm {
    /// One of: `once` (allow this request only), `session` (allow for the rest of
    /// this session), `always` (allow and save to settings), or `deny`.
    pub decision: String,
}

elicit_safe!(WebApprovalForm);

/// Map the client's raw choice to a [`WebApprovalDecision`]. Lenient on spelling
/// (case-insensitive, ignores surrounding whitespace, accepts a few synonyms) but
/// **fails safe**: anything unrecognized — including an empty string — is a
/// [`WebApprovalDecision::Deny`], never an allow.
pub fn parse_answer(raw: &str) -> WebApprovalDecision {
    match raw.trim().to_ascii_lowercase().as_str() {
        "once" | "allow_once" | "allow once" => WebApprovalDecision::AllowOnce,
        "session" | "allow_session" | "allow session" => WebApprovalDecision::AllowSession,
        "always" | "allow_always" | "allow always" | "persist" => WebApprovalDecision::AllowAlways,
        _ => WebApprovalDecision::Deny,
    }
}

/// The human-facing prompt message. States the domain and the exact URL so the
/// operator can judge the request, and spells out what each answer does and its
/// duration.
pub fn prompt_message(domain: &str, url: &str) -> String {
    format!(
        "Allow web access to '{domain}'?\n\n\
         A tool requested: {url}\n\n\
         Answer with one of:\n\
         • once — allow just this request\n\
         • session — allow '{domain}' until the server restarts\n\
         • always — allow '{domain}' and save it to [web].always_allow\n\
         • deny — refuse (the default)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_answer_maps_each_choice() {
        assert_eq!(parse_answer("once"), WebApprovalDecision::AllowOnce);
        assert_eq!(parse_answer("session"), WebApprovalDecision::AllowSession);
        assert_eq!(parse_answer("always"), WebApprovalDecision::AllowAlways);
        assert_eq!(parse_answer("deny"), WebApprovalDecision::Deny);
    }

    #[test]
    fn parse_answer_is_lenient_on_case_whitespace_and_synonyms() {
        assert_eq!(parse_answer("  ALWAYS "), WebApprovalDecision::AllowAlways);
        assert_eq!(
            parse_answer("Allow Session"),
            WebApprovalDecision::AllowSession
        );
        assert_eq!(parse_answer("persist"), WebApprovalDecision::AllowAlways);
    }

    #[test]
    fn parse_answer_fails_safe_to_deny() {
        // Unknown, empty, and garbage inputs must never widen access.
        for bad in ["", "yes", "sure", "allow", "grant everything", "🤷"] {
            assert_eq!(
                parse_answer(bad),
                WebApprovalDecision::Deny,
                "'{bad}' must map to Deny"
            );
        }
    }

    #[test]
    fn prompt_message_states_domain_and_url() {
        let m = prompt_message("api.github.com", "https://api.github.com/repos");
        assert!(m.contains("api.github.com"));
        assert!(m.contains("https://api.github.com/repos"));
        // The four options are all offered.
        for opt in ["once", "session", "always", "deny"] {
            assert!(m.contains(opt), "prompt must offer '{opt}'");
        }
    }
}
