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

/// An outer sandbox ahma may be running nested inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostSandbox {
    /// Cursor's agent sandbox (named via `CURSOR_SANDBOX` / `CURSOR_AGENT`).
    Cursor,
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
            Self::Cursor => "Cursor",
            Self::VsCode => "VS Code",
            Self::Docker => "Docker",
            Self::Unidentified => "an outer sandbox",
        }
    }
}

/// The raw signals the classifier reads, abstracted so the decision logic stays
/// pure and unit-testable without touching the real process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostSignals {
    /// `CURSOR_SANDBOX` or `CURSOR_AGENT` is set.
    pub cursor: bool,
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
    if signals.cursor {
        return Some(HostSandbox::Cursor);
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
/// [`super::prerequisites`] and re-[`classify`].
pub fn signals_from_env() -> HostSignals {
    let env_set = |k: &str| std::env::var_os(k).is_some();

    let cursor = env_set("CURSOR_SANDBOX") || env_set("CURSOR_AGENT");

    let vscode = env_set("VSCODE_PID")
        || env_set("VSCODE_CWD")
        || env_set("VSCODE_IPC_HOOK_CLI")
        || std::env::var("TERM_PROGRAM").is_ok_and(|v| v.eq_ignore_ascii_case("vscode"));

    let docker = env_set("container") || Path::new("/.dockerenv").exists();

    HostSignals {
        cursor,
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
            HostSandbox::VsCode,
            HostSandbox::Docker,
            HostSandbox::Unidentified,
        ] {
            assert!(!h.label().is_empty());
        }
        assert_ne!(HostSandbox::Cursor.label(), HostSandbox::VsCode.label());
    }
}
