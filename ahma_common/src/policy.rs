//! One decision point for userspace permissions (SPEC R-POLICY.1, R-POLICY.2).
//!
//! Every permission ahma decides in userspace (an egress host, a `fetch_webpage`
//! URL, and in time a link, an SSH signature, a grant, a tool approval) is
//! decided here, in one order, so the same question cannot be answered two
//! ways on two surfaces. Each domain keeps its own matching (host patterns,
//! path prefixes) and hands [`decide`] what it matched as [`Facts`]; the order
//! lives only in this file:
//!
//! 1. a **forbid**: the domain's built-in hard denial, or an installed
//!    [`PolicyOverlay`] (team policy, R-POLICY.3);
//! 2. this session's **answer from a human**;
//! 3. a **standing permit**: a saved grant, an allowlist entry;
//! 4. the domain's **default**: allow, ask, or deny.
//!
//! A forbid always wins: nothing permits what a forbid covers.
//!
//! The kernel boundary is not decided here. Cedar-style policy cannot be
//! evaluated per system call; kernel profiles are built from what this admits.

use std::path::Path;
use std::sync::{Arc, OnceLock};

/// What is asked for. The verbs ahma's permissions are about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Read,
    Write,
    /// A sandboxed command connects to a host through the egress proxy.
    Connect,
    /// ahma's own `fetch_webpage` reads a URL.
    Fetch,
    OpenLink,
    SshSign,
    CallTool,
}

impl Action {
    /// The action's name as policy files spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Read => "read",
            Action::Write => "write",
            Action::Connect => "connect",
            Action::Fetch => "fetch",
            Action::OpenLink => "open_link",
            Action::SshSign => "ssh_sign",
            Action::CallTool => "call_tool",
        }
    }
}

/// What the action is applied to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource<'a> {
    Path(&'a Path),
    /// A host name, lower-cased by the caller.
    Host(&'a str),
    Tool(&'a str),
    /// An `ssh-keygen -Y` signature namespace.
    Namespace(&'a str),
}

/// Who asks: the agent working in a workspace, when the caller knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Principal<'a> {
    pub workspace: Option<&'a Path>,
}

/// One permission question, shaped principal / action / resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request<'a> {
    pub principal: Principal<'a>,
    pub action: Action,
    pub resource: Resource<'a>,
}

impl<'a> Request<'a> {
    pub fn new(action: Action, resource: Resource<'a>) -> Self {
        Self {
            principal: Principal::default(),
            action,
            resource,
        }
    }

    pub fn in_workspace(mut self, workspace: &'a Path) -> Self {
        self.principal.workspace = Some(workspace);
        self
    }
}

/// This session's answer from a human, if there is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAnswer {
    Granted,
    Denied,
}

/// What a domain does when nothing above matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultAnswer {
    Allow,
    Ask,
    Deny,
}

/// What the domain's own matching found for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// A built-in hard denial, with the reason shown to whoever asked.
    pub forbidden: Option<String>,
    pub session: Option<SessionAnswer>,
    /// A standing permit, naming what matched (a pattern, a grant).
    pub permitted: Option<String>,
    pub default: DefaultAnswer,
}

impl Facts {
    pub fn new(default: DefaultAnswer) -> Self {
        Self {
            forbidden: None,
            session: None,
            permitted: None,
            default,
        }
    }
}

/// Which rule decided: for the audit log and `ahma policy explain` (R-POLICY.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Forbid,
    Overlay,
    Session,
    Permit,
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow {
        stage: Stage,
        matched: String,
    },
    Deny {
        stage: Stage,
        reason: String,
    },
    /// Nothing decided and the default is to ask a human.
    Ask,
}

/// Policy that can only narrow what ahma allows: a forbid evaluated before
/// everything else (R-POLICY.3). Team policy in Cedar implements this.
pub trait PolicyOverlay: Send + Sync {
    /// `Some(reason)` when this request is forbidden.
    fn forbids(&self, request: &Request<'_>) -> Option<String>;
}

static OVERLAY: OnceLock<Arc<dyn PolicyOverlay>> = OnceLock::new();

/// Install the overlay every later [`decide`] consults. Once per process, at
/// startup, like the rest of configuration (R-CFG4); a second call is refused.
pub fn install_overlay(overlay: Arc<dyn PolicyOverlay>) -> Result<(), Arc<dyn PolicyOverlay>> {
    OVERLAY.set(overlay)
}

fn overlay() -> Option<&'static dyn PolicyOverlay> {
    OVERLAY.get().map(|o| o.as_ref())
}

/// Decide `request` from `facts`, consulting the installed overlay.
pub fn decide(request: &Request<'_>, facts: &Facts) -> Decision {
    decide_with(request, facts, overlay())
}

/// [`decide`] with an explicit overlay, for callers and tests that hold one.
pub fn decide_with(
    request: &Request<'_>,
    facts: &Facts,
    overlay: Option<&dyn PolicyOverlay>,
) -> Decision {
    if let Some(reason) = &facts.forbidden {
        return Decision::Deny {
            stage: Stage::Forbid,
            reason: reason.clone(),
        };
    }
    if let Some(reason) = overlay.and_then(|o| o.forbids(request)) {
        return Decision::Deny {
            stage: Stage::Overlay,
            reason,
        };
    }
    match facts.session {
        Some(SessionAnswer::Granted) => {
            return Decision::Allow {
                stage: Stage::Session,
                matched: "session".into(),
            };
        }
        Some(SessionAnswer::Denied) => {
            return Decision::Deny {
                stage: Stage::Session,
                reason: "denied earlier this session".into(),
            };
        }
        None => {}
    }
    if let Some(matched) = &facts.permitted {
        return Decision::Allow {
            stage: Stage::Permit,
            matched: matched.clone(),
        };
    }
    match facts.default {
        DefaultAnswer::Allow => Decision::Allow {
            stage: Stage::Default,
            matched: "default-allow".into(),
        },
        DefaultAnswer::Ask => Decision::Ask,
        DefaultAnswer::Deny => Decision::Deny {
            stage: Stage::Default,
            reason: "not allowed by default".into(),
        },
    }
}

/// The machine-wide team-policy directory, for an administrator or MDM
/// (SPEC R-POLICY.3). Here rather than in `ahma_policy` so grant admission can
/// refuse it without depending on Cedar.
pub fn system_policy_dir() -> std::path::PathBuf {
    use std::path::PathBuf;
    if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/ahma/policy")
    } else if cfg!(windows) {
        std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
            .join("ahma")
            .join("policy")
    } else {
        PathBuf::from("/etc/ahma/policy")
    }
}

/// Why calling the tool `name` is forbidden, if it is: team policy may forbid
/// a tool, and a policy file that failed to load forbids every one. The MCP
/// server asks this for each `tools/call`, and terminal hooks for each hooked
/// command (as `run_terminal_command`), so both fail closed the same way.
pub fn forbidden_tool(name: &str) -> Option<String> {
    match decide(
        &Request::new(Action::CallTool, Resource::Tool(name)),
        &Facts::new(DefaultAnswer::Allow),
    ) {
        Decision::Deny { reason, .. } => Some(reason),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(h: &str) -> Request<'_> {
        Request::new(Action::Connect, Resource::Host(h))
    }

    fn all_set() -> Facts {
        Facts {
            forbidden: Some("hard".into()),
            session: Some(SessionAnswer::Granted),
            permitted: Some("pattern".into()),
            default: DefaultAnswer::Allow,
        }
    }

    struct ForbidHost(&'static str);
    impl PolicyOverlay for ForbidHost {
        fn forbids(&self, request: &Request<'_>) -> Option<String> {
            (request.resource == Resource::Host(self.0)).then(|| format!("team: {}", self.0))
        }
    }

    #[test]
    fn a_forbid_beats_everything() {
        let d = decide_with(&host("x"), &all_set(), None);
        assert_eq!(
            d,
            Decision::Deny {
                stage: Stage::Forbid,
                reason: "hard".into()
            }
        );
    }

    #[test]
    fn the_overlay_beats_a_session_grant_and_a_permit() {
        let facts = Facts {
            forbidden: None,
            ..all_set()
        };
        let d = decide_with(&host("x"), &facts, Some(&ForbidHost("x")));
        assert!(matches!(
            d,
            Decision::Deny {
                stage: Stage::Overlay,
                ..
            }
        ));
        // It narrows only what it names.
        let d = decide_with(&host("y"), &facts, Some(&ForbidHost("x")));
        assert!(matches!(
            d,
            Decision::Allow {
                stage: Stage::Session,
                ..
            }
        ));
    }

    #[test]
    fn a_session_answer_beats_a_standing_permit() {
        let facts = Facts {
            forbidden: None,
            session: Some(SessionAnswer::Denied),
            ..all_set()
        };
        assert!(matches!(
            decide_with(&host("x"), &facts, None),
            Decision::Deny {
                stage: Stage::Session,
                ..
            }
        ));
    }

    #[test]
    fn a_permit_beats_the_default() {
        let mut facts = Facts::new(DefaultAnswer::Deny);
        facts.permitted = Some("*.example".into());
        assert_eq!(
            decide_with(&host("a.example"), &facts, None),
            Decision::Allow {
                stage: Stage::Permit,
                matched: "*.example".into()
            }
        );
    }

    #[test]
    fn the_default_decides_last() {
        let r = host("x");
        assert!(matches!(
            decide_with(&r, &Facts::new(DefaultAnswer::Allow), None),
            Decision::Allow {
                stage: Stage::Default,
                ..
            }
        ));
        assert_eq!(
            decide_with(&r, &Facts::new(DefaultAnswer::Ask), None),
            Decision::Ask
        );
        assert!(matches!(
            decide_with(&r, &Facts::new(DefaultAnswer::Deny), None),
            Decision::Deny {
                stage: Stage::Default,
                ..
            }
        ));
    }
}
