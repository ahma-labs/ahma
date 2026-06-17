//! Terminal-hook fall-open consent ledger (SPEC R5.5.3).
//!
//! A terminal hook that can sandbox a command runs it normally. A hook that
//! *cannot* sandbox (the ahma binary is missing/stale/crashes/times out, or the
//! kernel sandbox is unavailable) must NOT silently run the command unsandboxed.
//! Instead:
//!
//!  - the **first** un-sandboxable invocation **fails closed** (the command is
//!    denied) and a one-time consent is requested out-of-band;
//!  - once the user explicitly consents, subsequent un-sandboxable invocations
//!    run unsandboxed **with a continuously visible banner** counting them;
//!  - consent is scoped to the process/session generation and is **never
//!    persisted** — a restart starts fresh (no silent re-downgrade).
//!
//! This type is the pure decision core; the hook entry point and the banner
//! surface drive it.

/// What the hook should do for one invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDecision {
    /// The command can be sandboxed — run it normally.
    RunSandboxed,
    /// The command cannot be sandboxed and consent has not been given — deny it
    /// and tell the user how to repair/approve (fail closed).
    DenyPendingConsent,
    /// The command cannot be sandboxed but the user consented — run it
    /// unsandboxed. The ledger has counted this run for the banner.
    RunUnsandboxed,
}

/// In-memory, non-persistent consent ledger for one process/session generation.
#[derive(Debug)]
pub struct HookConsentLedger {
    generation: u64,
    consented: bool,
    unsandboxed_count: u64,
}

impl Default for HookConsentLedger {
    fn default() -> Self {
        Self::new(0)
    }
}

impl HookConsentLedger {
    /// A fresh ledger for the given session generation. No consent yet.
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            consented: false,
            unsandboxed_count: 0,
        }
    }

    /// Decide what to do for one hook invocation. `can_sandbox` is whether ahma
    /// was able to establish enforcement for this command.
    pub fn evaluate(&mut self, can_sandbox: bool) -> HookDecision {
        if can_sandbox {
            return HookDecision::RunSandboxed;
        }
        if self.consented {
            self.unsandboxed_count += 1;
            HookDecision::RunUnsandboxed
        } else {
            HookDecision::DenyPendingConsent
        }
    }

    /// Record explicit user consent to run unsandboxed for this session. The
    /// command that triggered the prompt is NOT retroactively run; the next
    /// invocation is the first allowed one.
    pub fn grant_consent(&mut self) {
        self.consented = true;
    }

    /// Whether unsandboxed execution is currently permitted.
    pub fn is_consented(&self) -> bool {
        self.consented
    }

    /// How many commands have run unsandboxed under the active consent.
    pub fn unsandboxed_count(&self) -> u64 {
        self.unsandboxed_count
    }

    /// The session generation this ledger belongs to.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The persistent banner to display while consent is active (R5.5.3), or
    /// `None` when enforcement has not fallen open.
    pub fn banner(&self) -> Option<String> {
        if self.consented {
            Some(format!(
                "⚠️  HOOK UNSANDBOXED — {} command(s) ran without kernel enforcement \
                 (consent granted this session). Repair ahma to restore sandboxing.",
                self.unsandboxed_count
            ))
        } else {
            None
        }
    }

    /// Reset for a new session generation (e.g. process restart). Consent does
    /// NOT carry over — R5.5.3 forbids persisting it.
    pub fn reset(&mut self, generation: u64) {
        self.generation = generation;
        self.consented = false;
        self.unsandboxed_count = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandboxable_command_runs_normally() {
        let mut l = HookConsentLedger::new(1);
        assert_eq!(l.evaluate(true), HookDecision::RunSandboxed);
        assert!(l.banner().is_none());
    }

    #[test]
    fn first_unsandboxable_fails_closed() {
        let mut l = HookConsentLedger::new(1);
        assert_eq!(l.evaluate(false), HookDecision::DenyPendingConsent);
        // still no consent, repeated attempts keep failing closed
        assert_eq!(l.evaluate(false), HookDecision::DenyPendingConsent);
        assert_eq!(l.unsandboxed_count(), 0);
        assert!(l.banner().is_none());
    }

    #[test]
    fn after_consent_runs_unsandboxed_and_counts() {
        let mut l = HookConsentLedger::new(1);
        assert_eq!(l.evaluate(false), HookDecision::DenyPendingConsent);
        l.grant_consent();
        assert_eq!(l.evaluate(false), HookDecision::RunUnsandboxed);
        assert_eq!(l.evaluate(false), HookDecision::RunUnsandboxed);
        assert_eq!(l.unsandboxed_count(), 2);
        let banner = l.banner().unwrap();
        assert!(banner.contains("UNSANDBOXED"));
        assert!(banner.contains('2'));
    }

    #[test]
    fn consent_does_not_count_sandboxable_runs() {
        let mut l = HookConsentLedger::new(1);
        l.grant_consent();
        assert_eq!(l.evaluate(true), HookDecision::RunSandboxed);
        assert_eq!(l.unsandboxed_count(), 0);
    }

    #[test]
    fn reset_clears_consent_no_persistence() {
        let mut l = HookConsentLedger::new(1);
        l.grant_consent();
        l.evaluate(false);
        assert!(l.is_consented());
        l.reset(2);
        assert_eq!(l.generation(), 2);
        assert!(
            !l.is_consented(),
            "consent must NOT persist across sessions (R5.5.3)"
        );
        assert_eq!(l.unsandboxed_count(), 0);
        // first fall-open after reset fails closed again
        assert_eq!(l.evaluate(false), HookDecision::DenyPendingConsent);
    }
}
