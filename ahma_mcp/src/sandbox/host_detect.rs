//! Detection of an outer "host" sandbox ahma may be running inside (Cursor, VS
//! Code, Docker, or an unidentified one).
//!
//! This drives the **capability-based deferral** decision: when an ahma terminal
//! hook fires inside a host that already kernel-sandboxes the command, ahma's own
//! sandbox would only add the redundant-double-sandbox friction (and chase the
//! host's private build-cache env injection). Instead ahma defers to the host and
//! says so loudly. Detection is intentionally cheap (environment variables + a
//! couple of marker-file stats) so it is safe to call on the hook hot path; it
//! never spawns a subprocess.
//!
//! Honesty limit: detecting a host does **not** prove the host's sandbox is
//! *enabled* (a user may have set Cursor's `sandbox.json` to `insecure_none`).
//! Callers that defer on this signal must disclose that protection now depends on
//! the host, so a user who turned the host sandbox off is informed, not surprised.

use std::path::Path;

/// Marker ahma stamps on every command it runs inside its own kernel sandbox:
/// the pid of the ahma that applied the sandbox. It is how a nested ahma — the
/// test suite or an `ahma serve` started *through* `run_terminal_command` —
/// knows the outer sandbox it must defer to is ahma's, and can say so
/// (SPEC R7.1, R7.6) instead of reporting "an outer sandbox". A marker, not
/// configuration: ahma sets it, nothing reads it as a setting.
pub const OUTER_SANDBOX_PID_ENV: &str = "AHMA_OUTER_SANDBOX_PID";

/// An outer sandbox ahma may be running nested inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostSandbox {
    /// Another ahma's kernel sandbox — this process was spawned by an outer
    /// `run_terminal_command` (named via [`OUTER_SANDBOX_PID_ENV`]).
    Ahma,
    /// Cursor's agent sandbox (named via `CURSOR_SANDBOX` / `CURSOR_AGENT`).
    Cursor,
    /// Claude Code's Bash sandbox (named via `CLAUDECODE` / `CLAUDE_CODE_ENTRYPOINT`).
    ClaudeCode,
    /// Visual Studio Code's integrated terminal/agent.
    VsCode,
    /// A Docker/OCI container.
    Docker,
    /// An outer sandbox was detected, but we cannot identify which one.
    Unidentified,
}

impl HostSandbox {
    /// Human-readable label for log lines and user-facing disclosure.
    pub fn label(self) -> &'static str {
        match self {
            Self::Ahma => "an outer ahma",
            Self::Cursor => "Cursor",
            Self::ClaudeCode => "Claude Code",
            Self::VsCode => "VS Code",
            Self::Docker => "Docker",
            Self::Unidentified => "an outer sandbox",
        }
    }

    /// Actionable, host-specific guidance for resolving a double-sandbox: how to
    /// make ahma the single authoritative sandbox (so the access ahma grants —
    /// e.g. the macOS keychain — is not re-blocked by the outer sandbox), plus the
    /// universal "run ahma outside the host" alternative. Rendered as part of the
    /// loud disclosure so the user is never left with a diagnosis and no fix.
    pub fn remediation(self) -> &'static str {
        match self {
            Self::Ahma => {
                "This process was started by an outer ahma `run_terminal_command`, whose kernel \
                 sandbox already confines every write it makes — macOS refuses to nest a second \
                 Seatbelt profile inside it, so the inner ahma cannot enforce a tighter scope \
                 and defers to the outer one. That is expected when ahma's own test suite or a \
                 nested `ahma serve` runs through ahma. For ahma's own enforcement, start this \
                 process from a plain terminal instead."
            }
            Self::ClaudeCode => {
                "To make ahma the sole sandbox: run ahma as a configured MCP server \
                 (Claude Code does NOT sandbox MCP servers — only its Bash tool), rather than \
                 launching it from inside Claude Code's Bash tool; or disable Claude Code's \
                 Bash sandbox in its settings. Alternatively start ahma from a plain terminal \
                 outside Claude Code."
            }
            Self::Cursor => {
                "To make ahma the sole sandbox: set Cursor's sandbox to \"insecure_none\" in \
                 its sandbox.json (or enable the Legacy Terminal Tool), so only ahma sandboxes. \
                 For ahma terminal hooks specifically, set AHMA_PREFER_OWN_SANDBOX=1 to apply \
                 ahma's sandbox instead of deferring. Alternatively start ahma outside Cursor."
            }
            Self::VsCode => {
                "VS Code has no execution sandbox of its own to disable; run ahma as its MCP \
                 server (VS Code does not wrap the server's executions) rather than from inside \
                 a sandboxed extension terminal. Alternatively start ahma from a plain terminal."
            }
            Self::Docker => {
                "The container IS the outer boundary — running ahma inside a container is a \
                 supported, deliberate compose (container for isolation, ahma for scope/egress). \
                 If you did not intend the double layer, run ahma directly on the host instead."
            }
            Self::Unidentified => {
                "To make ahma the sole sandbox, start it from a plain terminal outside the outer \
                 sandbox, or disable that outer sandbox. For ahma terminal hooks, \
                 AHMA_PREFER_OWN_SANDBOX=1 forces ahma's own sandbox instead of deferring."
            }
        }
    }
}

/// The raw signals the classifier reads, abstracted so the decision logic stays
/// pure and unit-testable without touching the real process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostSignals {
    /// [`OUTER_SANDBOX_PID_ENV`] is set: an outer ahma's sandbox spawned us.
    /// Matched first — it is the most specific signal there is, and an outer
    /// ahma is itself usually running inside one of the IDEs below, whose
    /// markers it inherits and passes on.
    pub ahma: bool,
    /// `CURSOR_SANDBOX` or `CURSOR_AGENT` is set.
    pub cursor: bool,
    /// A Claude Code marker is present (`CLAUDECODE`, `CLAUDE_CODE_ENTRYPOINT`).
    /// Claude Code may run inside a VS Code terminal (setting `TERM_PROGRAM=vscode`
    /// too), so Claude Code is matched before VS Code.
    pub claude_code: bool,
    /// A VS Code marker is present (`VSCODE_PID`/`VSCODE_CWD`, or
    /// `TERM_PROGRAM=vscode`). Cursor is built on VS Code and may also set these,
    /// so Cursor is matched first.
    pub vscode: bool,
    /// A container marker is present (`/.dockerenv`, or `container` env set).
    pub docker: bool,
    /// A generic runtime probe concluded an outer sandbox is blocking ahma from
    /// applying its own (e.g. macOS `sandbox-exec` nesting denied). Used only as a
    /// last-resort, unnamed fallback.
    pub nested_probe: bool,
}

/// Classify host signals into a single [`HostSandbox`], most-specific first.
/// Pure: depends only on its input.
pub fn classify(signals: &HostSignals) -> Option<HostSandbox> {
    if signals.ahma {
        return Some(HostSandbox::Ahma);
    }
    if signals.cursor {
        return Some(HostSandbox::Cursor);
    }
    if signals.claude_code {
        return Some(HostSandbox::ClaudeCode);
    }
    if signals.vscode {
        return Some(HostSandbox::VsCode);
    }
    if signals.docker {
        return Some(HostSandbox::Docker);
    }
    if signals.nested_probe {
        return Some(HostSandbox::Unidentified);
    }
    None
}

/// Gather host signals from the current process environment (and a couple of
/// cheap marker-file stats). Does not run the subprocess nesting probe — callers
/// that want it can set [`HostSignals::nested_probe`] from
/// `super::prerequisites` and re-[`classify`].
pub fn signals_from_env() -> HostSignals {
    let env_set = |k: &str| std::env::var_os(k).is_some();

    let ahma = env_set(OUTER_SANDBOX_PID_ENV);

    let cursor = env_set("CURSOR_SANDBOX") || env_set("CURSOR_AGENT");

    let claude_code = env_set("CLAUDECODE") || env_set("CLAUDE_CODE_ENTRYPOINT");

    let vscode = env_set("VSCODE_PID")
        || env_set("VSCODE_CWD")
        || env_set("VSCODE_IPC_HOOK_CLI")
        || std::env::var("TERM_PROGRAM").is_ok_and(|v| v.eq_ignore_ascii_case("vscode"));

    let docker = env_set("container") || Path::new("/.dockerenv").exists();

    HostSignals {
        ahma,
        cursor,
        claude_code,
        vscode,
        docker,
        nested_probe: false,
    }
}

/// Detect whether ahma is running inside a host sandbox, naming it when possible.
///
/// Cheap (env vars + marker-file stats, no subprocess) so it is safe on the hook
/// hot path. Returns `None` when no outer sandbox is detected.
pub fn detect_host_sandbox() -> Option<HostSandbox> {
    classify(&signals_from_env())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_when_no_signals() {
        assert_eq!(classify(&HostSignals::default()), None);
    }

    #[test]
    fn cursor_is_named() {
        let s = HostSignals {
            cursor: true,
            ..Default::default()
        };
        assert_eq!(classify(&s), Some(HostSandbox::Cursor));
    }

    #[test]
    fn cursor_wins_over_vscode_markers() {
        // Cursor is built on VS Code and may set both; it must be named, not VS Code.
        let s = HostSignals {
            cursor: true,
            vscode: true,
            ..Default::default()
        };
        assert_eq!(classify(&s), Some(HostSandbox::Cursor));
    }

    #[test]
    fn vscode_when_only_vscode() {
        let s = HostSignals {
            vscode: true,
            ..Default::default()
        };
        assert_eq!(classify(&s), Some(HostSandbox::VsCode));
    }

    #[test]
    fn claude_code_is_named() {
        let s = HostSignals {
            claude_code: true,
            ..Default::default()
        };
        assert_eq!(classify(&s), Some(HostSandbox::ClaudeCode));
    }

    #[test]
    fn claude_code_wins_over_vscode_markers() {
        // Claude Code can run inside a VS Code terminal (setting vscode markers);
        // it must be named Claude Code, not VS Code.
        let s = HostSignals {
            claude_code: true,
            vscode: true,
            ..Default::default()
        };
        assert_eq!(classify(&s), Some(HostSandbox::ClaudeCode));
    }

    /// SPEC R7.1 / R7.6: an outer ahma is the most specific host there is —
    /// it usually runs inside one of the IDEs, inherits their markers, and
    /// passes them on to the command it sandboxes. The nested ahma must name
    /// ahma, not the IDE two levels up.
    #[test]
    fn ahma_wins_over_every_ide_marker() {
        let s = HostSignals {
            ahma: true,
            cursor: true,
            claude_code: true,
            vscode: true,
            docker: true,
            nested_probe: true,
        };
        assert_eq!(classify(&s), Some(HostSandbox::Ahma));
        assert_eq!(HostSandbox::Ahma.label(), "an outer ahma");
        assert!(
            HostSandbox::Ahma
                .remediation()
                .contains("run_terminal_command"),
            "the remediation must name the path that produced the nesting"
        );
    }

    #[test]
    fn cursor_wins_over_claude_code_markers() {
        let s = HostSignals {
            cursor: true,
            claude_code: true,
            ..Default::default()
        };
        assert_eq!(classify(&s), Some(HostSandbox::Cursor));
    }

    #[test]
    fn docker_when_only_docker() {
        let s = HostSignals {
            docker: true,
            ..Default::default()
        };
        assert_eq!(classify(&s), Some(HostSandbox::Docker));
    }

    #[test]
    fn unidentified_when_only_probe() {
        let s = HostSignals {
            nested_probe: true,
            ..Default::default()
        };
        assert_eq!(classify(&s), Some(HostSandbox::Unidentified));
    }

    #[test]
    fn labels_are_distinct_and_nonempty() {
        for h in [
            HostSandbox::Cursor,
            HostSandbox::ClaudeCode,
            HostSandbox::VsCode,
            HostSandbox::Docker,
            HostSandbox::Unidentified,
        ] {
            assert!(!h.label().is_empty());
        }
        assert_ne!(HostSandbox::Cursor.label(), HostSandbox::VsCode.label());
        assert_ne!(HostSandbox::ClaudeCode.label(), HostSandbox::Cursor.label());
    }

    #[test]
    fn remediation_is_actionable_for_every_host() {
        for h in [
            HostSandbox::Cursor,
            HostSandbox::ClaudeCode,
            HostSandbox::VsCode,
            HostSandbox::Docker,
            HostSandbox::Unidentified,
        ] {
            let r = h.remediation();
            assert!(!r.is_empty(), "{:?} remediation must be non-empty", h);
        }
        // The named IDEs must tell the user how to make ahma authoritative.
        assert!(
            HostSandbox::ClaudeCode.remediation().contains("MCP server"),
            "Claude Code remediation should point at the MCP-server path"
        );
        assert!(
            HostSandbox::Cursor.remediation().contains("insecure_none"),
            "Cursor remediation should name the sandbox.json escape hatch"
        );
    }
}
